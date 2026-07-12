use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tauri::{AppHandle, Emitter, EventTarget};
use url::Url;

use crate::auth::oauth::dropbox_redirect_uri;
use crate::auth::oauth_complete::complete_oauth_internal;
use crate::models::DropboxOauthFinishedEvent;
use crate::state::{AppState, OauthListenerControl};

/// Socket address for the loopback listener. Browsers hitting `http://localhost:…` often connect
/// to `127.0.0.1` (IPv4). On Windows, binding to `localhost` can listen only on IPv6, so the
/// redirect never reaches this app — normalize `localhost` to `127.0.0.1`.
fn loopback_bind_address(host: &str, port: u16) -> String {
    match host.to_ascii_lowercase().as_str() {
        "localhost" | "127.0.0.1" => format!("127.0.0.1:{port}"),
        "::1" | "[::1]" => format!("[::1]:{port}"),
        _ => format!("{host}:{port}"),
    }
}

fn normalize_path(path: &str) -> String {
    path.trim_end_matches('/').to_string()
}

/// Result of parsing the HTTP request-target for an OAuth redirect (RFC 7230 origin-form or
/// absolute-form `http://host/...`).
enum OauthRedirectParse {
    Success { code: String, state: String },
    OAuthDenied { message: String },
    /// Callback path matched but query is incomplete (or favicon noise on another path).
    Incomplete,
    /// Not our OAuth callback path (e.g. `/favicon.ico`).
    WrongPath,
}

fn parse_oauth_redirect(request_target: &str, expected_path: &str) -> OauthRedirectParse {
    let expected = normalize_path(expected_path);

    let url = if request_target.starts_with("http://") || request_target.starts_with("https://") {
        match Url::parse(request_target) {
            Ok(u) => u,
            Err(_) => return OauthRedirectParse::WrongPath,
        }
    } else {
        match Url::parse(&format!("http://127.0.0.1{request_target}")) {
            Ok(u) => u,
            Err(_) => return OauthRedirectParse::WrongPath,
        }
    };

    let path = normalize_path(url.path());
    if path != expected {
        return OauthRedirectParse::WrongPath;
    }

    let mut code = None;
    let mut state = None;
    let mut oauth_error = None;
    let mut oauth_err_desc = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => oauth_error = Some(v.into_owned()),
            "error_description" => oauth_err_desc = Some(v.into_owned()),
            _ => {}
        }
    }

    if let Some(err) = oauth_error {
        let msg = oauth_err_desc.unwrap_or(err);
        return OauthRedirectParse::OAuthDenied { message: msg };
    }

    match (code, state) {
        (Some(c), Some(s)) => OauthRedirectParse::Success { code: c, state: s },
        _ => OauthRedirectParse::Incomplete,
    }
}

fn emit_oauth_finished(app: &AppHandle, event: DropboxOauthFinishedEvent) {
    match app.emit_to(
        EventTarget::webview_window("main"),
        "dropbox-oauth-finished",
        event.clone(),
    ) {
        Ok(()) => {}
        Err(e) => {
            tracing::error!(error = %e, "emit_to webview_window(main) failed");
            if let Err(e2) = app.emit("dropbox-oauth-finished", event) {
                tracing::error!(error = %e2, "emit global dropbox-oauth-finished failed");
            }
        }
    }
}

/// Stops any running localhost OAuth listener and joins its thread so the TCP port is released.
pub(crate) fn stop_oauth_listener(
    oauth_listener: &Arc<Mutex<Option<OauthListenerControl>>>,
) -> Result<(), String> {
    let mut guard = oauth_listener
        .lock()
        .map_err(|_| "oauth listener mutex poisoned".to_string())?;
    if let Some(ctrl) = guard.take() {
        ctrl.cancel.store(true, Ordering::SeqCst);
        let _ = ctrl.handle.join();
    }
    Ok(())
}

pub(crate) fn start_oauth_callback_listener(
    app: AppHandle,
    app_state: AppState,
    oauth_listener: Arc<Mutex<Option<OauthListenerControl>>>,
) -> Result<(), String> {
    stop_oauth_listener(&oauth_listener)?;

    let redirect_uri = dropbox_redirect_uri();
    let url = Url::parse(&redirect_uri).map_err(|e| format!("invalid redirect uri: {e}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| "redirect uri must contain host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "redirect uri must contain port".to_string())?;
    let callback_path = url.path().to_string();
    let bind_addr = loopback_bind_address(host, port);

    let listener = TcpListener::bind(&bind_addr).map_err(|e| {
        format!("cannot bind OAuth callback listener on {bind_addr}: {e}. Close other apps using this port or cancel OAuth and retry.")
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking: {e}"))?;

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_thread = cancel.clone();

    let handle = thread::spawn(move || {
        run_oauth_listener_loop(
            listener,
            cancel_for_thread,
            app,
            app_state,
            callback_path,
        );
    });

    {
        let mut guard = oauth_listener
            .lock()
            .map_err(|_| "oauth listener mutex poisoned".to_string())?;
        *guard = Some(OauthListenerControl { cancel, handle });
    }

    Ok(())
}

fn run_oauth_listener_loop(
    listener: TcpListener,
    cancel: Arc<AtomicBool>,
    app: AppHandle,
    app_state: AppState,
    callback_path: String,
) {
    loop {
        if cancel.load(Ordering::SeqCst) {
            break;
        }

        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut reader = BufReader::new(&stream);
                let mut first_line = String::new();
                if reader.read_line(&mut first_line).is_err() {
                    continue;
                }

                let request_path = first_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();

                let oauth_result = parse_oauth_redirect(&request_path, &callback_path);

                let (body, done_after_write): (String, bool) = match &oauth_result {
                    OauthRedirectParse::Success { .. } => (
                        "<html><body><h3>Dropbox login completed</h3><p>You can return to the app.</p></body></html>"
                            .to_string(),
                        true,
                    ),
                    OauthRedirectParse::OAuthDenied { message } => (
                        format!(
                            "<html><body><h3>Dropbox login was not completed</h3><p>{}</p></body></html>",
                            html_escape(message)
                        ),
                        true,
                    ),
                    OauthRedirectParse::Incomplete => (
                        "<html><body><h3>Waiting for Dropbox callback...</h3></body></html>".to_string(),
                        false,
                    ),
                    OauthRedirectParse::WrongPath => (
                        "<html><body><h3>Waiting for Dropbox callback...</h3></body></html>".to_string(),
                        false,
                    ),
                };

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{body}"
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();

                if !done_after_write {
                    continue;
                }

                let stop_listener = matches!(
                    &oauth_result,
                    OauthRedirectParse::Success { .. } | OauthRedirectParse::OAuthDenied { .. }
                );

                match oauth_result {
                    OauthRedirectParse::Success { code, state } => {
                        let result = tauri::async_runtime::block_on(complete_oauth_internal(
                            &app_state, code, state,
                        ));
                        if let Err(ref e) = result {
                            tracing::error!(error = %e, "Dropbox OAuth token exchange failed");
                        }
                        let event = DropboxOauthFinishedEvent {
                            ok: result.is_ok(),
                            message: result.err(),
                        };
                        emit_oauth_finished(&app, event);
                    }
                    OauthRedirectParse::OAuthDenied { message } => {
                        emit_oauth_finished(
                            &app,
                            DropboxOauthFinishedEvent {
                                ok: false,
                                message: Some(message),
                            },
                        );
                    }
                    OauthRedirectParse::Incomplete | OauthRedirectParse::WrongPath => {}
                }

                if stop_listener {
                    break;
                }
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
