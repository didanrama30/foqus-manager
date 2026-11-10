use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use thiserror::Error;

pub const PROFILE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub creation_time_100ns: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessSample {
    pub identity: ProcessIdentity,
    pub name: String,
    pub executable_path: Option<PathBuf>,
    pub parent_pid: Option<u32>,
    pub session_id: Option<u32>,
    pub cpu_percent: Option<f32>,
    pub working_set_bytes: Option<u64>,
    pub private_bytes: Option<u64>,
    pub capability: ProcessCapability,
    pub state: ProcessState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProcessCapability {
    #[default]
    Unknown,
    Candidate,
    Managed,
    Excluded,
    SystemProtected,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProcessState {
    #[default]
    Running,
    Exited,
    AccessDenied,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum GamePriority {
    #[default]
    AboveNormal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GameProfile {
    pub id: String,
    pub name: String,
    pub executable_path: PathBuf,
    pub game_priority: GamePriority,
    pub reduce_background_memory: bool,
    pub managed_executables: Vec<PathBuf>,
    pub excluded_executables: Vec<PathBuf>,
    pub restore_when_game_exits: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileStore {
    pub schema_version: u32,
    pub selected_profile: Option<String>,
    pub profiles: Vec<GameProfile>,
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self {
            schema_version: PROFILE_SCHEMA_VERSION,
            selected_profile: None,
            profiles: vec![],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ChangeOutcome {
    Applied,
    Excluded,
    AccessDenied,
    Unsupported,
    Exited,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppliedChange {
    pub identity: ProcessIdentity,
    pub original_cpu_priority: Option<u32>,
    pub applied_cpu_priority: Option<u32>,
    pub original_memory_priority: Option<u8>,
    pub applied_memory_priority: Option<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FocusState {
    Idle,
    Starting,
    Active { started_at_unix: u64 },
    Stopping,
    Error(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FocusSession {
    pub target: ProcessIdentity,
    pub profile_snapshot: GameProfile,
    pub state: FocusState,
    pub changes: Vec<AppliedChange>,
    pub errors: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("could not access profile directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("profile data is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported profile schema version {0}")]
    UnsupportedSchema(u32),
}

impl ProfileStore {
    pub fn load(path: &Path) -> Result<Self, ProfileError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let store: Self = serde_json::from_slice(&fs::read(path)?)?;
        if store.schema_version != PROFILE_SCHEMA_VERSION {
            return Err(ProfileError::UnsupportedSchema(store.schema_version));
        }
        Ok(store)
    }

    pub fn save_atomic(&self, path: &Path) -> Result<(), ProfileError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let temp_path = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        fs::write(&temp_path, bytes)?;
        fs::File::open(&temp_path)?.sync_all()?;
        replace_file(&temp_path, path)?;
        Ok(())
    }
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        core::PCWSTR,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both strings are NUL-terminated and remain alive for the call.
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(std::io::Error::other)
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

pub fn default_profile_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    base.join("FoqusManager").join("profiles.json")
}

pub fn normalize_executable(path: &Path) -> PathBuf {
    let absolute = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    PathBuf::from(absolute.to_string_lossy().to_lowercase())
}

pub fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}
