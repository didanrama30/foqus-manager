use foqus_core::{ControllerRequest, ControllerResponse};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

#[test]
fn corrupt_journal_logs_to_stderr_without_shifting_stdio_protocol() {
    let root = std::env::temp_dir().join(format!(
        "foqus-controller-stdio-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let data_dir = root.join("FoqusManager");
    fs::create_dir_all(&data_dir).unwrap();
    fs::write(data_dir.join("focus-journal.json"), "{ not valid json").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_foqus-app"))
        .arg("--controller")
        .env("HOME", &root)
        .env("LOCALAPPDATA", &root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    for request in [ControllerRequest::Status, ControllerRequest::Shutdown] {
        serde_json::to_writer(&mut stdin, &request).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    let mut lines = BufReader::new(stdout).lines();
    let first = lines.next().unwrap().unwrap();
    let response: ControllerResponse = serde_json::from_str(&first)
        .expect("controller stdout must contain only protocol JSON, never log lines");
    assert!(matches!(
        response,
        ControllerResponse::Status { status }
            if matches!(status.state, foqus_core::FocusState::Idle)
    ));
    let second = lines.next().unwrap().unwrap();
    assert!(matches!(
        serde_json::from_str::<ControllerResponse>(&second).unwrap(),
        ControllerResponse::Status { .. }
    ));

    drop(stdin);
    assert!(child.wait().unwrap().success());
    let mut logs = String::new();
    std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut logs).unwrap();
    assert!(logs.contains("could not recover focus journal"));
    let _ = fs::remove_dir_all(root);
}
