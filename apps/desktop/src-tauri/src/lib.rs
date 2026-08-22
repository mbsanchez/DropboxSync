mod auth;
mod auth_session;
#[cfg(windows)]
mod cloud_filter;
mod cloudsc;
mod cloudsc_ops;
mod commands;
mod dropbox_transfer;
mod error;
mod finder_extension;
mod flyout_geometry;
mod fs_watcher;
mod logging;
mod models;
mod oauth_listener;
mod open_handlers;
mod overlay_state;
mod path_util;
mod remote_index;
mod remote_longpoll;
mod run_events;
mod sharing;
mod shell_actions;
mod state;
mod storage;
mod sync;
mod sync_pipeline;
#[cfg(windows)]
mod windows_file_assoc;
#[cfg(windows)]
mod windows_identity;
#[cfg(windows)]
mod windows_shell_menu;
#[cfg(windows)]
mod windows_startup;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use state::AppState;
use storage::db::Db;
use storage::secure_store::SecureStore;
use sync::engine::SyncEngine;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, PhysicalPosition, Rect, WebviewWindow};

/// Timestamp of the most recent tray-triggered show/hide of the `main` flyout.
/// `run_events::handle_run_event` reads this via [`recently_toggled`] to skip the
/// blur-hide handler right after a tray click, so a click that both focuses the
/// (already-visible) window and re-triggers the tray toggle doesn't race a
/// blur-driven hide immediately followed by a click-driven show.
static LAST_TOGGLE: Mutex<Option<Instant>> = Mutex::new(None);

fn stamp_toggle() {
    if let Ok(mut guard) = LAST_TOGGLE.lock() {
        *guard = Some(Instant::now());
    }
}

/// True when the `main` flyout was toggled (shown or hidden) from the tray within
/// the last `within` duration. Used to suppress the blur-hide handler right after a
/// tray click so it doesn't fight the toggle it just performed.
pub(crate) fn recently_toggled(within: Duration) -> bool {
    match LAST_TOGGLE.lock() {
        Ok(guard) => guard.map(|t| t.elapsed() < within).unwrap_or(false),
        Err(_) => false,
    }
}

/// Every display, primary first.
///
/// The ordering is load-bearing: [`flyout_geometry::monitor_for_point`] falls back to `[0]`
/// when a point lands outside every display, which is exactly what a coordinate-space mismatch
/// produces — so the fallback should be the primary rather than whichever display the OS
/// happened to enumerate first.
fn collect_monitor_boxes(app: &AppHandle) -> Vec<flyout_geometry::MonitorBox> {
    use flyout_geometry::MonitorBox;
    let mut boxes: Vec<MonitorBox> = Vec::new();
    let mut push = |m: &tauri::Monitor| {
        let b = MonitorBox {
            x: m.position().x as f64,
            y: m.position().y as f64,
            width: m.size().width as f64,
            height: m.size().height as f64,
        };
        if !boxes.contains(&b) {
            boxes.push(b);
        }
    };
    if let Ok(Some(m)) = app.primary_monitor() {
        push(&m);
    }
    if let Ok(all) = app.available_monitors() {
        for m in all {
            push(&m);
        }
    }
    boxes
}

/// Logs the display layout once, at startup (DBSYNC-85).
///
/// The click-time line in [`position_flyout`] is the one that answers GitHub #112, but it only
/// appears when someone clicks the tray icon — and the icon can be invisible when the menu bar
/// overflows, which is exactly what happened while trying to verify this. Logging the layout at
/// launch means the plumbing is provably alive without depending on a click, and it gives the
/// measurement half its context for free.
pub(crate) fn log_display_layout(app: &AppHandle) {
    tracing::info!(
        target: "dbsync85",
        monitors = ?collect_monitor_boxes(app),
        "display layout at startup"
    );
}

