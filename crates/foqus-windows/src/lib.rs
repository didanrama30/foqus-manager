//! Platform adapter boundary: Win32 sampling/mutation on Windows, an explicit
//! in-memory demo backend elsewhere. The demo backend never touches host processes.
use foqus_core::{
    ControlError, GameProfile, ProcessControlBackend, ProcessIdentity, ProcessPriorities,
    ProcessSample,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};
#[cfg(windows)]
mod icons;
#[cfg(windows)]
pub use icons::executable_icon;

#[derive(Clone, Debug)]
pub enum BackendCommand {
    ListProcesses,
    StartFocus {
        target_pid: u32,
        creation_time_100ns: u64,
        profile: GameProfile,
    },
    StopFocus,
    UpdateProfile(GameProfile),
    SetProcessPolicy {
        pid: u32,
        creation_time_100ns: u64,
        managed: bool,
    },
    GetSessionStatus,
}

#[derive(Clone, Debug)]
pub enum BackendReply {
    Processes(Vec<ProcessSample>),
    Accepted,
    Unsupported(String),
}

pub trait ProcessBackend: Send + Sync {
    fn execute(&self, command: BackendCommand) -> BackendReply;
    fn is_demo(&self) -> bool;
}

pub struct DemoBackend {
    processes: Vec<ProcessSample>,
    priorities: Mutex<HashMap<ProcessIdentity, ProcessPriorities>>,
}

impl Default for DemoBackend {
    fn default() -> Self {
        let samples = [
            (1001, "Cyberpunk2077.exe", r"C:\Games\Cyberpunk2077.exe"),
            (1002, "Discord.exe", r"C:\Apps\Discord.exe"),
            (1003, "chrome.exe", r"C:\Apps\chrome.exe"),
        ];
        let processes = samples
            .iter()
            .map(|(pid, name, path)| ProcessSample {
                identity: ProcessIdentity {
                    pid: *pid,
                    creation_time_100ns: 1_300_000_000_000 + u64::from(*pid),
                },
                name: (*name).into(),
                executable_path: Some(PathBuf::from(path)),
                parent_pid: None,
                session_id: Some(1),
                cpu_percent: None,
                working_set_bytes: None,
                private_bytes: None,
                capability: foqus_core::ProcessCapability::Candidate,
                state: foqus_core::ProcessState::Running,
            })
            .collect::<Vec<_>>();
        let priorities = processes
            .iter()
            .map(|process| {
                (
                    process.identity,
                    ProcessPriorities {
                        cpu: 0x20,
                        memory: Some(5),
                    },
                )
            })
            .collect();
        Self {
            processes,
            priorities: Mutex::new(priorities),
        }
    }
}

impl DemoBackend {
    fn priorities(&self, identity: ProcessIdentity) -> Result<ProcessPriorities, ControlError> {
        self.priorities
            .lock()
            .map_err(|_| ControlError::Backend("demo backend lock poisoned".into()))?
            .get(&identity)
            .copied()
            .ok_or(ControlError::Exited)
    }
}

impl ProcessControlBackend for DemoBackend {
    fn list_processes(&self) -> Vec<ProcessSample> {
        self.processes.clone()
    }

    fn read_priorities(
        &self,
        identity: ProcessIdentity,
    ) -> Result<ProcessPriorities, ControlError> {
        self.priorities(identity)
    }

    fn set_cpu_priority(
        &self,
        identity: ProcessIdentity,
        priority: u32,
    ) -> Result<(), ControlError> {
        let mut priorities = self
            .priorities
            .lock()
            .map_err(|_| ControlError::Backend("demo backend lock poisoned".into()))?;
        let value = priorities.get_mut(&identity).ok_or(ControlError::Exited)?;
        value.cpu = priority;
        Ok(())
    }

    fn set_memory_priority(
        &self,
        identity: ProcessIdentity,
        priority: u8,
    ) -> Result<(), ControlError> {
        let mut priorities = self
            .priorities
            .lock()
            .map_err(|_| ControlError::Backend("demo backend lock poisoned".into()))?;
        let value = priorities.get_mut(&identity).ok_or(ControlError::Exited)?;
        value.memory = Some(priority);
        Ok(())
    }
}

