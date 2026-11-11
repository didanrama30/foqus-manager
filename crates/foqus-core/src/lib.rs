use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use thiserror::Error;

pub const PROFILE_SCHEMA_VERSION: u32 = 1;
pub const JOURNAL_SCHEMA_VERSION: u32 = 1;
/// Win32 `ABOVE_NORMAL_PRIORITY_CLASS`.
pub const GAME_CPU_PRIORITY: u32 = 0x0000_8000;
/// Win32 `BELOW_NORMAL_PRIORITY_CLASS`.
pub const BACKGROUND_CPU_PRIORITY: u32 = 0x0000_4000;
/// Win32 `MEMORY_PRIORITY_LOW`.
pub const BACKGROUND_MEMORY_PRIORITY: u8 = 2;

pub mod controller;
pub mod journal;
pub mod protocol;

pub use protocol::{ControllerRequest, ControllerResponse};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub creation_time_100ns: u64,
}

impl Serialize for ProcessIdentity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ProcessIdentity", 2)?;
        state.serialize_field("pid", &self.pid)?;
        state.serialize_field("creation_time_100ns", &self.creation_time_100ns.to_string())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ProcessIdentity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            pid: u32,
            creation_time_100ns: String,
        }
        let wire = Wire::deserialize(deserializer)?;
        let creation_time_100ns = wire
            .creation_time_100ns
            .parse()
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            pid: wire.pid,
            creation_time_100ns,
        })
    }
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
    Conflict,
    RestoreFailed,
    Skipped,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessOutcome {
    pub identity: ProcessIdentity,
    pub name: String,
    pub outcome: ChangeOutcome,
    pub message: Option<String>,
    #[serde(default)]
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessPriorities {
    pub cpu: u32,
    pub memory: Option<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppliedChange {
    pub identity: ProcessIdentity,
    pub executable_path: Option<PathBuf>,
    pub original_cpu_priority: Option<u32>,
    pub applied_cpu_priority: Option<u32>,
    pub original_memory_priority: Option<u8>,
    pub applied_memory_priority: Option<u8>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ControlError {
    #[error("process is no longer running")]
    Exited,
    #[error("process identity changed (PID reused)")]
    IdentityChanged,
    #[error("access denied")]
    AccessDenied,
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("process control failed: {0}")]
    Backend(String),
}

pub trait ProcessControlBackend: Send + Sync {
    fn list_processes(&self) -> Vec<ProcessSample>;
    fn read_priorities(&self, identity: ProcessIdentity)
    -> Result<ProcessPriorities, ControlError>;
    fn set_cpu_priority(
        &self,
        identity: ProcessIdentity,
        priority: u32,
    ) -> Result<(), ControlError>;
    fn set_memory_priority(
        &self,
        identity: ProcessIdentity,
        priority: u8,
    ) -> Result<(), ControlError>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FocusState {
    Idle,
    Starting,
    Active { started_at_unix: u64 },
    Stopping,
    Recovery,
    Error { message: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FocusSession {
    pub target: ProcessIdentity,
    pub profile_snapshot: GameProfile,
    pub state: FocusState,
    pub changes: Vec<AppliedChange>,
    pub outcomes: Vec<ProcessOutcome>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionStatus {
    pub state: FocusState,
    pub target: Option<ProcessIdentity>,
    pub profile_name: Option<String>,
    pub outcomes: Vec<ProcessOutcome>,
    pub errors: Vec<String>,
    pub applied_changes: usize,
}

impl Default for SessionStatus {
    fn default() -> Self {
        Self {
            state: FocusState::Idle,
            target: None,
            profile_name: None,
            outcomes: vec![],
            errors: vec![],
            applied_changes: 0,
        }
    }
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
        fs::OpenOptions::new()
            .write(true)
            .open(&temp_path)?
            .sync_all()?;
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

impl ProcessIdentity {
    pub fn from_ui(pid: u32, creation_time_100ns: &str) -> Result<Self, std::num::ParseIntError> {
        Ok(Self {
            pid,
            creation_time_100ns: creation_time_100ns.parse()?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PathPolicy {
    Excluded,
    Managed,
    Unmanaged,
}

/// Excluded always wins over managed. Paths are compared after normalization.
pub fn classify_path(profile: &GameProfile, path: &Path) -> PathPolicy {
    let normalized = normalize_executable(path);
    if profile
        .excluded_executables
        .iter()
        .any(|excluded| normalize_executable(excluded) == normalized)
    {
        return PathPolicy::Excluded;
    }
    if profile
        .managed_executables
        .iter()
        .any(|managed| normalize_executable(managed) == normalized)
    {
        return PathPolicy::Managed;
    }
    PathPolicy::Unmanaged
}

pub fn default_journal_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    base.join("FoqusManager").join("focus-journal.json")
}

pub fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}
