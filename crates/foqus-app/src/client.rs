use foqus_core::{
    ControllerRequest, ControllerResponse, GameProfile, ProcessIdentity, SessionStatus,
};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

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
        let _ = self.request(ControllerRequest::Shutdown);
        if let Some(child) = self.child.as_mut() {
            let _ = child.wait();
        }
    }

    fn request(&mut self, request: ControllerRequest) -> Result<SessionStatus, String> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        let stdin = self.stdin.as_mut().ok_or("controller is not running")?;
        serde_json::to_writer(&mut *stdin, &request).map_err(|error| error.to_string())?;
        stdin.write_all(b"\n").map_err(|error| error.to_string())?;
        stdin.flush().map_err(|error| error.to_string())?;
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
