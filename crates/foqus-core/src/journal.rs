use crate::{FocusSession, JOURNAL_SCHEMA_VERSION, ProfileError};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FocusJournal {
    pub schema_version: u32,
    pub session: FocusSession,
}

impl FocusJournal {
    pub fn load(path: &Path) -> Result<Option<Self>, ProfileError> {
        if !path.exists() {
            return Ok(None);
        }
        let journal: Self = serde_json::from_slice(&fs::read(path)?)?;
        if journal.schema_version != JOURNAL_SCHEMA_VERSION {
            return Err(ProfileError::UnsupportedSchema(journal.schema_version));
        }
        Ok(Some(journal))
    }

    pub fn save_atomic(&self, path: &Path) -> Result<(), ProfileError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let temp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        fs::write(&temp, bytes)?;
        fs::OpenOptions::new().write(true).open(&temp)?.sync_all()?;
        replace_file(&temp, path)?;
        Ok(())
    }

    pub fn remove(path: &Path) -> Result<(), ProfileError> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ProfileError::Io(error)),
        }
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
