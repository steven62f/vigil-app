mod agent;
mod flags;
mod notify;
mod history;
mod permissions;
mod pipe;
mod jsonl;
mod commands;
mod config;
mod tray;
mod updater;
mod stats;
mod usage;
mod watcher;
mod session_meta;

pub const SETTINGS_STORE: &str = "vigil-settings.json";

use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager};
use tauri_plugin_store::StoreExt;
use tauri_plugin_notification::NotificationExt;
use agent::state_engine::AgentStateEngine;
use agent::pid_watcher::PidWatcher;
use pipe::manager::{PendingApprovals, PipeManager};
use commands::{
    approve_agent, cancel_interrupt, deny_agent, dismiss_agent, focus_terminal, interrupt_agent,
    send_message, get_conversation, get_agent_model,
    start_conversation_watch, stop_conversation_watch,
    get_history_grouped, show_history_window, resume_session, get_agents_snapshot,
    get_agent_rules, set_agent_rule, set_all_agent_rules, always_approve_agent, reset_agent_rules,
    get_usage_state, refresh_usage, set_usage_tracking_enabled,
    get_local_stats,
    get_agent_stats,
    get_session_metadata, set_session_metadata,
    set_hide_notch_if_empty,
    set_autostart,
    get_tools_status, set_tool_enabled, get_tool_hook_diff, get_installation_required,
    notify_get_settings, notify_set_settings, notify_set_token, notify_start_pairing,
    notify_confirm_pairing, notify_send_test, notify_disconnect, notify_open_link,
    get_agent_notif_rules, set_agent_notif_rule, set_all_agent_notif_rules, reset_agent_notif_rules,
    clear_agent_notif_rule,
};
use notify::manager::NotifyManager;
use tray::PendingUpdateUrl;
use usage::manager::UsageManager;
use watcher::ConversationWatcher;
#[cfg(debug_assertions)]
use commands::emit_test_snapshot;

// Tracks the exact physical center of the notch window (f64, never rounded).
// Using the integer outer_position() as the center source accumulates 0.5px rounding
// errors on each resize at fractional DPI scales (125%, 150%…). By storing center_x
// as an exact f64 and only deriving new_x at the moment of set_position, we keep
// the center stable across arbitrarily many open/close cycles.
struct NotchState {
    center_x: f64,
    logical_width: f64,
    // Counts pending set_position calls we issued programmatically, so the Moved
    // handler can ignore them and only update center_x on genuine user drags.
    pending_programmatic_moves: u32,
}

impl NotchState {
    fn new() -> Self {
        Self { center_x: 0.0, logical_width: 160.0, pending_programmatic_moves: 0 }
    }
}

#[tauri::command]
fn resize_notch(app: tauri::AppHandle, width: f64, height: f64) {
    if width.is_nan() || width <= 0.0 || height.is_nan() || height <= 0.0 {
        return;
    }
    let Some(notch) = app.get_webview_window("notch") else { return };
    let Ok(scale) = notch.scale_factor() else { return };
    let Ok(cur_pos) = notch.outer_position() else { return };

    let new_x = {
        let state = app.state::<Mutex<NotchState>>();
        let mut s = state.lock().unwrap();
        let x = (s.center_x - width * scale / 2.0).round() as i32;
        s.pending_programmatic_moves += 1;
        s.logical_width = width;
        x
    };

    let _ = notch.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
        x: new_x,
        y: cur_pos.y,
    }));
    let _ = notch.set_size(tauri::LogicalSize { width, height });
}

fn disable_window_corners(notch: &tauri::WebviewWindow) {
    #[cfg(target_os = "windows")]
    if let Ok(tauri_hwnd) = notch.hwnd() {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
        let hwnd = HWND(tauri_hwnd.0);
        unsafe {
            // Disable Windows 11 DWM rounded corners
            let no_round: u32 = 1; // DWMWCP_DONOTROUND
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWINDOWATTRIBUTE(33), // DWMWA_WINDOW_CORNER_PREFERENCE
                &no_round as *const u32 as *const std::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            );
            // Remove the Windows 11 DWM accent border
            let no_border: u32 = 0xFFFFFFFE; // DWMWA_COLOR_NONE
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWINDOWATTRIBUTE(34), // DWMWA_BORDER_COLOR
                &no_border as *const u32 as *const std::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            );
        }
    }
}

