#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
use base64::Engine;
mod client;

use client::ControllerClient;
use foqus_core::ProcessControlBackend;
use foqus_core::controller::FocusController;
use foqus_core::{
    GameProfile, ProcessCapability, ProcessIdentity, ProcessSample, ProcessState, ProfileStore,
    SessionStatus, default_journal_path, default_profile_path, normalize_executable,
    protocol::serve,
};
use foqus_windows::PlatformBackend;
use serde::Serialize;
use std::time::Duration;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone, Serialize)]
struct UiIdentity {
    pid: u32,
    // FILETIME exceeds JavaScript's safe integer range. Preserve it losslessly.
    creation_time_100ns: String,
}

#[derive(Clone, Serialize)]
struct UiProcess {
    identity: UiIdentity,
    name: String,
    executable_path: Option<PathBuf>,
    cpu_percent: Option<f32>,
    working_set_bytes: Option<u64>,
    private_bytes: Option<u64>,
    capability: ProcessCapability,
    state: ProcessState,
}

impl From<ProcessSample> for UiProcess {
    fn from(p: ProcessSample) -> Self {
        Self {
            identity: UiIdentity {
                pid: p.identity.pid,
                creation_time_100ns: p.identity.creation_time_100ns.to_string(),
            },
            name: p.name,
            executable_path: p.executable_path,
            cpu_percent: p.cpu_percent,
            working_set_bytes: p.working_set_bytes,
            private_bytes: p.private_bytes,
            capability: p.capability,
            state: p.state,
        }
    }
}

#[derive(Clone, Default, Serialize)]
struct Snapshot {
    processes: Vec<UiProcess>,
    memory: Option<(u64, u64)>,
    cpu: Option<f32>,
    demo: bool,
}

struct AppState {
    snapshot: Arc<Mutex<Snapshot>>,
    profile_lock: Mutex<()>,
    controller: Mutex<Option<ControllerClient>>,
}