/// Positions the `main` flyout window just above the tray icon, right-aligned to
/// it — anchored to the primary/tray monitor's bottom-right corner (bottom taskbar
/// assumption; no multi-monitor/taskbar-edge detection). All math is done in
/// physical pixels; the result is clamped to the target monitor's bounds so the
/// flyout never renders off-screen.
fn position_flyout(
    app: &AppHandle,
    window: &WebviewWindow,
    position: PhysicalPosition<f64>,
    rect: Rect,
) {
    use flyout_geometry::{flyout_origin, monitor_for_point, MonitorBox, TrayRect};

    let primary = app.primary_monitor().ok().flatten();
    let boxes = collect_monitor_boxes(app);

    // DBSYNC-85 instrumentation. One line per click, not per frame. It prints the raw tray
    // position, the tray rect and every display together so the coordinate space can be named
    // from a SINGLE line rather than reconstructed from three — see GitHub #112. The reported
    // multi-monitor inversion is deliberately NOT corrected until this has been read on a
    // two-display machine; guessing the sign would look right on one arrangement half the time.
    tracing::info!(
        target: "dbsync85",
        tray_click_x = position.x,
        tray_click_y = position.y,
        monitors = ?boxes,
        "tray click geometry"
    );

    let scale = primary.as_ref().map(|m| m.scale_factor()).unwrap_or(1.0);
    let rect_pos = rect.position.to_physical::<f64>(scale);
    let rect_size = rect.size.to_physical::<f64>(scale);

    let Some(monitor) = monitor_for_point(&boxes, position.x, position.y) else {
        let _ = window.show();
        return;
    };

    let tray = TrayRect {
        x: rect_pos.x,
        y: rect_pos.y,
        width: rect_size.width,
        height: rect_size.height,
    };
    // `main` is a fixed LOGICAL 360x600 (tauri.conf.json); convert to physical.
    let (x, y) = flyout_origin(tray, monitor, 360.0 * scale, 600.0 * scale, 8.0 * scale);

    let _ = window.set_position(PhysicalPosition::new(x, y));
    let _ = window.show();
}

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

    // The notification plugin pulls WinRT `windows`-crate imports that make the
    // lib *test* binary fail to load on Windows (STATUS_ENTRYPOINT_NOT_FOUND — a
    // known Tauri/windows-rs cargo-test issue; the real app binary is unaffected).
    // Gating it out of `cfg(test)` lets the linker drop those imports from the
    // test binary while the shipped app keeps native notifications (DBSYNC-52).
    #[cfg(not(test))]
    {
        builder = builder.plugin(tauri_plugin_notification::init());
    }

    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // A shell-action verb (`--action/--path`) and a `.cloudsc` open are
            // mutually exclusive invocations — handle the action first and skip
            // the open path so a `--path <file>.cloudsc` can't double-dispatch.
            if let Some((action, path)) = crate::shell_actions::parse_action_args(&argv) {
                tracing::info!(action = %action, path = %path.display(), "shell action on running instance");
                crate::shell_actions::handle_action_from_os(app, &action, &path);
            } else {
                let paths = crate::open_handlers::cloudsc_paths_from_argv(&argv);
                if !paths.is_empty() {
                    tracing::info!(?paths, "file open on running instance");
                    crate::open_handlers::handle_cloudsc_paths_from_os(app, paths);
                }
            }
            // Do not raise an empty dashboard when the user is fully set up (tray-only
            // workflow). `main` is a tray-click-only flyout now; onboarding surfaces via
            // the `setup` window instead.
            if let Some(state) = app.try_state::<AppState>() {
                if crate::commands::should_show_main_window_for_onboarding(state.inner()) {
                    if let Some(window) = app.get_webview_window("setup") {
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

            log_display_layout(app.handle());

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
                    // DBSYNC-33 Slice 2: hook up the COM context menu when its DLL
                    // is deployed next to the exe (replaces the legacy flyout).
                    crate::windows_shell_menu::sync_shell_menu_registration();
                }

                // DBSYNC-58: CfAPI shell integration (the Explorer status column,
                // DBSYNC-57) needs package identity from the sparse package.
                tracing::info!(
                    package_identity = crate::windows_identity::has_package_identity(),
                    "windows package identity"
                );
            }

            // Windows/Linux: double-clicking `.cloudsc` runs `app.exe "path\to\file.cloudsc"`; there is no
            // `RunEvent::Opened` (macOS/iOS only). Handle argv on first launch here.
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            {
                // Action verb takes precedence over `.cloudsc` open (mutually
                // exclusive; prevents a `--path <file>.cloudsc` double-dispatch).
                let argv: Vec<String> = std::env::args().collect();
                if let Some((action, path)) = crate::shell_actions::parse_action_args(&argv) {
                    tracing::info!(action = %action, path = %path.display(), "startup shell action");
                    crate::shell_actions::handle_action_from_os(&app.handle().clone(), &action, &path);
                } else {
                    let paths = crate::open_handlers::cloudsc_paths_from_current_exe_args();
                    if !paths.is_empty() {
                        tracing::info!(?paths, "startup file args");
                        crate::open_handlers::handle_cloudsc_paths_from_os(&app.handle().clone(), paths);
                    }
                }
            }

            let app_handle = app.handle().clone();

            // "Open Dashboard" is gone: the tray icon itself now toggles the `main`
            // flyout on a single left click (see `on_tray_icon_event` below); the menu
            // needs Settings (DBSYNC-36, opens the `setup` window) and Exit.
            let settings = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&settings, &quit])?;
            // DBSYNC-32: start on the idle icon; update_tray_tooltip swaps the glyph
            // (sync/error) live. The asset and the template flag both come from
            // `tray_icon_for_label` so this initial icon can never disagree with the ones
            // the live swap installs — macOS gets a monochrome template, Windows the
            // colour brand icon (DBSYNC-78).
            let (idle_bytes, idle_as_template) = crate::auth_session::tray_icon_for_label("Idle");
            let tray_image = Image::from_bytes(idle_bytes)
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            let tray_builder = TrayIconBuilder::with_id("main")
                .icon(tray_image)
                .icon_as_template(idle_as_template)
                .menu(&menu)
                // DBSYNC-82. `tray-icon` defaults `menu_on_left_click` to TRUE, so without
                // this the left click opened the menu as well as running the handler below,
                // and the menu stealing focus made the flyout blur-hide itself. Three
                // behaviours on one gesture; it took about three clicks to land the window.
                // Off, the menu belongs to the right click — which `tray-icon` shows
                // unconditionally, so nothing is lost and the handler needs no change.
                .show_menu_on_left_click(false)
                .tooltip("DropboxSyncDesktop - Idle")
                .on_tray_icon_event(move |_tray, event| {
                    // The flyout's only trigger — true because `show_menu_on_left_click`
                    // above takes the menu off this gesture, not because nothing else
                    // could claim it. A left-click toggles the window (shows+positions if
                    // hidden, hides if visible). No DoubleClick. The right click also
                    // arrives here carrying `MouseButton::Right` and is ignored by the
                    // match below, so it opens the menu without touching the flyout.
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        position,
                        rect,
                        ..
                    } = event
                    {
                        if let Some(window) = app_handle.get_webview_window("main") {
                            let visible = window.is_visible().unwrap_or(false);
                            if visible {
                                let _ = window.hide();
                            } else {
                                position_flyout(&app_handle, &window, position, rect);
                                let _ = window.set_focus();
                            }
                            stamp_toggle();
                        }
                    }
                })
                .on_menu_event(move |app, event| {
                    if event.id.as_ref() == "quit" {
                        app.exit(0);
                    } else if event.id.as_ref() == "settings" {
                        if let Some(window) = app.get_webview_window("setup") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                    }
                });

            let tray_ok = match tray_builder.build(app) {
                Ok(_) => true,
                Err(err) => {
                    tracing::error!(error = %err, "failed to create tray icon");
                    false
                }
            };

            // Step 0: both windows start hidden (tauri.conf.json `visible: false`). The
            // `main` flyout is never shown here — it only appears via the tray click
            // handler above (and only when the tray was built successfully). When
            // onboarding (auth/sync folder) is incomplete, show the `setup` window instead.
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

                // Clear phantom "Error" state from editor-temp files a previous
                // build tracked/failed (DBSYNC-55).
                match crate::sync_pipeline::cleanup_stale_upload_state(state.inner()) {
                    Ok(count) if count > 0 => {
                        tracing::info!(count, "cleaned stale editor-temp sync state on startup");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "failed to clean stale upload state"),
                }

                // Recover any `.dbsync-dehydrate.tmp` aside files left behind by a
                // crash/kill mid-dehydration, before the scan/watcher can see them
                // (DBSYNC-64).
                match crate::cloudsc_ops::recover_stray_dehydrate_asides(state.inner()) {
                    Ok(count) if count > 0 => {
                        tracing::info!(count, "recovered stray dehydration aside file(s) on startup");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!(error = %e, "failed to recover stray dehydration asides"),
                }

                crate::overlay_state::refresh_overlay_state_internal(state.inner());

                // DBSYNC-36: the ignore-glob predicate is backed by a process-wide
                // static (`path_util::USER_IGNORE_GLOBS`), so the persisted CSV must be
                // loaded into it here — otherwise saved patterns stay inactive until the
                // user re-saves the settings panel this session.
                match state.db.get_ignore_globs_csv() {
                    Ok(csv) => {
                        crate::path_util::set_user_ignore_globs(
                            crate::path_util::parse_ignore_globs_csv(csv),
                        );
                    }
                    Err(e) => tracing::error!(error = %e, "failed to load ignore globs at startup"),
                }

                if crate::commands::should_show_main_window_for_onboarding(state.inner()) {
                    if let Some(setup_window) = app.get_webview_window("setup") {
                        let _ = setup_window.show();
                        let _ = setup_window.set_focus();
                    }
                }
            }

            // Single set-once site for `state::APP_HANDLE`, read by
            // `dropbox_transfer::emit_upload_progress` to emit `upload-progress` events
            // from background sync work. Not duplicated in `start_background_scheduler`.
            let _ = crate::state::APP_HANDLE.set(app.handle().clone());

            // Start the filesystem watcher for near-instant local change
            // detection (DBSYNC-29); best-effort — the 5-min fallback scan
            // covers anything it can't watch.
            if let Some(state) = app.try_state::<crate::state::AppState>() {
                if let Err(e) = crate::fs_watcher::arm_watcher(&state) {
                    tracing::warn!(error = %e, "failed to arm filesystem watcher at startup");
                }
                // Start the Dropbox longpoll loop for near-instant remote change
                // detection (DBSYNC-30). Idles until logged in; the 5-min sweep
                // remains the reconciliation fallback.
                crate::remote_longpoll::start_longpoll(&state);
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::start_oauth_flow,
            commands::complete_oauth_flow,
            commands::cancel_oauth_flow,
            commands::get_startup_requirements,
            finder_extension::open_finder_extension_settings,
            finder_extension::restart_finder,
            commands::pick_sync_folder_dialog,
            commands::start_background_scheduler,
            commands::hide_main_window,
            commands::show_main_window,
            commands::show_setup_window,
            commands::open_logs,
            commands::open_sync_folder,
            commands::set_sync_folder,
            commands::get_sync_status,
            commands::get_sync_dashboard,
            commands::retry_failed_jobs,
            commands::resolve_conflict,
            commands::confirm_pending_deletions,
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
            commands::set_selective_sync_filters,
            commands::get_ignore_globs,
            commands::set_ignore_globs,
            commands::disconnect_dropbox,
            commands::get_startup_at_login,
            commands::set_startup_at_login
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        run_events::handle_run_event(app_handle, event);
    });
}
