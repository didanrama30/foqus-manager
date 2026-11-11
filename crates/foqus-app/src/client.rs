use foqus_core::{
    ControllerRequest, ControllerResponse, GameProfile, ProcessIdentity, SessionStatus,
};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};

/// How long an explicit shutdown waits for the controller to finish restoring
/// before the child is terminated. The journal survives, so the next startup
/// can still recover an interrupted restore.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ControllerClient {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    failed: Option<String>,
}

impl ControllerClient {
    pub fn spawn() -> Self {
        match Self::try_spawn() {
            Ok(client) => client,
            Err(error) => Self {
                child: None,
                stdin: None,
                stdout: None,
                failed: Some(format!("could not start focus controller: {error}")),
            },
        }
    }

    fn try_spawn() -> Result<Self, String> {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let mut child = Command::new(exe)
            .arg("--controller")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| error.to_string())?;
        let stdin = child.stdin.take().ok_or("controller stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("controller stdout unavailable")?;
        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout: Some(BufReader::new(stdout)),
            failed: None,
        })
    }

    pub fn status(&mut self) -> Result<SessionStatus, String> {
        self.request(ControllerRequest::Status)
    }

    pub fn start(
        &mut self,
        target: ProcessIdentity,
        profile: GameProfile,
    ) -> Result<SessionStatus, String> {
        self.request(ControllerRequest::Start { target, profile })
    }

    pub fn stop(&mut self) -> Result<SessionStatus, String> {
        self.request(ControllerRequest::Stop)
    }

    pub fn retry_restore(&mut self) -> Result<SessionStatus, String> {
        self.request(ControllerRequest::RetryRestore)
    }

    pub fn apply_profile(&mut self, profile: GameProfile) -> Result<SessionStatus, String> {
        self.request(ControllerRequest::ApplyProfile { profile })
    }

    pub fn shutdown(&mut self) {
        if self.child.is_none() {
            return;
        }
        if self.failed.is_none() {
            let _ = self.write_request(ControllerRequest::Shutdown);
        }
        // Close the request pipe immediately. Even when writing the shutdown
        // request failed, the controller's EOF path runs the same restore, so
        // never leave stdin open while waiting for the child.
        let _ = self.stdin.take();
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        loop {
            match self.child.as_mut().map(Child::try_wait) {
                Some(Ok(Some(_))) | Some(Err(_)) => break,
                Some(Ok(None)) => {
                    if Instant::now() >= deadline {
                        if let Some(mut child) = self.child.take() {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                None => break,
            }
        }
        let _ = self.stdout.take();
        self.child = None;
    }

    fn write_request(&mut self, request: ControllerRequest) -> Result<(), String> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        let stdin = self.stdin.as_mut().ok_or("controller is not running")?;
        serde_json::to_writer(&mut *stdin, &request).map_err(|error| error.to_string())?;
        stdin.write_all(b"\n").map_err(|error| error.to_string())?;
        stdin.flush().map_err(|error| error.to_string())
    }

    fn request(&mut self, request: ControllerRequest) -> Result<SessionStatus, String> {
        self.write_request(request)?;
        let stdout = self.stdout.as_mut().ok_or("controller is not running")?;
        let mut line = String::new();
        stdout
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        if line.is_empty() {
            self.failed = Some("focus controller closed unexpectedly".into());
            return Err(self.failed.clone().unwrap_or_default());
        }
        match serde_json::from_str::<ControllerResponse>(&line) {
            Ok(ControllerResponse::Status { status }) => Ok(status),
            Ok(ControllerResponse::Error { message }) => Err(message),
            Err(error) => Err(format!("invalid controller reply: {error}")),
        }
    }
}

impl Drop for ControllerClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn shutdown_waits_for_child_exit_and_reaps_after_stdin_eof() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("cat >/dev/null; sleep 0.35")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut client = ControllerClient {
            child: Some(child),
            stdin: Some(stdin),
            stdout: Some(stdout),
            failed: Some("controller is already gone".into()),
        };
        let started = Instant::now();
        client.shutdown();
        assert!(client.child.is_none());
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "shutdown returned before the child exited (Ok(None) must keep polling)"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!alive.success(), "the exited child should have been reaped");
    }
}