#[tauri::command]
fn get_snapshot(state: tauri::State<'_, AppState>) -> Result<Snapshot, String> {
    state
        .snapshot
        .lock()
        .map(|s| s.clone())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn load_profiles(state: tauri::State<'_, AppState>) -> Result<ProfileStore, String> {
    let _lock = state.profile_lock.lock().map_err(|e| e.to_string())?;
    ProfileStore::load(&default_profile_path()).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_profiles(store: ProfileStore, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let _lock = state.profile_lock.lock().map_err(|e| e.to_string())?;
    ProfileStore::load(&default_profile_path()).map_err(|e| e.to_string())?;
    if store.schema_version != foqus_core::PROFILE_SCHEMA_VERSION
        || store
            .profiles
            .iter()
            .any(|p| !p.executable_path.is_absolute() || p.name.trim().is_empty())
    {
        return Err(
            "Profiles require a name, absolute executable path, and supported schema".into(),
        );
    }
    store
        .save_atomic(&default_profile_path())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_focus_status(state: tauri::State<'_, AppState>) -> Result<SessionStatus, String> {
    let mut guard = state.controller.lock().map_err(|e| e.to_string())?;
    let controller = guard.as_mut().ok_or("focus controller is not ready")?;
    controller.status()
}

#[tauri::command]
fn start_focus(
    pid: u32,
    creation_time_100ns: String,
    profile: GameProfile,
    state: tauri::State<'_, AppState>,
) -> Result<SessionStatus, String> {
    let identity =
        ProcessIdentity::from_ui(pid, &creation_time_100ns).map_err(|e| e.to_string())?;
    let path_matches = state
        .snapshot
        .lock()
        .map_err(|e| e.to_string())?
        .processes
        .iter()
        .find(|process| {
            process.identity.pid == pid
                && process.identity.creation_time_100ns == creation_time_100ns
        })
        .and_then(|process| process.executable_path.as_ref())
        .is_some_and(|path| {
            normalize_executable(path) == normalize_executable(&profile.executable_path)
        });
    if !path_matches {
        return Err("the selected game no longer matches this profile".into());
    }
    let mut guard = state.controller.lock().map_err(|e| e.to_string())?;
    let controller = guard.as_mut().ok_or("focus controller is not ready")?;
    controller.start(identity, profile)
}

#[tauri::command]
fn stop_focus(state: tauri::State<'_, AppState>) -> Result<SessionStatus, String> {
    let mut guard = state.controller.lock().map_err(|e| e.to_string())?;
    let controller = guard.as_mut().ok_or("focus controller is not ready")?;
    controller.stop()
}

#[tauri::command]
fn retry_restore(state: tauri::State<'_, AppState>) -> Result<SessionStatus, String> {
    let mut guard = state.controller.lock().map_err(|e| e.to_string())?;
    let controller = guard.as_mut().ok_or("focus controller is not ready")?;
    controller.retry_restore()
}

#[tauri::command]
fn apply_profile(
    profile: GameProfile,
    state: tauri::State<'_, AppState>,
) -> Result<SessionStatus, String> {
    let mut guard = state.controller.lock().map_err(|e| e.to_string())?;
    let controller = guard.as_mut().ok_or("focus controller is not ready")?;
    controller.apply_profile(profile)
}

#[tauri::command]
async fn choose_executable() -> Result<Option<GameProfile>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let path = rfd::FileDialog::new()
            .add_filter("Windows executable", &["exe"])
            .pick_file()?;
        let executable_path = normalize_executable(&path);
        Some(GameProfile {
            id: format!(
                "profile-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            name: path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            executable_path,
            game_priority: Default::default(),
            reduce_background_memory: false,
            managed_executables: vec![],
            excluded_executables: vec![],
            restore_when_game_exits: true,
        })
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_icon(
    path: PathBuf,
    state: tauri::State<'_, AppState>,
) -> Result<Option<String>, String> {
    let allowed = state
        .snapshot
        .lock()
        .map_err(|e| e.to_string())?
        .processes
        .iter()
        .any(|p| p.executable_path.as_ref() == Some(&path));
    if !allowed {
        return Ok(None);
    }
    tauri::async_runtime::spawn_blocking(move || icon_data(&path))
        .await
        .map_err(|e| e.to_string())
}

#[cfg(windows)]
fn icon_data(path: &std::path::Path) -> Option<String> {
    let pixels = foqus_windows::executable_icon(path)?;
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 32, 32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .ok()?
            .write_image_data(&pixels)
            .ok()?;
    }
    Some(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

#[cfg(not(windows))]
fn icon_data(_path: &std::path::Path) -> Option<String> {
    None
}

fn run_controller() -> ! {
    // stdout is the JSON request/reply channel. Logs must never write there.
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .init();
    let mut controller = FocusController::new(PlatformBackend::default(), default_journal_path());
    if let Err(error) = controller.recover_if_needed() {
        tracing::warn!("could not recover focus journal: {error}");
    }
    let controller = Arc::new(Mutex::new(controller));
    let ticker = controller.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(2));
            if let Ok(mut controller) = ticker.lock() {
                controller.tick();
            }
        }
    });
    let result = serve(
        &controller,
        std::io::stdin().lock(),
        std::io::stdout().lock(),
    );
    if let Err(error) = result {
        tracing::error!("focus controller stopped: {error}");
    }
    std::process::exit(0);
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--controller") {
        run_controller();
    }
    tracing_subscriber::fmt().with_env_filter("info").init();
    let snapshot = Arc::new(Mutex::new(Snapshot {
        demo: !cfg!(windows),
        ..Default::default()
    }));
    #[cfg(windows)]
    {
        let shared = snapshot.clone();
        std::thread::spawn(move || {
            let mut backend = foqus_windows::WindowsBackend::default();
            loop {
                let started = std::time::Instant::now();
                let (processes, memory, cpu) = backend.sample();
                if let Ok(mut value) = shared.lock() {
                    *value = Snapshot {
                        processes: processes.into_iter().map(UiProcess::from).collect(),
                        memory,
                        cpu,
                        demo: false,
                    };
                }
                std::thread::sleep(Duration::from_secs(1).saturating_sub(started.elapsed()));
            }
        });
    }
    #[cfg(not(windows))]
    {
        let backend = foqus_windows::DemoBackend::default();
        let demo_processes = backend
            .list_processes()
            .into_iter()
            .map(|process| {
                let mut ui = UiProcess::from(process);
                ui.cpu_percent = Some(4.0);
                ui.working_set_bytes = Some(512 * 1024 * 1024);
                ui
            })
            .collect();
        if let Ok(mut value) = snapshot.lock() {
            value.processes = demo_processes;
            value.cpu = Some(9.0);
            value.memory = Some((8 * 1024 * 1024 * 1024, 32 * 1024 * 1024 * 1024));
        }
    }
    let app_state = AppState {
        snapshot,
        profile_lock: Mutex::new(()),
        controller: Mutex::new(None),
    };
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            use tauri::Manager;
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            }
        }))
        .manage(app_state)
        .setup(|app| {
            use tauri::Manager;
            let state = app.state::<AppState>();
            *state
                .controller
                .lock()
                .map_err(|_| "controller lock poisoned")? = Some(ControllerClient::spawn());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            load_profiles,
            save_profiles,
            choose_executable,
            get_icon,
            get_focus_status,
            start_focus,
            stop_focus,
            retry_restore,
            apply_profile
        ])
        .build(tauri::generate_context!())
        .expect("could not build Foqus Manager");
    app.run(|app_handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            use tauri::Manager;
            if let Some(state) = app_handle.try_state::<AppState>()
                && let Ok(mut controller) = state.controller.lock()
                && let Some(controller) = controller.as_mut()
            {
                controller.shutdown();
            }
        }
    });
}
