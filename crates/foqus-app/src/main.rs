#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
use base64::Engine;
use foqus_core::{
    GameProfile, ProcessCapability, ProcessSample, ProfileStore, default_profile_path,
    normalize_executable,
};
use serde::Serialize;
#[cfg(windows)]
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
    capability: ProcessCapability,
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
            capability: p.capability,
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

fn main() {
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
    tauri::Builder::default()
        .manage(AppState {
            snapshot,
            profile_lock: Mutex::new(()),
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            load_profiles,
            save_profiles,
            choose_executable,
            get_icon
        ])
        .run(tauri::generate_context!())
        .expect("could not start Foqus Manager");
}