impl ProcessBackend for DemoBackend {
    fn execute(&self, command: BackendCommand) -> BackendReply {
        match command {
            BackendCommand::ListProcesses => BackendReply::Processes(self.list_processes()),
            _ => BackendReply::Unsupported("Process control is not available yet".into()),
        }
    }
    fn is_demo(&self) -> bool {
        true
    }
}

#[cfg(not(windows))]
pub type PlatformBackend = DemoBackend;
#[cfg(windows)]
pub type PlatformBackend = WindowsBackend;

pub fn policy_name(path: &Path, profile: &GameProfile) -> foqus_core::PathPolicy {
    foqus_core::classify_path(profile, path)
}

#[cfg(windows)]
mod win32 {
    use super::*;
    use foqus_core::{ProcessCapability, ProcessIdentity, ProcessState};
    use std::{collections::HashMap, mem::size_of, os::windows::ffi::OsStringExt, time::Instant};
    use windows::{
        Win32::{
            Foundation::{
                CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, FILETIME, HANDLE,
                INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
            },
            Security::{EqualSid, GetTokenInformation, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser},
            System::{
                Diagnostics::ToolHelp::{
                    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                    TH32CS_SNAPPROCESS,
                },
                ProcessStatus::{
                    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
                },
                RemoteDesktop::ProcessIdToSessionId,
                SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
                Threading::{
                    GetCurrentProcess, GetPriorityClass, GetProcessId, GetProcessInformation,
                    GetProcessTimes, GetSystemTimes, MEMORY_PRIORITY, MEMORY_PRIORITY_INFORMATION,
                    OpenProcess, OpenProcessToken, PROCESS_CREATION_FLAGS,
                    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
                    PROCESS_SYNCHRONIZE, PROCESS_VM_READ, ProcessMemoryPriority,
                    QueryFullProcessImageNameW, SetPriorityClass, SetProcessInformation,
                    WaitForSingleObject,
                },
            },
        },
        core::PWSTR,
    };

