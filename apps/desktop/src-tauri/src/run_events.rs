use std::path::{Path, PathBuf};

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
                eprintln!("DropboxSyncDesktop: opened with file: {url}");
                if let Ok(p) = url.to_file_path() {
                    paths.push(p);
                }
                if let Err(e) = app_handle.emit("open-with-file", url.to_string()) {
                    eprintln!("emit open-with-file failed: {e}");
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
        RunEvent::WindowEvent { label, event: win_evt, .. } => {
            match win_evt {
                WindowEvent::CloseRequested { api, .. } if label == "main" => {
                    api.prevent_close();
                    if let Some(w) = app_handle.get_webview_window("main") {
                        let _ = w.hide();
                    }
                }
                WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) => {
                    for path in paths.iter() {
                        let p: &Path = path.as_ref();
                        eprintln!(
                            "DropboxSyncDesktop: file drop (window): {}",
                            p.display()
                        );
                        if let Err(e) =
                            app_handle.emit("file-drop", path.to_string_lossy().to_string())
                        {
                            eprintln!("emit file-drop failed: {e}");
                        }
                    }
                    handle_cloudsc_paths_from_os(app_handle, paths);
                }
                _ => {}
            }
        }
        RunEvent::WebviewEvent { event: wv_evt, .. } => {
            if let WebviewEvent::DragDrop(DragDropEvent::Drop { paths, .. }) = wv_evt {
                for path in paths.iter() {
                    let p: &Path = path.as_ref();
                    eprintln!(
                        "DropboxSyncDesktop: file drop (webview): {}",
                        p.display()
                    );
                    if let Err(e) =
                        app_handle.emit("file-drop", path.to_string_lossy().to_string())
                    {
                        eprintln!("emit file-drop failed: {e}");
                    }
                }
                handle_cloudsc_paths_from_os(app_handle, paths);
            }
        }
        _ => {}
    }
}