// Returns the saved physical center_x if a valid position was restored, None otherwise.
// Saves and reads the CENTER of the notch (not the left edge) so that the initial
// 300px startup width does not introduce per-launch drift when the notch is narrower.
fn restore_notch_position(app: &tauri::AppHandle, notch: &tauri::WebviewWindow) -> Option<f64> {
    let Ok(store) = app.store(SETTINGS_STORE) else { return None; };
    let y = store.get("notch_y").and_then(|v| v.as_f64()).map(|v| v as i32).unwrap_or(0);

    let win_w = notch.outer_size().map(|s| s.width as f64).unwrap_or(300.0);

    // Prefer the exact center (new key). Fall back to old left-edge key for one-shot migration.
    let center_x = store.get("notch_center_x").and_then(|v| v.as_f64())
        .or_else(|| {
            let saved_x = store.get("notch_x").and_then(|v| v.as_f64())?;
            Some(saved_x + win_w / 2.0)
        });
    let center_x = center_x?;

    // Visibility check on center point
    let monitors = notch.available_monitors().unwrap_or_default();
    let visible = monitors.iter().any(|m| {
        let cx = center_x as i32;
        let cy = y;
        cx >= m.position().x && cx < m.position().x + m.size().width as i32
            && cy >= m.position().y && cy < m.position().y + m.size().height as i32
    });
    if !visible { return None; }

    let left_edge = (center_x - win_w / 2.0).round() as i32;
    let _ = notch.set_position(tauri::Position::Physical(tauri::PhysicalPosition { x: left_edge, y }));
    Some(center_x)
}

// app is needed to write directly to the store — do not rely on the Moved event handler
// for saving, because center_notch runs on a background thread and the Moved event
// fires on the main thread, creating a race where the wrong center_x might be saved.
fn center_notch_at_top(app: &tauri::AppHandle, notch: &tauri::WebviewWindow, state: &Mutex<NotchState>) {
    if let Ok(Some(monitor)) = notch.primary_monitor() {
        let screen_w = monitor.size().width as i32;
        let win_w = notch.outer_size().map(|s| s.width as i32).unwrap_or(300);
        let x = (screen_w - win_w) / 2;
        let center_x = x as f64 + win_w as f64 / 2.0;

        {
            let mut s = state.lock().unwrap();
            s.center_x = center_x;
            if let Ok(scale) = notch.scale_factor() {
                s.logical_width = win_w as f64 / scale;
            }
            s.pending_programmatic_moves += 1;
        }

        let _ = notch.set_position(tauri::Position::Physical(tauri::PhysicalPosition { x, y: 0 }));

        // Write directly to store — the Moved event may fire before or after this thread
        // updates s.center_x, so we cannot rely on it to persist the correct value.
        if let Ok(store) = app.store(SETTINGS_STORE) {
            store.set("notch_center_x", center_x);
            store.set("notch_y", 0i64);
            let _ = store.save();
        }
    }
}

