use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri_plugin_notification::NotificationExt;

fn show_panel_page(app: &AppHandle, page: &str) {
    if let Some(win) = app.get_webview_window("panel") {
        // Emit BEFORE show so React updates the page while still hidden — avoids any flash
        // of the previous page. unminimize() is required because show() alone keeps the
        // window in the taskbar when it was minimized.
        let _ = win.emit("vigil:navigate", serde_json::json!({ "page": page }));
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

pub struct CheckUpdateItem(pub tauri::menu::MenuItem<tauri::Wry>);

pub struct PendingUpdateUrl(pub Mutex<Option<String>>);

pub async fn handle_update_found(app: &AppHandle, payload: crate::updater::UpdatePayload) {
    app.notification()
        .builder()
        .title("Vigil")
        .body(format!("Version {} available", payload.version))
        .show()
        .ok();

    let label = format!("Update available (v{})", payload.version);
    {
        let item = app.state::<Mutex<CheckUpdateItem>>();
        let guard = item.lock().unwrap();
        guard.0.set_text(&label).ok();
        guard.0.set_enabled(true).ok();
    }
    *app.state::<PendingUpdateUrl>().0.lock().unwrap() = Some(payload.url);
}

pub async fn run_check_update(app: AppHandle) {
    use crate::updater;

    {
        let item = app.state::<Mutex<CheckUpdateItem>>();
        let guard = item.lock().unwrap();
        guard.0.set_text("Checking...").ok();
        guard.0.set_enabled(false).ok();
    }

    match updater::check_for_updates().await {
        Some(payload) => {
            handle_update_found(&app, payload).await;
        }
        None => {
            {
                let item = app.state::<Mutex<CheckUpdateItem>>();
                let guard = item.lock().unwrap();
                guard.0.set_text("You're up to date").ok();
                guard.0.set_enabled(false).ok();
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            {
                let item = app.state::<Mutex<CheckUpdateItem>>();
                let guard = item.lock().unwrap();
                guard.0.set_text("Check for update").ok();
                guard.0.set_enabled(true).ok();
            }
        }
    }
}

pub fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let version_label = format!("Version {}", env!("CARGO_PKG_VERSION"));

    let check_update_item = MenuItemBuilder::with_id("check_update", "Check for update")
        .enabled(true)
        .build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&MenuItemBuilder::with_id("show", "Show").build(app)?)
        .item(&MenuItemBuilder::with_id("settings", "Settings").build(app)?)
        .item(&MenuItemBuilder::with_id("history", "History").build(app)?)
        .item(&MenuItemBuilder::with_id("usage", "Usage").build(app)?)
        .item(&MenuItemBuilder::with_id("statistics", "Statistics").build(app)?)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&MenuItemBuilder::with_id("version", &version_label).enabled(false).build(app)?)
        .item(&check_update_item)
        .item(&PredefinedMenuItem::separator(app)?)
        .item(&MenuItemBuilder::with_id("quit", "Quit").build(app)?)
        .build()?;

    app.manage(Mutex::new(CheckUpdateItem(check_update_item)));

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::InvalidIcon(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "default window icon not found",
        )))?;

    let tray = TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .tooltip("Vigil")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                if let Some(win) = app.get_webview_window("notch") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
            "history" => show_panel_page(app, "history"),
            "usage" => show_panel_page(app, "usage"),
            "statistics" => show_panel_page(app, "statistics"),
            "settings" => show_panel_page(app, "settings"),
            "check_update" => {
                let pending = app.state::<PendingUpdateUrl>();
                let url = pending.0.lock().unwrap().clone();
                if let Some(url) = url {
                    let _ = open::that(&url);
                } else {
                    {
                        let item = app.state::<Mutex<CheckUpdateItem>>();
                        let guard = item.lock().unwrap();
                        guard.0.set_enabled(false).ok();
                    }
                    let app_clone = app.clone();
                    tauri::async_runtime::spawn(async move {
                        run_check_update(app_clone).await;
                    });
                }
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(win) = app.get_webview_window("notch") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
        })
        .build(app)?;

    app.manage(tray);
    Ok(())
}
