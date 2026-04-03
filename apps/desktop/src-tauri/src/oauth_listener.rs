use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use url::Url;

use crate::auth::oauth::dropbox_redirect_uri;
use crate::sync::engine::SyncEngine;

pub(crate) fn start_oauth_callback_listener(
    sync_engine: Arc<Mutex<SyncEngine>>,
) -> Result<(), String> {
    let redirect_uri = dropbox_redirect_uri();
    let url = Url::parse(&redirect_uri).map_err(|e| format!("invalid redirect uri: {e}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| "redirect uri must contain host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "redirect uri must contain port".to_string())?;
    let callback_path = url.path().to_string();
    let bind_addr = format!("{host}:{port}");

    std::thread::spawn(move || {
        let listener = match TcpListener::bind(&bind_addr) {
            Ok(v) => v,
            Err(_) => return,
        };

        for incoming in listener.incoming().take(8) {
            let Ok(mut stream) = incoming else {
                continue;
            };

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

            let mut callback_captured = false;
            if request_path.starts_with(&callback_path) {
                if let Some(query) = request_path.split('?').nth(1) {
                    let mut code = None;
                    let mut state = None;
                    for pair in query.split('&') {
                        let mut parts = pair.splitn(2, '=');
                        let key = parts.next().unwrap_or_default();
                        let value = parts.next().unwrap_or_default();
                        let decoded = urlencoding::decode(value)
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        if key == "code" {
                            code = Some(decoded);
                        } else if key == "state" {
                            state = Some(decoded);
                        }
                    }
                    if let (Some(code), Some(state_value)) = (code, state) {
                        if let Ok(mut engine) = sync_engine.lock() {
                            engine.set_oauth_callback(code, state_value);
                            callback_captured = true;
                        }
                    }
                }
            }

            let body = if callback_captured {
                "<html><body><h3>Dropbox login completed</h3><p>You can return to the app.</p></body></html>"
            } else {
                "<html><body><h3>Waiting for Dropbox callback...</h3></body></html>"
            };

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n{body}"
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();

            if callback_captured {
                break;
            }
        }
    });

    Ok(())
}