#[tauri::command]
fn center_notch(app: tauri::AppHandle) {
    if let Some(notch) = app.get_webview_window("notch") {
        let state = app.state::<Mutex<NotchState>>();
        center_notch_at_top(&app, &notch, &state);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let engine = Arc::new(Mutex::new(AgentStateEngine::new()));
    let engine_for_pipe = Arc::clone(&engine);
    let engine_for_watcher = Arc::clone(&engine);

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .setup(move |app| {
            // Story 10.3 (revue) — AMORCER le miroir des binaires de hook AVANT toute évaluation de
            // conformité. Sans cela : (1) après chaque mise à jour de Vigil le miroir porte encore
            // l'ancienne version, la conformité le voit périmé et l'utilisateur reçoit une
            // notification « Hooks repaired » par outil — alors qu'il n'y a qu'une maintenance à
            // faire, pas de compromission ; (2) sur un poste où le miroir est définitivement
            // impossible, le garde-fou anti-boucle est encore vide à la première passe, donc le
            // premier outil évalué est réparé et notifié à CHAQUE lancement.
            //
            // L'ordre « fraîcheur avant résolution » de la T3 reste intact : on amorce ici, hors du
            // prédicat de conformité, jamais à l'intérieur.
            config::prime_hook_mirrors(app.handle());

            // Auto-repair (Story 6.8) — réparer les hooks compromis des outils
            // activés AVANT d'évaluer l'ouverture du panel. Un outil réparé ne
            // déclenche plus l'ouverture de la page Installation.
            let auto_repair = app
                .store(SETTINGS_STORE)
                .ok()
                .and_then(|s| s.get("autoRepairHooks"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true); // défaut activé si la clé est absente
            if auto_repair {
                for label in config::auto_repair_hooks(app.handle()) {
                    let _ = app
                        .notification()
                        .builder()
                        .title("Vigil")
                        .body(format!("Hooks repaired for {label}"))
                        .show();
                }
            }

            let installation_required = config::check_installation_required_sync(app.handle());
            if installation_required {
                if let Some(win) = app.get_webview_window("panel") {
                    let _ = win.emit("vigil:navigate", serde_json::json!({ "page": "installation" }));
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }

            tray::setup_tray(app.handle())?;

            if let Some(notch) = app.get_webview_window("notch") {
                disable_window_corners(&notch);
                let notch_state = app.state::<Mutex<NotchState>>();
                if let Some(center_x) = restore_notch_position(app.handle(), &notch) {
                    if let (Ok(size), Ok(scale)) = (notch.outer_size(), notch.scale_factor()) {
                        let mut s = notch_state.lock().unwrap();
                        s.center_x = center_x;
                        s.logical_width = size.width as f64 / scale;
                        s.pending_programmatic_moves += 1;
                    }
                } else {
                    center_notch_at_top(app.handle(), &notch, &notch_state);
                }

                let win_clone = notch.clone();
                notch.on_window_event(move |event| {
                    match event {
                        tauri::WindowEvent::CloseRequested { api, .. } => {
                            api.prevent_close();
                            let _ = win_clone.hide();
                        }
                        tauri::WindowEvent::Moved(pos) => {
                            let notch_state = win_clone.app_handle().state::<Mutex<NotchState>>();
                            let center_to_save = {
                                let mut s = notch_state.lock().unwrap();
                                if s.pending_programmatic_moves > 0 {
                                    s.pending_programmatic_moves -= 1;
                                } else {
                                    // Genuine user drag — update center from new position
                                    let scale = win_clone.scale_factor().unwrap_or(1.0);
                                    s.center_x = pos.x as f64 + s.logical_width * scale / 2.0;
                                }
                                s.center_x
                            };
                            if let Ok(store) = win_clone.app_handle().store(crate::SETTINGS_STORE) {
                                store.set("notch_center_x", center_to_save);
                                store.set("notch_y", pos.y);
                                let _ = store.save();
                            }
                        }
                        _ => {}
                    }
                });
            }

            if let Some(win) = app.get_webview_window("panel") {
                let win_clone = win.clone();
                win.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = win_clone.hide();
                    }
                });
            }

            if let Some(notch) = app.get_webview_window("notch") {
                if let Ok(store) = app.store(SETTINGS_STORE) {
                    let hide_if_empty = store.get("hideNotchIfEmpty")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if hide_if_empty {
                        let _ = notch.hide();
                    }
                }
            }

            // Sync autostart registry only when at least one tool is properly installed;
            // always rewrite the path when enabled (handles reinstall / path-change scenario).
            if !config::check_installation_required_sync(app.handle()) {
                let store_enabled = app.store(SETTINGS_STORE)
                    .ok()
                    .and_then(|s| s.get("startWithWindows"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                if store_enabled {
                    config::set_autostart(app.handle(), true).ok();
                } else if config::is_autostart_enabled() {
                    config::set_autostart(app.handle(), false).ok();
                }
            }

            let usage_manager = UsageManager::new(app.handle().clone());
            app.manage(Arc::clone(&usage_manager));

            let notify_manager = NotifyManager::new(app.handle().clone());
            app.manage(Arc::clone(&notify_manager));

            PipeManager::start(
                app.handle().clone(),
                engine_for_pipe,
                Arc::clone(&usage_manager),
                Arc::clone(&notify_manager),
            );
            PidWatcher::start(app.handle().clone(), engine_for_watcher);

            let app_for_update = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Some(payload) = updater::check_for_updates().await {
                    tray::handle_update_found(&app_for_update, payload).await;
                }
            });

            Ok(())
        })
        .manage(engine)
        .manage(Mutex::new(PendingApprovals::new()))
        .manage(Mutex::new(NotchState::new()))
        .manage(ConversationWatcher::new_state())
        .manage(PendingUpdateUrl(Mutex::new(None)));

    #[cfg(debug_assertions)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        approve_agent,
        deny_agent,
        send_message,
        focus_terminal,
        dismiss_agent,
        interrupt_agent,
        cancel_interrupt,
        emit_test_snapshot,
        resize_notch,
        center_notch,
        get_conversation,
        get_agent_model,
        start_conversation_watch,
        stop_conversation_watch,
        get_history_grouped,
        show_history_window,
        resume_session,
        get_agents_snapshot,
        get_agent_rules,
        set_agent_rule,
        set_all_agent_rules,
        always_approve_agent,
        reset_agent_rules,
        get_usage_state,
        refresh_usage,
        set_usage_tracking_enabled,
        get_local_stats,
        get_agent_stats,
        get_session_metadata,
        set_session_metadata,
        set_hide_notch_if_empty,
        set_autostart,
        get_tools_status,
        set_tool_enabled,
        get_tool_hook_diff,
        get_installation_required,
        notify_get_settings,
        notify_set_settings,
        notify_set_token,
        notify_start_pairing,
        notify_confirm_pairing,
        notify_send_test,
        notify_disconnect,
        notify_open_link,
        get_agent_notif_rules,
        set_agent_notif_rule,
        set_all_agent_notif_rules,
        reset_agent_notif_rules,
        clear_agent_notif_rule,
    ]);

    #[cfg(not(debug_assertions))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        approve_agent,
        deny_agent,
        send_message,
        focus_terminal,
        dismiss_agent,
        interrupt_agent,
        cancel_interrupt,
        resize_notch,
        center_notch,
        get_conversation,
        get_agent_model,
        start_conversation_watch,
        stop_conversation_watch,
        get_history_grouped,
        show_history_window,
        resume_session,
        get_agents_snapshot,
        get_agent_rules,
        set_agent_rule,
        set_all_agent_rules,
        always_approve_agent,
        reset_agent_rules,
        get_usage_state,
        refresh_usage,
        set_usage_tracking_enabled,
        get_local_stats,
        get_agent_stats,
        get_session_metadata,
        set_session_metadata,
        set_hide_notch_if_empty,
        set_autostart,
        get_tools_status,
        set_tool_enabled,
        get_tool_hook_diff,
        get_installation_required,
        notify_get_settings,
        notify_set_settings,
        notify_set_token,
        notify_start_pairing,
        notify_confirm_pairing,
        notify_send_test,
        notify_disconnect,
        notify_open_link,
        get_agent_notif_rules,
        set_agent_notif_rule,
        set_all_agent_notif_rules,
        reset_agent_notif_rules,
        clear_agent_notif_rule,
    ]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
