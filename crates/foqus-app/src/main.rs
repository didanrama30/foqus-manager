#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
use base64::Engine;
mod client;
mod desktop;

use client::ControllerClient;
#[cfg(not(windows))]
use foqus_core::ProcessControlBackend;
use foqus_core::controller::FocusController;
use foqus_core::{
    FocusState, GameProfile, ProcessCapability, ProcessIdentity, ProcessSample, ProcessState,
    ProfileStore, SessionStatus, default_journal_path, default_profile_path, normalize_executable,
    protocol::serve,
};
use foqus_windows::PlatformBackend;
use serde::Serialize;
use std::time::{Duration, Instant};
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
    monitoring_error: Option<String>,
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
    let existing = ProfileStore::load(&default_profile_path()).map_err(|e| e.to_string())?;
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
    let removes_profiles = existing
        .profiles
        .iter()
        .any(|old| !store.profiles.iter().any(|new| new.id == old.id));
    let active_profile = if removes_profiles {
        read_active_profile_id(&state)
    } else {
        Ok(None)
    };
    verify_profile_removal(&existing, &store, active_profile)?;
    store
        .save_atomic(&default_profile_path())
        .map_err(|e| e.to_string())
}

fn read_active_profile_id(state: &AppState) -> Result<Option<String>, String> {
    let mut guard = state
        .controller
        .lock()
        .map_err(|_| "focus controller lock poisoned".to_string())?;
    let controller = guard.as_mut().ok_or("focus controller is not ready")?;
    let status = controller.status()?;
    if matches!(status.state, FocusState::Idle) {
        return Ok(None);
    }
    status
        .profile_id
        .map(Some)
        .ok_or_else(|| "the active Focus Mode profile could not be determined".to_string())
}

/// Removing profiles is allowed only when the controller state is known and
/// the removed profile is not the one the active session owns. If the status
/// cannot be read, fail closed instead of risking deletion of an active game.
fn verify_profile_removal(
    existing: &ProfileStore,
    updated: &ProfileStore,
    active_profile: Result<Option<String>, String>,
) -> Result<(), String> {
    let removed: Vec<&str> = existing
        .profiles
        .iter()
        .filter(|old| !updated.profiles.iter().any(|new| new.id == old.id))
        .map(|profile| profile.id.as_str())
        .collect();
    if removed.is_empty() {
        return Ok(());
    }
    let active_profile = active_profile.map_err(|error| {
        format!("Cannot verify the active Focus Mode profile; refusing to remove profiles. {error}")
    })?;
    if let Some(active_profile) = active_profile
        && removed.contains(&active_profile.as_str())
    {
        return Err(
            "The active Focus Mode profile cannot be removed while Focus Mode is running. Stop Focus Mode first."
                .into(),
        );
    }
    Ok(())
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
    {
        let snapshot = state.snapshot.lock().map_err(|e| e.to_string())?;
        let process = snapshot
            .processes
            .iter()
            .find(|process| {
                process.identity.pid == pid
                    && process.identity.creation_time_100ns == creation_time_100ns
            })
            .ok_or("the selected game is no longer running")?;
        if process.state != ProcessState::Running {
            return Err("the selected game cannot be queried".into());
        }
        if process.capability != ProcessCapability::Candidate {
            return Err("the selected process is protected and cannot be focused".into());
        }
        let path = process
            .executable_path
            .as_ref()
            .ok_or("the selected game has no executable path")?;
        if normalize_executable(path) != normalize_executable(&profile.executable_path) {
            return Err("the selected game no longer matches this profile".into());
        }
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

fn shutdown_controller(app: &tauri::AppHandle) {
    use tauri::Manager;
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match state.controller.try_lock() {
            Ok(mut guard) => {
                if let Some(mut controller) = guard.take() {
                    controller.shutdown();
                }
                return;
            }
            Err(std::sync::TryLockError::Poisoned(_)) => return,
            Err(std::sync::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    // A command is still using the controller. Exiting closes
                    // its stdin, which makes the controller restore and exit;
                    // an unfinished restore stays in the journal for the next
                    // startup.
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Explicit Exit: stop the controller (restoring what Foqus changed and
/// preserving the journal if restore cannot finish), then quit.
pub(crate) fn exit_application(app: &tauri::AppHandle) {
    shutdown_controller(app);
    app.exit(0);
}

#[tauri::command]
fn exit_app(app: tauri::AppHandle) -> Result<(), String> {
    exit_application(&app);
    Ok(())
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
                let sample = backend.sample();
                if let Ok(mut value) = shared.lock() {
                    *value = Snapshot {
                        processes: sample.processes.into_iter().map(UiProcess::from).collect(),
                        memory: sample.memory,
                        cpu: sample.cpu,
                        demo: false,
                        monitoring_error: sample.error,
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
            desktop::show_main(app);
        }))
        .manage(app_state)
        .setup(|app| {
            use tauri::Manager;
            {
                let state = app.state::<AppState>();
                *state
                    .controller
                    .lock()
                    .map_err(|_| "controller lock poisoned")? = Some(ControllerClient::spawn());
            }
            if let Err(error) = desktop::install(app) {
                tracing::warn!("notification-area tray unavailable: {error}");
            }
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
            apply_profile,
            exit_app
        ])
        .build(tauri::generate_context!())
        .expect("could not build Foqus Manager");
    app.run(|app_handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            shutdown_controller(app_handle);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::verify_profile_removal;
    use foqus_core::{GameProfile, ProfileStore};
    use std::path::PathBuf;

    fn profile(id: &str) -> GameProfile {
        GameProfile {
            id: id.into(),
            name: id.into(),
            executable_path: PathBuf::from(r"C:\Games\game.exe"),
            game_priority: Default::default(),
            reduce_background_memory: false,
            managed_executables: vec![],
            excluded_executables: vec![],
            restore_when_game_exits: true,
        }
    }

    fn store(ids: &[&str]) -> ProfileStore {
        ProfileStore {
            schema_version: foqus_core::PROFILE_SCHEMA_VERSION,
            selected_profile: ids.first().map(|id| (*id).to_string()),
            profiles: ids.iter().map(|id| profile(id)).collect(),
        }
    }

    #[test]
    fn removal_fails_closed_when_active_profile_cannot_be_read() {
        let existing = store(&["game-a", "game-b"]);
        let updated = store(&["game-a"]);
        let result = verify_profile_removal(
            &existing,
            &updated,
            Err("focus controller is not ready".into()),
        );
        assert!(result.unwrap_err().contains("refusing to remove profiles"));
    }

    #[test]
    fn removal_rejects_the_active_profile_only() {
        let existing = store(&["game-a", "game-b"]);
        let updated = store(&["game-a"]);
        assert!(verify_profile_removal(&existing, &updated, Ok(Some("game-b".into()))).is_err());
        assert!(verify_profile_removal(&existing, &updated, Ok(Some("game-a".into()))).is_ok());
        assert!(verify_profile_removal(&existing, &updated, Ok(None)).is_ok());
    }

    #[test]
    fn unknown_status_does_not_block_saves_without_removals() {
        let existing = store(&["game-a"]);
        let updated = store(&["game-a"]);
        assert!(verify_profile_removal(&existing, &updated, Err("unavailable".into())).is_ok());
    }
}
