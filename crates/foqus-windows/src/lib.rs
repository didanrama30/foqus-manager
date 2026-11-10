//! Platform adapter boundary. Real Win32 sampling and mutation are intentionally
//! introduced in later milestones; this backend never changes host processes.
use foqus_core::{GameProfile, ProcessSample};
use std::path::PathBuf;
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

#[derive(Default)]
pub struct DemoBackend;
impl ProcessBackend for DemoBackend {
    fn execute(&self, command: BackendCommand) -> BackendReply {
        match command {
            BackendCommand::ListProcesses => BackendReply::Processes(vec![]),
            _ => BackendReply::Unsupported("Process control is not available yet".into()),
        }
    }
    fn is_demo(&self) -> bool {
        true
    }
}

#[cfg(windows)]
mod win32 {
    use super::*;
    use foqus_core::{ProcessCapability, ProcessIdentity, ProcessState};
    use std::{collections::HashMap, mem::size_of, os::windows::ffi::OsStringExt, time::Instant};
    use windows::{
        Win32::{
            Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE},
            System::{
                Diagnostics::ToolHelp::{
                    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                    TH32CS_SNAPPROCESS,
                },
                ProcessStatus::{
                    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
                },
                SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
                Threading::{
                    GetProcessTimes, GetSystemTimes, OpenProcess,
                    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ, QueryFullProcessImageNameW,
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

    impl ProcessBackend for WindowsBackend {
        fn execute(&self, command: BackendCommand) -> BackendReply {
            match command {
                BackendCommand::ListProcesses => BackendReply::Processes(vec![]),
                _ => BackendReply::Unsupported(
                    "Focus control is not available until recovery support is implemented".into(),
                ),
            }
        }
        fn is_demo(&self) -> bool {
            false
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
            let windows_root = std::env::var_os("WINDIR").map(PathBuf::from);
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
                    let protected_path = path
                        .as_ref()
                        .zip(windows_root.as_ref())
                        .is_some_and(|(p, w)| p.starts_with(w));
                    let protected_name = matches!(
                        name.to_ascii_lowercase().as_str(),
                        "system"
                            | "registry"
                            | "smss.exe"
                            | "csrss.exe"
                            | "wininit.exe"
                            | "services.exe"
                            | "lsass.exe"
                    );
                    let capability = if protected_path || protected_name {
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
                        session_id: None,
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
