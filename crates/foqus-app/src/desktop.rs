//! Desktop window/tray behaviour.
//!
//! The tray icon is the durable home for the application: closing or
//! minimizing the window hides it instead of terminating the process, and only
//! the explicit Exit action shuts the controller down. If the tray cannot be
//! created, the window keeps its normal close behaviour so the user can never
//! end up with a hidden, unreachable window.
use tauri::{
    App, AppHandle, Manager,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

pub const TRAY_ID: &str = "foqus-tray";

pub fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Install the notification-area icon and the hide-on-close/minimize
/// behaviour. Window interception is only enabled when the tray exists.
pub fn install(app: &mut App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "tray-show", "Show", true, None::<&str>)?;
    let exit = MenuItem::with_id(app, "tray-exit", "Exit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &exit])?;
    let icon = app.default_window_icon().cloned();
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("Foqus Manager")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "tray-show" => show_main(app),
            "tray-exit" => crate::exit_application(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
            | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => show_main(tray.app_handle()),
            _ => {}
        });
    if let Some(icon) = icon {
        builder = builder.icon(icon);
    }
    builder.build(app)?;
    install_window_behavior(app);
    Ok(())
}

fn install_window_behavior(app: &App) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let handle = window.clone();
    window.on_window_event(move |event| match event {
        tauri::WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = handle.hide();
        }
        tauri::WindowEvent::Resized(_) if handle.is_minimized().unwrap_or(false) => {
            let _ = handle.hide();
        }
        _ => {}
    });
}
