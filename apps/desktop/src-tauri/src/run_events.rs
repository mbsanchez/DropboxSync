use std::path::Path;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use std::path::PathBuf;
use std::time::Duration;

use tauri::AppHandle;
use tauri::DragDropEvent;
use tauri::Emitter;
use tauri::Manager;
use tauri::RunEvent;
use tauri::WebviewEvent;
use tauri::WindowEvent;

use crate::open_handlers::handle_cloudsc_paths_from_os;

pub(crate) fn handle_run_event(app_handle: &AppHandle, event: RunEvent) {
    match event {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        RunEvent::Opened { urls } => {
            let mut paths: Vec<PathBuf> = Vec::new();
            for url in &urls {
                tracing::info!(%url, "opened with file");
                if let Ok(p) = url.to_file_path() {
                    paths.push(p);
                }
                if let Err(e) = app_handle.emit("open-with-file", url.to_string()) {
                    tracing::error!(error = %e, "emit open-with-file failed");
                }
            }
            handle_cloudsc_paths_from_os(app_handle, paths);
        }
        RunEvent::ExitRequested { code, api, .. } => {
            if code.is_none() {
                api.prevent_exit();
                if let Some(w) = app_handle.get_webview_window("main") {
                    let _ = w.hide();
                }
            }
        }
        RunEvent::WindowEvent {
            label,
            event: win_evt,
            ..
        } => {
            match win_evt {
                // `main` is a tray flyout, not the app: closing it (e.g. Alt+F4) just
                // hides it, it never quits the app.
                WindowEvent::CloseRequested { api, .. } if label == "main" => {
                    api.prevent_close();
                    if let Some(w) = app_handle.get_webview_window("main") {
                        let _ = w.hide();
                    }
                }
                // `setup` is a real, closable window (not a flyout) — let it close
                // genuinely; do not prevent-close/hide it like `main`.
                WindowEvent::CloseRequested { .. } if label == "setup" => {}
                // Hide the flyout when it loses focus (click-away), unless it was *just*
                // toggled from the tray — otherwise a tray click on an already-focused
                // flyout would blur-hide it and then immediately re-show it, or a click
                // that both blurs a previous window and shows the flyout could race the
                // blur-hide against the tray's own show.
                WindowEvent::Focused(false) if label == "main" => {
                    if let Some(w) = app_handle.get_webview_window("main") {
                        let visible = w.is_visible().unwrap_or(false);
                        if visible && !crate::recently_toggled(Duration::from_millis(250)) {
                            let _ = w.hide();
                        }
                    }
                }
                WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) => {
                    for path in paths.iter() {
                        let p: &Path = path.as_ref();
                        tracing::info!(file_path = %p.display(), "file drop (window)");
                        if let Err(e) =
                            app_handle.emit("file-drop", path.to_string_lossy().to_string())
                        {
                            tracing::error!(error = %e, "emit file-drop failed");
                        }
                    }
                    handle_cloudsc_paths_from_os(app_handle, paths);
                }
                _ => {}
            }
        }
        RunEvent::WebviewEvent {
            event: WebviewEvent::DragDrop(DragDropEvent::Drop { paths, .. }),
            ..
        } => {
            for path in paths.iter() {
                let p: &Path = path.as_ref();
                tracing::info!(file_path = %p.display(), "file drop (webview)");
                if let Err(e) = app_handle.emit("file-drop", path.to_string_lossy().to_string()) {
                    tracing::error!(error = %e, "emit file-drop failed");
                }
            }
            handle_cloudsc_paths_from_os(app_handle, paths);
        }
        _ => {}
    }
}