    #[derive(Default)]
    pub struct WindowsBackend {
        previous_cpu: HashMap<ProcessIdentity, u64>,
        previous_system: Option<(u64, u64)>,
        previous_wall: Option<Instant>,
    }

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    fn filetime_value(value: FILETIME) -> u64 {
        (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
    }

    fn map_open_error(error: windows::core::Error) -> ControlError {
        if error.code() == ERROR_ACCESS_DENIED.to_hresult() {
            ControlError::AccessDenied
        } else if error.code() == ERROR_INVALID_PARAMETER.to_hresult() {
            ControlError::Exited
        } else {
            ControlError::Backend(error.message())
        }
    }

    fn ensure_running(handle: HANDLE) -> Result<(), ControlError> {
        let status = unsafe { WaitForSingleObject(handle, 0) };
        if status == WAIT_TIMEOUT {
            Ok(())
        } else if status == WAIT_OBJECT_0 {
            Err(ControlError::Exited)
        } else {
            Err(ControlError::Backend(
                windows::core::Error::from_thread().message(),
            ))
        }
    }

    fn identity_of(handle: HANDLE) -> Result<ProcessIdentity, ControlError> {
        ensure_running(handle)?;
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let pid = unsafe { GetProcessId(handle) };
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
            .map_err(|error| ControlError::Backend(error.message()))?;
        Ok(ProcessIdentity {
            pid,
            creation_time_100ns: filetime_value(created),
        })
    }

    fn open_query(identity: ProcessIdentity) -> Result<OwnedHandle, ControlError> {
        let handle = OwnedHandle(
            unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    false,
                    identity.pid,
                )
            }
            .map_err(map_open_error)?,
        );
        if identity_of(handle.0)? != identity {
            return Err(ControlError::IdentityChanged);
        }
        Ok(handle)
    }

    fn open_verified(identity: ProcessIdentity) -> Result<OwnedHandle, ControlError> {
        let handle = OwnedHandle(
            unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION
                        | PROCESS_SET_INFORMATION
                        | PROCESS_SYNCHRONIZE,
                    false,
                    identity.pid,
                )
            }
            .map_err(map_open_error)?,
        );
        if identity_of(handle.0)? != identity {
            return Err(ControlError::IdentityChanged);
        }
        Ok(handle)
    }

    fn memory_priority(handle: HANDLE) -> Option<u8> {
        let mut info = MEMORY_PRIORITY_INFORMATION::default();
        unsafe {
            GetProcessInformation(
                handle,
                ProcessMemoryPriority,
                (&mut info as *mut MEMORY_PRIORITY_INFORMATION).cast(),
                size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
            )
        }
        .ok()?;
        u8::try_from(info.MemoryPriority.0).ok()
    }

    fn current_user_sid() -> Option<Vec<u8>> {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
        let token = OwnedHandle(token);
        let mut length = 0u32;
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut length) };
        if length == 0 {
            return None;
        }
        let mut buffer = vec![0u8; length as usize];
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                length,
                &mut length,
            )
        }
        .ok()?;
        Some(buffer)
    }

    fn sid_of(buffer: &[u8]) -> PSID {
        // SAFETY: the buffer came from GetTokenInformation(TokenUser) and holds a TOKEN_USER.
        unsafe { (*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid }
    }

    fn same_user(handle: HANDLE, expected: PSID) -> bool {
        let mut token = HANDLE::default();
        if unsafe { OpenProcessToken(handle, TOKEN_QUERY, &mut token) }.is_err() {
            return false;
        }
        let token = OwnedHandle(token);
        let mut length = 0u32;
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut length) };
        if length == 0 {
            return false;
        }
        let mut buffer = vec![0u8; length as usize];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                length,
                &mut length,
            )
        }
        .is_err()
        {
            return false;
        }
        unsafe { EqualSid(sid_of(&buffer), expected) }.is_ok()
    }

    fn path_is_under(path: &Path, root: &Path) -> bool {
        path.to_string_lossy()
            .to_ascii_lowercase()
            .starts_with(&root.to_string_lossy().to_ascii_lowercase())
    }

    fn is_protected_name(name: &str) -> bool {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "system"
                | "registry"
                | "smss.exe"
                | "csrss.exe"
                | "wininit.exe"
                | "services.exe"
                | "lsass.exe"
                | "svchost.exe"
                | "winlogon.exe"
                | "audiodg.exe"
                | "rtkaudioservice.exe"
                | "nahimicsvc64.exe"
                | "wavesysservice.exe"
                | "syntpenh.exe"
                | "touchkeyboardservice.exe"
                | "tabtip.exe"
                | "fontdrvhost.exe"
                | "dwm.exe"
                | "explorer.exe"
                | "searchhost.exe"
                | "sihost.exe"
                | "ctfmon.exe"
                | "securityhealthservice.exe"
                | "msmpeng.exe"
                | "mpdefendercoreservice.exe"
                | "easyanticheat.exe"
                | "easyanticheat_eos.exe"
                | "beservice.exe"
                | "vgc.exe"
                | "riotclientservices.exe"
        )
    }

    fn process_path(handle: HANDLE) -> Option<PathBuf> {
        let mut buf = vec![0u16; 32768];
        let mut len = buf.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                handle,
                Default::default(),
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
        }
        .ok()?;
        Some(PathBuf::from(std::ffi::OsString::from_wide(
            &buf[..len as usize],
        )))
    }

    struct QueriedProcess {
        identity: ProcessIdentity,
        path: Option<PathBuf>,
        cpu_ticks: Option<u64>,
        working_set: Option<u64>,
        private_bytes: Option<u64>,
    }

    fn query_process(pid: u32) -> Option<QueriedProcess> {
        let handle = OwnedHandle(
            unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                    false,
                    pid,
                )
            }
            .ok()?,
        );
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        unsafe { GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) }
            .ok()?;
        let identity = ProcessIdentity {
            pid,
            creation_time_100ns: filetime_value(created),
        };
        let path = process_path(handle.0);
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        let memory_ok = unsafe {
            GetProcessMemoryInfo(
                handle.0,
                (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX)
                    .cast::<PROCESS_MEMORY_COUNTERS>(),
                counters.cb,
            )
        }
        .is_ok();
        let working_set = memory_ok.then_some(counters.WorkingSetSize as u64);
        let private_bytes = memory_ok.then_some(counters.PrivateUsage as u64);
        let cpu_ticks = filetime_value(kernel).checked_add(filetime_value(user));
        Some(QueriedProcess {
            identity,
            path,
            cpu_ticks,
            working_set,
            private_bytes,
        })
    }

    fn process_session(pid: u32) -> Option<u32> {
        let mut session = 0u32;
        unsafe { ProcessIdToSessionId(pid, &mut session) }
            .ok()
            .map(|_| session)
    }

    fn process_owner_ok(pid: u32, expected: PSID) -> bool {
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            return false;
        };
        let handle = OwnedHandle(handle);
        same_user(handle.0, expected)
    }

    impl ProcessControlBackend for WindowsBackend {
        fn list_processes(&self) -> Vec<ProcessSample> {
            let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
                Ok(handle) if handle != INVALID_HANDLE_VALUE => OwnedHandle(handle),
                _ => return vec![],
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let windows_root = std::env::var_os("WINDIR")
                .or_else(|| std::env::var_os("SystemRoot"))
                .map(PathBuf::from)
                .or_else(|| Some(PathBuf::from(r"C:\Windows")));
            let current_sid = current_user_sid();
            let current_sid = current_sid.as_deref().map(sid_of);
            let mut out = Vec::new();
            if unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_ok() {
                loop {
                    let end = entry
                        .szExeFile
                        .iter()
                        .position(|value| *value == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                    let pid = entry.th32ProcessID;
                    let parent_pid = entry.th32ParentProcessID;
                    let session_id = process_session(pid);
                    let same_owner = current_sid
                        .map(|sid| process_owner_ok(pid, sid))
                        .unwrap_or(false);
                    if let Some(process) = query_process(pid) {
                        let protected_path = process
                            .path
                            .as_ref()
                            .zip(windows_root.as_ref())
                            .is_some_and(|(path, root)| path_is_under(path, root));
                        let protected = protected_path
                            || is_protected_name(&name)
                            || session_id == Some(0)
                            || !same_owner
                            || !process.path.as_deref().is_some_and(|path| {
                                foqus_core::normalize_executable(path).is_absolute()
                            });
                        let capability = if protected {
                            ProcessCapability::SystemProtected
                        } else if process.path.is_some() {
                            ProcessCapability::Candidate
                        } else {
                            ProcessCapability::Unknown
                        };
                        out.push(ProcessSample {
                            identity: process.identity,
                            name,
                            executable_path: process.path,
                            parent_pid: Some(parent_pid),
                            session_id,
                            cpu_percent: None,
                            working_set_bytes: process.working_set,
                            private_bytes: process.private_bytes,
                            capability,
                            state: ProcessState::Running,
                        });
                    } else {
                        out.push(ProcessSample {
                            identity: ProcessIdentity {
                                pid,
                                creation_time_100ns: 0,
                            },
                            name,
                            executable_path: None,
                            parent_pid: Some(parent_pid),
                            session_id,
                            cpu_percent: None,
                            working_set_bytes: None,
                            private_bytes: None,
                            capability: ProcessCapability::Unknown,
                            state: ProcessState::AccessDenied,
                        });
                    }
                    if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
                        break;
                    }
                }
            }
            out
        }

        fn read_priorities(
            &self,
            identity: ProcessIdentity,
        ) -> Result<ProcessPriorities, ControlError> {
            let handle = open_query(identity)?;
            let cpu = unsafe { GetPriorityClass(handle.0) };
            if cpu == 0 {
                return Err(ControlError::Backend(
                    windows::core::Error::from_thread().message(),
                ));
            }
            Ok(ProcessPriorities {
                cpu,
                memory: memory_priority(handle.0),
            })
        }

        fn set_cpu_priority(
            &self,
            identity: ProcessIdentity,
            priority: u32,
        ) -> Result<(), ControlError> {
            let handle = open_verified(identity)?;
            unsafe { SetPriorityClass(handle.0, PROCESS_CREATION_FLAGS(priority)) }
                .map_err(|error| ControlError::Backend(error.message()))
        }

        fn set_memory_priority(
            &self,
            identity: ProcessIdentity,
            priority: u8,
        ) -> Result<(), ControlError> {
            let handle = open_verified(identity)?;
            let info = MEMORY_PRIORITY_INFORMATION {
                MemoryPriority: MEMORY_PRIORITY(u32::from(priority)),
            };
            unsafe {
                SetProcessInformation(
                    handle.0,
                    ProcessMemoryPriority,
                    (&info as *const MEMORY_PRIORITY_INFORMATION).cast(),
                    size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
                )
            }
            .map_err(|error| ControlError::Backend(error.message()))
        }
    }

    impl WindowsBackend {
        pub fn sample(&mut self) -> (Vec<ProcessSample>, Option<(u64, u64)>, Option<f32>) {
            let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
                Ok(h) if h != INVALID_HANDLE_VALUE => OwnedHandle(h),
                _ => return (vec![], None, None),
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut entries = Vec::new();
            if unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_ok() {
                loop {
                    let end = entry
                        .szExeFile
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                    entries.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
                    if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
                        break;
                    }
                }
            }
            let mut idle = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            let system_times =
                unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }
                    .ok()
                    .map(|_| {
                        let total = filetime_value(kernel).saturating_add(filetime_value(user));
                        (total.saturating_sub(filetime_value(idle)), total)
                    });
            let now = Instant::now();
            let wall_seconds = self
                .previous_wall
                .map(|old| now.duration_since(old).as_secs_f64())
                .unwrap_or(0.0);
            let prior_system = self.previous_system;
            let mut current_cpu = HashMap::new();
            let windows_root = std::env::var_os("WINDIR")
                .or_else(|| std::env::var_os("SystemRoot"))
                .map(PathBuf::from)
                .or_else(|| Some(PathBuf::from(r"C:\Windows")));
            let current_sid = current_user_sid();
            let current_sid = current_sid.as_deref().map(sid_of);
            let mut out = Vec::with_capacity(entries.len());
            for (pid, parent_pid, name) in entries {
                if let Some(process) = query_process(pid) {
                    let QueriedProcess {
                        identity,
                        path,
                        cpu_ticks: ticks,
                        working_set,
                        private_bytes,
                    } = process;
                    let cpu_ticks = ticks.unwrap_or_default();
                    let cpu_percent =
                        match (self.previous_cpu.get(&identity), prior_system, system_times) {
                            (Some(previous), Some((_, old_total)), Some((_, total)))
                                if wall_seconds > 0.0 && total > old_total =>
                            {
                                Some(
                                    ((cpu_ticks.saturating_sub(*previous)) as f64
                                        / (total - old_total) as f64
                                        * 100.0)
                                        .clamp(0.0, 100.0)
                                        as f32,
                                )
                            }
                            _ => None,
                        };
                    current_cpu.insert(identity, cpu_ticks);
                    let session_id = process_session(pid);
                    let same_owner = current_sid
                        .map(|sid| process_owner_ok(pid, sid))
                        .unwrap_or(false);
                    let protected_path = path
                        .as_ref()
                        .zip(windows_root.as_ref())
                        .is_some_and(|(p, w)| path_is_under(p, w));
                    let protected = protected_path
                        || is_protected_name(&name)
                        || session_id == Some(0)
                        || !same_owner;
                    let capability = if protected {
                        ProcessCapability::SystemProtected
                    } else if path.is_some() {
                        ProcessCapability::Candidate
                    } else {
                        ProcessCapability::Unknown
                    };
                    out.push(ProcessSample {
                        identity,
                        name,
                        executable_path: path,
                        parent_pid: Some(parent_pid),
                        session_id,
                        cpu_percent,
                        working_set_bytes: working_set,
                        private_bytes,
                        capability,
                        state: ProcessState::Running,
                    });
                }
            }
            self.previous_cpu = current_cpu;
            self.previous_system = system_times;
            self.previous_wall = Some(now);
            let memory = unsafe {
                let mut m = MEMORYSTATUSEX {
                    dwLength: size_of::<MEMORYSTATUSEX>() as u32,
                    ..Default::default()
                };
                GlobalMemoryStatusEx(&mut m)
                    .ok()
                    .map(|_| (m.ullTotalPhys - m.ullAvailPhys, m.ullTotalPhys))
            };
            let cpu = match (prior_system, system_times) {
                (Some((old_busy, old_total)), Some((busy, total))) if total > old_total => Some(
                    ((busy - old_busy) as f64 / (total - old_total) as f64 * 100.0)
                        .clamp(0.0, 100.0) as f32,
                ),
                _ => None,
            };
            (out, memory, cpu)
        }
    }
}

#[cfg(windows)]
pub use win32::WindowsBackend;
