use crate::{
    GameProfile, ProcessControlBackend, ProcessIdentity, SessionStatus, controller::FocusController,
};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, Write},
    sync::Mutex,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ControllerRequest {
    Status,
    Start {
        target: ProcessIdentity,
        profile: GameProfile,
    },
    Stop,
    RetryRestore,
    ApplyProfile {
        profile: GameProfile,
    },
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControllerResponse {
    Status { status: SessionStatus },
    Error { message: String },
}

/// Newline-delimited JSON controller loop. EOF restores the session and exits.
pub fn serve<B: ProcessControlBackend, R: BufRead, W: Write>(
    controller: &Mutex<FocusController<B>>,
    mut input: R,
    mut output: W,
) -> Result<(), String> {
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) => {
                controller
                    .lock()
                    .map_err(|_| "controller lock poisoned".to_string())?
                    .stop();
                return Ok(());
            }
            Ok(_) => {}
            Err(error) => {
                if let Ok(mut controller) = controller.lock() {
                    let _ = controller.stop();
                }
                return Err(error.to_string());
            }
        }
        let request: ControllerRequest = match serde_json::from_str(line.trim()) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut output,
                    &ControllerResponse::Error {
                        message: format!("invalid controller request: {error}"),
                    },
                )?;
                continue;
            }
        };
        let mut controller = controller
            .lock()
            .map_err(|_| "controller lock poisoned".to_string())?;
        let response = match request {
            ControllerRequest::Status => ControllerResponse::Status {
                status: controller.tick(),
            },
            ControllerRequest::Start { target, profile } => {
                match controller.start(target, profile) {
                    Ok(status) => ControllerResponse::Status { status },
                    Err(error) => ControllerResponse::Error {
                        message: error.to_string(),
                    },
                }
            }
            ControllerRequest::Stop => ControllerResponse::Status {
                status: controller.stop(),
            },
            ControllerRequest::RetryRestore => ControllerResponse::Status {
                status: controller.retry_restore(),
            },
            ControllerRequest::ApplyProfile { profile } => {
                match controller.update_profile(profile) {
                    Ok(status) => ControllerResponse::Status { status },
                    Err(error) => ControllerResponse::Error {
                        message: error.to_string(),
                    },
                }
            }
            ControllerRequest::Shutdown => {
                let status = controller.stop();
                write_response(&mut output, &ControllerResponse::Status { status })?;
                return Ok(());
            }
        };
        drop(controller);
        write_response(&mut output, &response)?;
    }
}

fn write_response<W: Write>(output: &mut W, response: &ControllerResponse) -> Result<(), String> {
    serde_json::to_writer(&mut *output, response).map_err(|error| error.to_string())?;
    output.write_all(b"\n").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ChangeOutcome, ControlError, FocusState, ProcessCapability, ProcessPriorities,
        ProcessSample, ProcessState,
    };
    use std::{
        collections::HashMap,
        io::Cursor,
        path::PathBuf,
        sync::{
            Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    struct Mock {
        values: Mutex<HashMap<ProcessIdentity, ProcessPriorities>>,
        processes: Vec<ProcessSample>,
    }

    impl ProcessControlBackend for Mock {
        fn list_processes(&self) -> Vec<ProcessSample> {
            self.processes.clone()
        }
        fn read_priorities(
            &self,
            identity: ProcessIdentity,
        ) -> Result<ProcessPriorities, ControlError> {
            self.values
                .lock()
                .unwrap()
                .get(&identity)
                .copied()
                .ok_or(ControlError::Exited)
        }
        fn set_cpu_priority(
            &self,
            identity: ProcessIdentity,
            priority: u32,
        ) -> Result<(), ControlError> {
            self.values
                .lock()
                .unwrap()
                .get_mut(&identity)
                .ok_or(ControlError::Exited)?
                .cpu = priority;
            Ok(())
        }
        fn set_memory_priority(
            &self,
            identity: ProcessIdentity,
            priority: u8,
        ) -> Result<(), ControlError> {
            self.values
                .lock()
                .unwrap()
                .get_mut(&identity)
                .ok_or(ControlError::Exited)?
                .memory = Some(priority);
            Ok(())
        }
    }

    fn identity(pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            creation_time_100ns: 1_000 + u64::from(pid),
        }
    }

    fn sample(pid: u32, path: &str) -> ProcessSample {
        ProcessSample {
            identity: identity(pid),
            name: path.rsplit('\\').next().unwrap_or(path).into(),
            executable_path: Some(PathBuf::from(path)),
            parent_pid: None,
            session_id: Some(1),
            cpu_percent: None,
            working_set_bytes: None,
            private_bytes: None,
            capability: ProcessCapability::Candidate,
            state: ProcessState::Running,
        }
    }

    #[test]
    fn protocol_round_trip_starts_and_restores() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "foqus-protocol-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let backend = Mock {
            values: Mutex::new(HashMap::from([(
                identity(1),
                ProcessPriorities {
                    cpu: 0x20,
                    memory: Some(5),
                },
            )])),
            processes: vec![sample(1, r"C:\Games\game.exe")],
        };
        let profile = GameProfile {
            id: "p".into(),
            name: "Game".into(),
            executable_path: PathBuf::from(r"C:\Games\game.exe"),
            game_priority: Default::default(),
            reduce_background_memory: false,
            managed_executables: vec![],
            excluded_executables: vec![],
            restore_when_game_exits: true,
        };
        let requests = [
            ControllerRequest::Start {
                target: identity(1),
                profile,
            },
            ControllerRequest::Status,
            ControllerRequest::Shutdown,
        ];
        let mut input = String::new();
        for request in requests {
            input.push_str(&serde_json::to_string(&request).unwrap());
            input.push('\n');
        }
        let mut output = Vec::new();
        let controller = FocusController::new(backend, path.clone());
        let shared = Mutex::new(controller);
        serve(&shared, Cursor::new(input), &mut output).unwrap();
        let lines: Vec<ControllerResponse> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert!(matches!(
            lines[0],
            ControllerResponse::Status {
                status: SessionStatus {
                    state: FocusState::Active { .. },
                    ..
                }
            }
        ));
        assert!(matches!(
            shared.lock().unwrap().status().state,
            FocusState::Idle
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn protocol_reports_invalid_requests_without_stopping() {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "foqus-protocol-bad-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let backend = Mock {
            values: Mutex::new(HashMap::new()),
            processes: vec![],
        };
        let controller = FocusController::new(backend, path.clone());
        let shared = Mutex::new(controller);
        let mut output = Vec::new();
        serve(
            &shared,
            Cursor::new("not json\n{\"command\":\"status\"}\n"),
            &mut output,
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("invalid controller request"));
        assert!(text.contains("\"kind\":\"status\""));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn outcome_is_serializable_for_ui() {
        let json = serde_json::to_string(&ChangeOutcome::Conflict).unwrap();
        assert_eq!(json, "\"Conflict\"");
    }
}
