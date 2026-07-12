mod auth;
mod auth_session;
mod cloudsc;
mod cloudsc_ops;
mod commands;
mod dropbox_transfer;
mod logging;
mod models;
mod oauth_listener;
mod open_handlers;
mod overlay_state;
mod path_util;
mod remote_index;
mod run_events;
mod state;
mod storage;
mod sync;
mod sync_pipeline;
#[cfg(windows)]
mod windows_file_assoc;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use state::AppState;
use storage::db::Db;
use storage::secure_store::SecureStore;
use sync::engine::SyncEngine;
use tauri::image::Image;
use tauri::Manager;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logging::init_tracing();

    let db = Db::new().expect("failed to initialize sqlite db");
    let mut sync_engine = SyncEngine::new();
    if let Ok(Some(folder)) = db.get_sync_folder() {
        sync_engine.set_tracked_path(folder);
    }
    let app_state = AppState {
        secure_store: SecureStore::new(),
        db: Arc::new(db),
        sync_engine: Arc::new(Mutex::new(sync_engine)),
        token_cache: Arc::new(Mutex::new(None)),
        scheduler_started: Arc::new(Mutex::new(false)),
        oauth_listener: Arc::new(Mutex::new(None)),
        sync_running: Arc::new(AtomicBool::new(false)),
        token_refresh_lock: Arc::new(Mutex::new(())),
        http_client: state::build_http_client(),
    };

    let mut builder = tauri::Builder::default()
        .manage(app_state)
        .plugin(tauri_plugin_opener::init());

    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            let paths = crate::open_handlers::cloudsc_paths_from_argv(&argv);
            if !paths.is_empty() {
                tracing::info!(?paths, "file open on running instance");
                crate::open_handlers::handle_cloudsc_paths_from_os(&app, paths);
            }
            // Do not raise an empty dashboard when the user is fully set up (tray-only workflow).
            if let Some(state) = app.try_state::<AppState>() {
                if crate::commands::should_show_main_window_for_onboarding(state.inner()) {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.unminimize();
                        let _ = window.set_focus();
                    }
                }
            }
        }));
    }

    let app = builder
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_dock_visibility(false);

            #[cfg(windows)]
            {
                let skip = std::env::var("DROPBOXSYNC_SKIP_WINDOWS_FILE_ASSOC")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                if skip {
                    tracing::info!(
                        "skipping per-user .cloudsc registration (DROPBOXSYNC_SKIP_WINDOWS_FILE_ASSOC)"
                    );
                } else {
                    match crate::windows_file_assoc::register_user_cloudsc_association() {
                        Ok(()) => tracing::info!(
                            "registered per-user .cloudsc association (HKCU) for portable testing"
                        ),
                        Err(e) => tracing::error!(error = %e, "per-user .cloudsc association failed"),
                    }
                }
            }

            // Windows/Linux: double-clicking `.cloudsc` runs `app.exe "path\to\file.cloudsc"`; there is no
            // `RunEvent::Opened` (macOS/iOS only). Handle argv on first launch here.
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            {
                let paths = crate::open_handlers::cloudsc_paths_from_current_exe_args();
                if !paths.is_empty() {
                    tracing::info!(?paths, "startup file args");
                    crate::open_handlers::handle_cloudsc_paths_from_os(&app.handle().clone(), paths);
                }
            }

            let app_handle = app.handle().clone();

            let open_dashboard = MenuItem::with_id(app, "open_dashboard", "Open Dashboard", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_dashboard, &quit])?;
            let tray_image = Image::from_bytes(include_bytes!("../icons/32x32.png"))
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            let tray_builder = TrayIconBuilder::with_id("main")
                .icon(tray_image)
                .icon_as_template(true)
                .menu(&menu)
                .tooltip("DropboxSyncDesktop - Idle")
                .on_tray_icon_event(move |_tray, event| {
                    if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                        if let Some(window) = app_handle.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                    }
                })
                .on_menu_event(move |app, event| {
                    if event.id.as_ref() == "open_dashboard" {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                    } else if event.id.as_ref() == "quit" {
                        app.exit(0);
                    }
                });

            let tray_ok = match tray_builder.build(app) {
                Ok(_) => true,
                Err(err) => {
                    tracing::error!(error = %err, "failed to create tray icon");
                    false
                }
            };

            // Step 0: start with the main window hidden; the UI shows it when setup is incomplete.
            if tray_ok {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }

            if let Some(state) = app.try_state::<AppState>() {
                // Recover jobs stuck in `running` from a previous crash/kill before the
                // background scheduler starts processing the queue, so a large in-flight
                // upload resumes from its checkpoint instead of being silently zombied.
                match state.db.recover_running_jobs() {
                    Ok(count) if count > 0 => {
                        tracing::info!(count, "recovered stale running job(s) on startup");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "failed to recover running jobs"),
                }

                crate::overlay_state::refresh_overlay_state_internal(state.inner());
            }

            // Single set-once site for `state::APP_HANDLE`, read by
            // `dropbox_transfer::emit_upload_progress` to emit `upload-progress` events
            // from background sync work. Not duplicated in `start_background_scheduler`.
            let _ = crate::state::APP_HANDLE.set(app.handle().clone());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::start_oauth_flow,
            commands::complete_oauth_flow,
            commands::cancel_oauth_flow,
            commands::get_startup_requirements,
            commands::pick_sync_folder_dialog,
            commands::start_background_scheduler,
            commands::hide_main_window,
            commands::show_main_window,
            commands::set_sync_folder,
            commands::get_sync_status,
            commands::get_sync_dashboard,
            commands::retry_failed_jobs,
            commands::scan_local_changes,
            commands::process_sync_queue,
            commands::sync_tick,
            commands::trigger_sync_tick,
            commands::list_remote_folder,
            commands::trigger_download_remote_file,
            commands::trigger_hydrate_remote_folder,
            commands::index_remote_root_placeholders,
            commands::list_cloudsc_placeholders,
            commands::trigger_hydrate_cloudsc_placeholder,
            commands::get_selective_sync_filters,
            commands::set_selective_sync_filters
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        run_events::handle_run_event(&app_handle, event);
    });
}
