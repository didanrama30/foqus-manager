use foqus_core::{ControllerRequest, ControllerResponse, GameProfile, ProcessIdentity};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
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
    let journal = data_dir.join("focus-journal.json");
    fs::write(&journal, "{ not valid json").unwrap();
    let before = fs::read(&journal).unwrap();

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

    let start = ControllerRequest::Start {
        target: ProcessIdentity {
            pid: 1,
            creation_time_100ns: 1,
        },
        profile: GameProfile {
            id: "p".into(),
            name: "Game".into(),
            executable_path: PathBuf::from(r"C:\Games\game.exe"),
            game_priority: Default::default(),
            reduce_background_memory: false,
            managed_executables: vec![],
            excluded_executables: vec![],
            restore_when_game_exits: true,
        },
    };
    for request in [
        ControllerRequest::Status,
        start,
        ControllerRequest::Shutdown,
    ] {
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
        ControllerResponse::Status {
            status
        } if matches!(status.state, foqus_core::FocusState::Error { .. })
            && !status.errors.is_empty()
    ));
    let second = lines.next().unwrap().unwrap();
    assert!(matches!(
        serde_json::from_str::<ControllerResponse>(&second).unwrap(),
        ControllerResponse::Error { message } if message.contains("needs recovery")
    ));
    assert_eq!(
        fs::read(&journal).unwrap(),
        before,
        "start must not overwrite an unreadable journal"
    );
    let third = lines.next().unwrap().unwrap();
    assert!(matches!(
        serde_json::from_str::<ControllerResponse>(&third).unwrap(),
        ControllerResponse::Status { .. }
    ));

    drop(stdin);
    assert!(child.wait().unwrap().success());
    assert_eq!(
        fs::read(&journal).unwrap(),
        before,
        "shutdown must preserve an unreadable journal"
    );
    let mut logs = String::new();
    std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut logs).unwrap();
    assert!(logs.contains("could not recover focus journal"));
    let _ = fs::remove_dir_all(root);
}
