//! How the File Provider extension asks the host for a file (DBSYNC-95 slice 3, ADR-0004). macOS only.
//!
//! The extension has no network and no token, and it only reads `app.db`, so a download crosses the
//! process boundary as files under `fp-exchange/` beside the database:
//!
//! | path | written by | meaning |
//! | --- | --- | --- |
//! | `host.lock` | host | the host holds an exclusive `flock` on it for its whole life |
//! | `requests/<id>.json` | extension | a download request, appeared by rename |
//! | `requests/<id>.cancelled` | extension | it stopped waiting for a request the host had taken |
//! | `staging/<id>.data` | host | the downloaded bytes, in place before the result appears |
//! | `results/<id>.json` | host | how the request ended, appeared by rename |
//!
//! The kernel drops the lock when the process dies, however it dies, so the extension can tell a host
//! that is down from one that is slow, before it queues and while it waits. A Darwin notification
//! wakes the host, and a scan every [`SCAN_INTERVAL`] delivers a request whose notification was lost
//! or was posted before the observer existed.
//!
//! Requests carry only an item id and results only a hash and a class, so nothing here puts a name or
//! a path in the container. The system clones and unlinks the file the extension hands over
//! (`NSFileProviderReplicatedExtension.h`, *File ownership*), which leaves the sweep with what a crash
//! or a cancelled request left behind.

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::storage::db::Db;

/// The exchange directory, beside the database. `Exchange.swift` must agree.
const EXCHANGE_DIR: &str = "fp-exchange";
/// Suffix of the Darwin notification name, after the App Group id. `Exchange.swift` must agree.
const NOTIFICATION_SUFFIX: &str = ".fp-request";
/// The version of the two JSON shapes. A request of another version is dropped.
const EXCHANGE_VERSION: u32 = 1;
/// How often the queue is scanned without a notification.
const SCAN_INTERVAL: Duration = Duration::from_secs(2);
/// How often the sweep runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// What the sweep leaves alone. Longer than any request can take: a download is bounded by a 300 s
/// timeout, and the extension gives up on a request after 600 s.
const STALE_AFTER: Duration = Duration::from_secs(60 * 60);
/// How long after an `Offline` or `Auth` answer the host lifts the system's throttling, so the system
/// asks again. Still offline, it gets the same answer and this comes round again.
const RESOLVE_DELAY: Duration = Duration::from_secs(30);
/// When the host repeats its startup lift. The extension can find the lock free just before the host
/// takes it and answer just after the first lift, which that lift would then miss.
const STARTUP_RESOLVE_REPEAT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq, Deserialize)]
struct Request {
    version: u32,
    item_id: i64,
    /// The system says Finder asked (`NSFileProviderRequest.isFileViewerRequest`): a person opened the
    /// file, as opposed to Spotlight or Quick Look reading it.
    #[serde(default)]
    viewer: bool,
}

/// What the queue held under one id.
#[derive(Debug, PartialEq, Eq)]
enum Pending {
    Serve(Request),
    /// A request nobody can read: an appex and a host of different versions, or a failed read.
    Unreadable,
}

/// How a request ended, in the classes the extension maps onto File Provider errors.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum Outcome {
    /// The bytes are in staging, and this is the row's `content_version` they were checked against.
    Ok { content_version: String },
    /// Dropbox could not be reached: `serverUnreachable`.
    Offline,
    /// The session needs the user: `notAuthenticated`.
    Auth,
    /// Dropbox will not hand these bytes out, a Paper document for one: `NSFileReadNoPermissionError`,
    /// which the system shows instead of retrying.
    Refused,
    /// Anything that can pass by itself: a transient error, and the system asks again.
    Retry,
}

#[derive(Serialize)]
struct ResultFile<'a> {
    version: u32,
    #[serde(flatten)]
    outcome: &'a Outcome,
}

/// The directories of one exchange.
struct Exchange {
    root: PathBuf,
}

/// A request id is the file's own stem, and it is joined into paths, so only a UUID is accepted.
fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

impl Exchange {
    fn open(data_dir: &Path) -> AppResult<Self> {
        let exchange = Self {
            root: data_dir.join(EXCHANGE_DIR),
        };
        fs::create_dir_all(exchange.requests())?;
        fs::create_dir_all(exchange.staging())?;
        fs::create_dir_all(exchange.results())?;
        Ok(exchange)
    }

    fn requests(&self) -> PathBuf {
        self.root.join("requests")
    }

    fn staging(&self) -> PathBuf {
        self.root.join("staging")
    }

    fn results(&self) -> PathBuf {
        self.root.join("results")
    }

    fn staged(&self, id: &str) -> PathBuf {
        self.staging().join(format!("{id}.data"))
    }

    /// Takes the host's lock, waiting out an extension that is testing it at this moment. The handle
    /// is the lock: it must live as long as the process.
    fn lock(&self) -> AppResult<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join("host.lock"))?;
        file.lock()?;
        Ok(file)
    }

    /// Removes and returns every readable request. A file that is not a request is removed too: nothing
    /// else would ever take it, and it says so in the log. Removing first means a request is served
    /// once, whichever of the notification and the scan found it.
    ///
    /// Nothing after the first removal may fail the pass: those requests exist nowhere else. An entry
    /// that vanished meanwhile was taken back by the extension, and one that cannot be removed is
    /// skipped until the next scan.
    fn take_pending(&self) -> AppResult<Vec<(String, Pending)>> {
        let mut taken = Vec::new();
        for entry in fs::read_dir(self.requests())?.flatten() {
            let path = entry.path();
            // The extension writes `.<id>.tmp` and renames it to `<id>.json`.
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let read = fs::read(&path);
            if let Err(e) = fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(error = %e, "File Provider exchange: cannot take a request");
                }
                continue;
            }
            let request = read
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Request>(&bytes).ok())
                .filter(|r| r.version == EXCHANGE_VERSION);
            match (request, valid_id(&id)) {
                (Some(request), true) => taken.push((id, Pending::Serve(request))),
                // Someone is waiting for this id, so it gets an answer.
                (None, true) => {
                    tracing::warn!("File Provider exchange: unreadable request");
                    taken.push((id, Pending::Unreadable));
                }
                (_, false) => tracing::warn!("File Provider exchange: dropped a stray file"),
            }
        }
        Ok(taken)
    }

    /// The marker the extension leaves when it stops waiting for a request the host already took.
    fn cancel_marker(&self, id: &str) -> PathBuf {
        self.requests().join(format!("{id}.cancelled"))
    }

    /// Whether the extension gave up on `id`. Consumes the marker.
    fn take_cancelled(&self, id: &str) -> bool {
        fs::remove_file(self.cancel_marker(id)).is_ok()
    }

    /// Makes the outcome visible, all at once: the rename is what the extension waits for.
    fn publish(&self, id: &str, outcome: &Outcome) -> AppResult<()> {
        let bytes = serde_json::to_vec(&ResultFile {
            version: EXCHANGE_VERSION,
            outcome,
        })?;
        let temp = self.results().join(format!(".{id}.tmp"));
        fs::write(&temp, bytes)?;
        fs::rename(&temp, self.results().join(format!("{id}.json")))?;
        Ok(())
    }

    /// Removes whatever is older than [`STALE_AFTER`]: results and staged files nobody collected, and
    /// requests that were written and never renamed.
    fn sweep(&self, now: SystemTime) -> usize {
        let mut removed = 0;
        for dir in [self.requests(), self.staging(), self.results()] {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|at| now.duration_since(at).is_ok_and(|age| age > STALE_AFTER));
                if stale && fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        removed
    }
}

/// The one thing that reaches Dropbox, so no test does.
trait Fetcher {
    fn fetch(&self, rev: &str, shown_path: &str, target: &Path) -> AppResult<()>;
}

struct DropboxFetcher {
    state: AppState,
}

impl Fetcher for DropboxFetcher {
    fn fetch(&self, rev: &str, shown_path: &str, target: &Path) -> AppResult<()> {
        crate::dropbox_transfer::download_revision_to_path(&self.state, rev, shown_path, target)
    }
}

fn outcome_for_error(error: &AppError) -> Outcome {
    match error {
        AppError::Network(_) => Outcome::Offline,
        AppError::Auth(_) | AppError::Dropbox { status: 401, .. } => Outcome::Auth,
        AppError::Dropbox {
            status: 409,
            message,
        } if error_summary(message).is_some_and(|s| s.starts_with("unsupported_file")) => {
            Outcome::Refused
        }
        _ => Outcome::Retry,
    }
}

/// Dropbox's `error_summary`, such as `unsupported_file/..`, out of an error message that also holds the
/// path. It names the error and never the file.
fn error_summary(message: &str) -> Option<&str> {
    const KEY: &str = r#""error_summary":"#;
    let rest = message[message.find(KEY)? + KEY.len()..].trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(&rest[..rest.find('"')?])
}

/// An error for the log. `AppError::Dropbox` carries the path, and the log lives in the container.
fn error_class(error: &AppError) -> String {
    match error {
        AppError::Network(_) => "network".to_string(),
        AppError::Auth(_) => "auth".to_string(),
        AppError::Dropbox { status, message } => match error_summary(message) {
            Some(summary) => format!("dropbox {status} {summary}"),
            None => format!("dropbox {status}"),
        },
        AppError::Storage(_) => "storage".to_string(),
        AppError::Sync(_) => "sync".to_string(),
        AppError::Io(_) => "io".to_string(),
        AppError::Other(_) => "other".to_string(),
    }
}

/// Downloads what the request names into staging. A row that is not a live file answers `Retry`,
/// never "no such item": during a split rename the row comes back (ADR-0004, *A rename split in two*),
/// and a real delete reaches the system through the change feed.
///
/// The download names the row's `rev`, so the bytes are that version whatever happened to the path
/// meanwhile, and the hash proves it before the extension can hand them over.
fn serve(
    db: &Db,
    exchange: &Exchange,
    id: &str,
    request: &Request,
    fetcher: &dyn Fetcher,
) -> Outcome {
    let target = match db.projection_download_target(request.item_id) {
        Ok(Some(target)) => target,
        Ok(None) => return Outcome::Retry,
        Err(e) => {
            tracing::warn!(item_id = request.item_id, error = %e, "File Provider exchange: cannot read the row");
            return Outcome::Retry;
        }
    };
    let staged = exchange.staged(id);
    if let Err(e) = fetcher.fetch(&target.rev, &target.path_lower, &staged) {
        tracing::warn!(
            item_id = request.item_id,
            class = error_class(&e),
            "File Provider exchange: download failed"
        );
        return outcome_for_error(&e);
    }
    match crate::path_util::hash_file(&staged) {
        Ok((hash, _, _)) if hash == target.content_hash => Outcome::Ok {
            content_version: String::from_utf8_lossy(&target.content_version).into_owned(),
        },
        outcome => {
            tracing::warn!(
                item_id = request.item_id,
                matched = outcome.is_ok(),
                "File Provider exchange: the download did not hash to the row"
            );
            let _ = fs::remove_file(&staged);
            Outcome::Retry
        }
    }
}

/// The lift of the system's throttling, [`RESOLVE_DELAY`] after the LAST answer that caused any. Counted
/// from the last one because a lift only helps an error the system has already received: an answer that
/// lands just before a lift counted from an earlier one reaches the system after it, and stays throttled.
struct ResolveTimer {
    last_answer: Mutex<Option<Instant>>,
}

impl ResolveTimer {
    const fn new() -> Self {
        Self {
            last_answer: Mutex::new(None),
        }
    }

    /// Records an answer. True when no lift is pending, so the caller has to start one.
    fn note_answer(&self, now: Instant) -> bool {
        let mut last = self.last_answer.lock().unwrap_or_else(|e| e.into_inner());
        last.replace(now).is_none()
    }

    /// How long the pending lift still has to wait. `Duration::ZERO` means fire now, and clears it, so
    /// that the next answer starts another.
    fn remaining(&self, now: Instant) -> Duration {
        let mut last = self.last_answer.lock().unwrap_or_else(|e| e.into_inner());
        let left = last.map_or(Duration::ZERO, |at| {
            RESOLVE_DELAY.saturating_sub(now.duration_since(at))
        });
        if left.is_zero() {
            *last = None;
        }
        left
    }
}

static RESOLVE_TIMER: ResolveTimer = ResolveTimer::new();

fn lift_throttling_later() {
    if !RESOLVE_TIMER.note_answer(Instant::now()) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("file-provider-resolve".into())
        .spawn(|| {
            loop {
                let left = RESOLVE_TIMER.remaining(Instant::now());
                if left.is_zero() {
                    break;
                }
                std::thread::sleep(left);
            }
            crate::file_provider::signal_errors_resolved();
        });
    if let Err(e) = spawned {
        // Clear the pending lift, or no later answer would start one.
        RESOLVE_TIMER
            .last_answer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        tracing::error!(error = %e, "File Provider exchange: could not schedule the retry signal");
    }
}

/// What a person who opened a file Dropbox will not hand out gets instead: the file on dropbox.com, where a
/// Paper document opens in its editor. The system shows nothing for the refusal itself.
fn open_in_browser(db: &Db, item_id: i64) {
    let url = match db.projection_web_location(item_id) {
        Ok(Some((path_lower, name))) => web_preview_url(&path_lower, &name),
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(item_id, error = %e, "File Provider exchange: cannot read the row to open");
            return;
        }
    };
    let Some(url) = url else { return };
    // The URL holds the path, so it goes to `open` and never to the log.
    match std::process::Command::new("/usr/bin/open")
        .arg(url.as_str())
        .spawn()
    {
        Ok(_) => tracing::info!(item_id, "File Provider exchange: opened on dropbox.com"),
        Err(e) => {
            tracing::warn!(item_id, error = %e, "File Provider exchange: cannot open the browser")
        }
    }
}

/// `https://www.dropbox.com/preview/<parent>/<name>`, encoded segment by segment. Checked live: it opens a
/// Paper document, by way of a redirect to its own link (#235). Dropbox's web paths ignore case, so the
/// lowercase parent from the projection reaches the same folder; the name keeps its case.
fn web_preview_url(path_lower: &str, name: &str) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse("https://www.dropbox.com/preview").ok()?;
    let parent = path_lower.rsplit_once('/').map_or("", |(parent, _)| parent);
    {
        let mut segments = url.path_segments_mut().ok()?;
        segments.extend(parent.split('/').filter(|s| !s.is_empty()));
        segments.push(name);
    }
    Some(url)
}

/// One browser tab per file per [`BROWSER_REPEAT`], whatever the system's retries ask for.
struct BrowserOpens {
    last: Mutex<Option<(i64, Instant)>>,
}

/// Longer than the system takes to ask twice for one double click.
const BROWSER_REPEAT: Duration = Duration::from_secs(10);

impl BrowserOpens {
    const fn new() -> Self {
        Self {
            last: Mutex::new(None),
        }
    }

    fn allow(&self, item_id: i64, now: Instant) -> bool {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let repeat =
            last.is_some_and(|(id, at)| id == item_id && now.duration_since(at) < BROWSER_REPEAT);
        if !repeat {
            *last = Some((item_id, now));
        }
        !repeat
    }
}

static BROWSER_OPENS: BrowserOpens = BrowserOpens::new();

/// Publishes `Retry` unless the request was answered, however the worker ends: a waiter whose host holds
/// the lock has no other way to hear about a panic.
struct AnswerGuard<'a> {
    exchange: &'a Exchange,
    id: &'a str,
    answered: bool,
}

impl AnswerGuard<'_> {
    /// The final step of a request: nothing is left for a host to do if the extension gave up.
    fn answer(mut self, outcome: &Outcome) {
        self.answered = true;
        if self.exchange.take_cancelled(self.id) {
            let _ = fs::remove_file(self.exchange.staged(self.id));
            return;
        }
        if let Err(e) = self.exchange.publish(self.id, outcome) {
            tracing::error!(error = %e, "File Provider exchange: could not publish the result");
            // The waiter will not take these. A short result may still fit where they did not.
            let _ = fs::remove_file(self.exchange.staged(self.id));
            if !matches!(outcome, Outcome::Retry) {
                let _ = self.exchange.publish(self.id, &Outcome::Retry);
            }
        }
    }
}

impl Drop for AnswerGuard<'_> {
    fn drop(&mut self) {
        if !self.answered {
            let _ = fs::remove_file(self.exchange.staged(self.id));
            let _ = self.exchange.publish(self.id, &Outcome::Retry);
        }
    }
}

fn handle(state: &AppState, exchange: &Exchange, id: &str, request: &Request) {
    let guard = AnswerGuard {
        exchange,
        id,
        answered: false,
    };
    let fetcher = DropboxFetcher {
        state: state.clone(),
    };
    let outcome = serve(&state.db, exchange, id, request, &fetcher);
    tracing::info!(
        item_id = request.item_id,
        ok = matches!(outcome, Outcome::Ok { .. }),
        "File Provider exchange: request served"
    );
    if matches!(outcome, Outcome::Offline | Outcome::Auth) {
        lift_throttling_later();
    }
    if outcome == Outcome::Refused
        && request.viewer
        && BROWSER_OPENS.allow(request.item_id, Instant::now())
    {
        open_in_browser(&state.db, request.item_id);
    }
    guard.answer(&outcome);
}

extern "C" {
    /// `notify.h`, in libSystem. Each notification writes the 4-byte token to the descriptor.
    fn notify_register_file_descriptor(
        name: *const std::ffi::c_char,
        notify_fd: *mut std::ffi::c_int,
        flags: std::ffi::c_int,
        out_token: *mut std::ffi::c_int,
    ) -> u32;
}

/// Forwards every Darwin notification to `wake` from its own thread. Failing leaves the scan.
fn observe_notifications(name: &str, wake: mpsc::Sender<()>) -> Result<(), String> {
    let name = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
    let (mut fd, mut token) = (-1, 0);
    // SAFETY: `name` is NUL-terminated and outlives the call, and both out pointers are live.
    let status =
        unsafe { notify_register_file_descriptor(name.as_ptr(), &raw mut fd, 0, &raw mut token) };
    if status != 0 || fd < 0 {
        return Err(format!("notify_register_file_descriptor returned {status}"));
    }
    // SAFETY: the descriptor was just created for this registration and has no other owner.
    let mut descriptor = unsafe { File::from_raw_fd(fd) };
    std::thread::Builder::new()
        .name("file-provider-notify".into())
        .spawn(move || {
            let mut token_bytes = [0_u8; 4];
            while descriptor.read_exact(&mut token_bytes).is_ok() {
                tracing::debug!("File Provider exchange: notification received");
                if wake.send(()).is_err() {
                    return;
                }
            }
            tracing::warn!("File Provider exchange: the notification descriptor closed");
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// One pass over the queue: every request gets a worker, or an answer now.
fn scan(state: &AppState, exchange: &Arc<Exchange>) {
    let pending = match exchange.take_pending() {
        Ok(pending) => pending,
        Err(e) => {
            tracing::warn!(error = %e, "File Provider exchange: cannot read the queue");
            return;
        }
    };
    for (id, pending) in pending {
        let Pending::Serve(request) = pending else {
            let _ = exchange.publish(&id, &Outcome::Retry);
            continue;
        };
        let (worker_state, worker_exchange, worker_id) =
            (state.clone(), exchange.clone(), id.clone());
        let spawned = std::thread::Builder::new()
            .name("file-provider-download".into())
            .spawn(move || handle(&worker_state, &worker_exchange, &worker_id, &request));
        if let Err(e) = spawned {
            tracing::error!(error = %e, "File Provider exchange: could not start a download");
            let _ = exchange.publish(&id, &Outcome::Retry);
        }
    }
}

fn run_queue(state: &AppState, exchange: &Arc<Exchange>, wake: &mpsc::Receiver<()>) {
    let mut last_sweep = None;
    loop {
        scan(state, exchange);
        let due = last_sweep.is_none_or(|at: Instant| at.elapsed() >= SWEEP_INTERVAL);
        if due {
            let removed = exchange.sweep(SystemTime::now());
            if removed > 0 {
                tracing::info!(removed, "File Provider exchange: swept stale files");
            }
            last_sweep = Some(Instant::now());
        }
        match wake.recv_timeout(SCAN_INTERVAL) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            // The observer never started or has gone: only the scan is left.
            Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(SCAN_INTERVAL),
        }
    }
}

/// What failed while this host was down can be retried now. On its own thread, so a slow
/// `fileproviderd` at login delays no request.
fn lift_throttling_at_startup() {
    let spawned = std::thread::Builder::new()
        .name("file-provider-startup-resolve".into())
        .spawn(|| {
            crate::file_provider::signal_errors_resolved();
            std::thread::sleep(STARTUP_RESOLVE_REPEAT);
            crate::file_provider::signal_errors_resolved();
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "File Provider exchange: could not signal at startup");
    }
}

/// Starts the exchange when the switch is on, and removes what an earlier run left when it is off, which
/// may be staged user content. A failure logs and leaves the extension answering that the host is down,
/// which is true of this host's ability to serve it.
pub(crate) fn start(state: &AppState) {
    if !state.db.projection_active() {
        remove_leftovers(state.db.data_dir());
        return;
    }
    let exchange = match Exchange::open(state.db.data_dir()) {
        Ok(exchange) => Arc::new(exchange),
        Err(e) => {
            tracing::error!(error = %e, "File Provider exchange: cannot open the directory");
            return;
        }
    };
    let (wake, woken) = mpsc::channel();
    match crate::app_group::group_id() {
        Some(group) => {
            if let Err(e) = observe_notifications(&format!("{group}{NOTIFICATION_SUFFIX}"), wake) {
                tracing::warn!(error = %e, "File Provider exchange: no notifications, scanning only");
            }
        }
        None => tracing::warn!("File Provider exchange: no App Group, scanning only"),
    }
    let state = state.clone();
    let spawned = std::thread::Builder::new()
        .name("file-provider-exchange".into())
        .spawn(move || {
            // Here and not on the caller, which is the app's setup: a second host waits on this
            // thread alone.
            let _held_for_the_life_of_the_process = match exchange.lock() {
                Ok(lock) => lock,
                Err(e) => {
                    tracing::error!(error = %e, "File Provider exchange: cannot take the host lock");
                    return;
                }
            };
            tracing::info!("File Provider exchange: running");
            lift_throttling_at_startup();
            run_queue(&state, &exchange, &woken);
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "File Provider exchange: could not start");
    }
}

fn remove_leftovers(data_dir: &Path) {
    let root = data_dir.join(EXCHANGE_DIR);
    if root.exists() {
        if let Err(e) = fs::remove_dir_all(&root) {
            tracing::warn!(error = %e, "File Provider exchange: cannot remove the directory");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::DropboxEntry;
    use crate::storage::db::projection::ProjectedEntry;
    use std::cell::Cell;

    const REV: &str = "65f9d7c5f289256342f44";

    struct World {
        _dir: tempfile::TempDir,
        db: Db,
        exchange: Exchange,
    }

    fn entry(tag: &str, display: &str, id: Option<&str>, hash: Option<String>) -> DropboxEntry {
        DropboxEntry {
            tag: tag.to_string(),
            path_display: Some(display.to_string()),
            path_lower: Some(display.to_lowercase()),
            content_hash: hash,
            rev: id.map(|_| REV.to_string()),
            server_modified: id.map(|_| "2026-11-01T10:00:00Z".to_string()),
            size: id.map(|_| 5),
            id: id.map(str::to_string),
        }
    }

    /// The Dropbox content hash of `bytes`, from the function the host hashes with.
    fn hash_of(bytes: &[u8]) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, bytes).unwrap();
        crate::path_util::hash_file(&path).unwrap().0
    }

    /// One live file, written by the production snapshot writer.
    fn world(bytes: &[u8]) -> World {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::new_at(&dir.path().join("app.db")).unwrap();
        db.set_projection_enabled(true);
        let listing = [entry("file", "/a.txt", Some("id:A"), Some(hash_of(bytes)))];
        let projected: Vec<ProjectedEntry> = listing
            .iter()
            .filter_map(ProjectedEntry::from_entry)
            .collect();
        db.project_snapshot(&projected).unwrap();
        let exchange = Exchange::open(dir.path()).unwrap();
        World {
            _dir: dir,
            db,
            exchange,
        }
    }

    impl World {
        fn item_id(&self) -> i64 {
            self.db.projection_item_id("/a.txt").unwrap()
        }

        fn request(&self) -> Request {
            Request {
                version: EXCHANGE_VERSION,
                item_id: self.item_id(),
                viewer: false,
            }
        }
    }

    const ID: &str = "11111111-2222-3333-4444-555555555555";

    /// Writes `bytes` and records what it was asked for.
    struct Fake {
        bytes: Vec<u8>,
        error: Cell<Option<AppError>>,
        calls: Cell<usize>,
        rev: std::cell::RefCell<String>,
    }

    impl Fake {
        fn writes(bytes: &[u8]) -> Self {
            Self {
                bytes: bytes.to_vec(),
                error: Cell::new(None),
                calls: Cell::new(0),
                rev: std::cell::RefCell::new(String::new()),
            }
        }

        fn fails(error: AppError) -> Self {
            let fake = Self::writes(b"");
            fake.error.set(Some(error));
            fake
        }
    }

    impl Fetcher for Fake {
        fn fetch(&self, rev: &str, _shown: &str, target: &Path) -> AppResult<()> {
            self.calls.set(self.calls.get() + 1);
            *self.rev.borrow_mut() = rev.to_string();
            if let Some(e) = self.error.take() {
                return Err(e);
            }
            fs::write(target, &self.bytes)?;
            Ok(())
        }
    }

    #[test]
    fn a_request_is_served_from_the_rev_of_the_row_into_staging() {
        let w = world(b"hello");
        let fake = Fake::writes(b"hello");

        let outcome = serve(&w.db, &w.exchange, ID, &w.request(), &fake);

        assert_eq!(
            outcome,
            Outcome::Ok {
                content_version: String::from_utf8(
                    crate::storage::db::projection::content_version(false, &hash_of(b"hello"))
                )
                .unwrap()
            }
        );
        assert_eq!(*fake.rev.borrow(), REV);
        assert_eq!(fs::read(w.exchange.staged(ID)).unwrap(), b"hello");
    }

    /// Proof this test can fail: without the comparison the wrong bytes stay in staging and answer `Ok`.
    #[test]
    fn bytes_that_do_not_hash_to_the_row_never_reach_the_extension() {
        let w = world(b"hello");

        let outcome = serve(
            &w.db,
            &w.exchange,
            ID,
            &w.request(),
            &Fake::writes(b"other"),
        );

        assert_eq!(outcome, Outcome::Retry);
        assert!(!w.exchange.staged(ID).exists());
    }

    /// A rename split in two leaves the row a tombstone for a few seconds: the answer is a retry, not a
    /// download of a path that is gone and not a claim that the item is gone.
    #[test]
    fn a_row_that_is_not_live_is_not_fetched_and_answers_retry() {
        let w = world(b"hello");
        let request = w.request();
        w.db.project_delta(&[entry("deleted", "/a.txt", None, None)])
            .unwrap();
        let fake = Fake::writes(b"hello");

        assert_eq!(
            serve(&w.db, &w.exchange, ID, &request, &fake),
            Outcome::Retry
        );

        assert_eq!(fake.calls.get(), 0);
    }

    #[test]
    fn an_unknown_item_is_not_fetched_and_answers_retry() {
        let w = world(b"hello");
        let fake = Fake::writes(b"hello");
        let request = Request {
            version: EXCHANGE_VERSION,
            item_id: 987_654,
            viewer: false,
        };

        assert_eq!(
            serve(&w.db, &w.exchange, ID, &request, &fake),
            Outcome::Retry
        );

        assert_eq!(fake.calls.get(), 0);
    }

    #[test]
    fn each_failure_maps_to_the_class_the_extension_answers_with() {
        let cases = [
            (AppError::Network("x".into()), Outcome::Offline),
            (AppError::Auth("x".into()), Outcome::Auth),
            (
                AppError::Dropbox {
                    status: 401,
                    message: String::new(),
                },
                Outcome::Auth,
            ),
            (
                AppError::Dropbox {
                    status: 429,
                    message: String::new(),
                },
                Outcome::Retry,
            ),
            (
                AppError::Dropbox {
                    status: 409,
                    message: String::new(),
                },
                Outcome::Retry,
            ),
            (AppError::Io("x".into()), Outcome::Retry),
        ];
        for (error, expected) in cases {
            let w = world(b"hello");
            let outcome = serve(&w.db, &w.exchange, ID, &w.request(), &Fake::fails(error));
            assert_eq!(outcome, expected);
        }
    }

    fn write_request(w: &World, name: &str, body: &str) -> PathBuf {
        let path = w.exchange.requests().join(name);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn a_pending_request_is_taken_once_and_its_file_goes() {
        let w = world(b"hello");
        let path = write_request(&w, &format!("{ID}.json"), r#"{"version":1,"item_id":7}"#);

        let first = w.exchange.take_pending().unwrap();

        assert_eq!(
            first,
            vec![(
                ID.to_string(),
                Pending::Serve(Request {
                    version: 1,
                    item_id: 7,
                    viewer: false,
                })
            )]
        );
        assert!(!path.exists());
        assert!(w.exchange.take_pending().unwrap().is_empty());
    }

    #[test]
    fn a_request_being_written_is_left_for_its_rename() {
        let w = world(b"hello");
        let temp = write_request(&w, &format!(".{ID}.tmp"), r#"{"version":1,"#);

        assert!(w.exchange.take_pending().unwrap().is_empty());

        assert!(temp.exists());
    }

    /// Someone is waiting for a valid id, so an unreadable request still gets an answer.
    #[test]
    fn an_unreadable_request_under_a_valid_id_is_reported_for_an_answer() {
        let w = world(b"hello");
        for body in [r#"{"version":2,"item_id":7}"#, "not json"] {
            let path = write_request(&w, &format!("{ID}.json"), body);
            let taken = w.exchange.take_pending().unwrap();
            assert_eq!(taken, vec![(ID.to_string(), Pending::Unreadable)], "{body}");
            assert!(!path.exists(), "{body}");
        }
    }

    #[test]
    fn a_stray_file_is_removed_and_nobody_is_answered() {
        let w = world(b"hello");
        let path = write_request(&w, "escape.json", r#"{"version":1,"item_id":7}"#);

        assert!(w.exchange.take_pending().unwrap().is_empty());

        assert!(!path.exists());
    }

    /// A pass that fails halfway would lose what it already removed. A directory cannot be removed as a
    /// file, so it stands for a request that cannot be taken.
    #[test]
    fn an_entry_that_cannot_be_taken_does_not_cost_the_others() {
        let w = world(b"hello");
        fs::create_dir(w.exchange.requests().join(format!("{ID}.json"))).unwrap();
        let other = "22222222-2222-3333-4444-555555555555";
        write_request(&w, &format!("{other}.json"), r#"{"version":1,"item_id":7}"#);

        let taken = w.exchange.take_pending().unwrap();

        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].0, other);
    }

    #[test]
    fn a_panic_still_answers_the_waiter_and_leaves_nothing_staged() {
        let w = world(b"hello");
        fs::write(w.exchange.staged(ID), b"partial").unwrap();

        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = AnswerGuard {
                exchange: &w.exchange,
                id: ID,
                answered: false,
            };
            panic!("in the worker");
        }));

        assert!(unwound.is_err());
        assert!(!w.exchange.staged(ID).exists());
        let result = fs::read_to_string(w.exchange.results().join(format!("{ID}.json"))).unwrap();
        assert_eq!(result, r#"{"version":1,"outcome":"retry"}"#);
    }

    #[test]
    fn a_request_the_extension_gave_up_on_publishes_nothing_and_drops_the_bytes() {
        let w = world(b"hello");
        fs::write(w.exchange.staged(ID), b"hello").unwrap();
        fs::write(w.exchange.cancel_marker(ID), b"").unwrap();

        AnswerGuard {
            exchange: &w.exchange,
            id: ID,
            answered: false,
        }
        .answer(&Outcome::Ok {
            content_version: "c1:abc".into(),
        });

        assert!(!w.exchange.staged(ID).exists());
        assert!(!w.exchange.cancel_marker(ID).exists());
        assert!(!w.exchange.results().join(format!("{ID}.json")).exists());
    }

    #[test]
    fn leftovers_go_when_the_switch_is_off() {
        let w = world(b"hello");
        fs::write(w.exchange.staged(ID), b"hello").unwrap();

        remove_leftovers(w.exchange.root.parent().unwrap());

        assert!(!w.exchange.root.exists());
    }

    /// The log lives in the container, so a Dropbox error's message, which holds the path, stays out.
    #[test]
    fn the_class_of_an_error_carries_no_path() {
        let bare = AppError::Dropbox {
            status: 429,
            message: "download for /secret/name.txt: too many".into(),
        };
        let summarized = AppError::Dropbox {
            status: 409,
            message: PAPER_REFUSAL.into(),
        };

        assert_eq!(error_class(&bare), "dropbox 429");
        assert_eq!(error_class(&summarized), "dropbox 409 unsupported_file/");
    }

    /// The shape `fetch_revision_or_path` builds around the body Dropbox sent for a Paper document.
    const PAPER_REFUSAL: &str = r#"download for /sin título.paper: {"error_summary": "unsupported_file/", "error": {".tag": "unsupported_file"}}"#;

    #[test]
    fn a_file_dropbox_will_not_hand_out_is_refused_and_any_other_conflict_retries() {
        let refused = AppError::Dropbox {
            status: 409,
            message: PAPER_REFUSAL.into(),
        };
        let gone = AppError::Dropbox {
            status: 409,
            message: r#"download for /a.txt: {"error_summary": "path/not_found/..", "error": {}}"#
                .into(),
        };
        let named_like_it = AppError::Dropbox {
            status: 409,
            message: r#"download for /unsupported_file.txt: {"error_summary": "path/not_found/"}"#
                .into(),
        };

        assert_eq!(outcome_for_error(&refused), Outcome::Refused);
        assert_eq!(outcome_for_error(&gone), Outcome::Retry);
        assert_eq!(outcome_for_error(&named_like_it), Outcome::Retry);
    }

    #[test]
    fn only_a_uuid_is_an_id() {
        assert!(valid_id(ID));
        assert!(!valid_id("../../../../../../../../../../etc/pass"));
        assert!(!valid_id("11111111-2222-3333-4444-55555555555"));
        assert!(!valid_id("11111111x2222-3333-4444-555555555555"));
    }

    /// The shape `Exchange.swift` decodes.
    #[test]
    fn a_result_appears_whole_under_its_final_name() {
        let w = world(b"hello");

        w.exchange
            .publish(
                ID,
                &Outcome::Ok {
                    content_version: "c1:abc".into(),
                },
            )
            .unwrap();
        w.exchange
            .publish("22222222-2222-3333-4444-555555555555", &Outcome::Offline)
            .unwrap();

        let ok = fs::read_to_string(w.exchange.results().join(format!("{ID}.json"))).unwrap();
        assert_eq!(
            ok,
            r#"{"version":1,"outcome":"ok","content_version":"c1:abc"}"#
        );
        let offline = fs::read_to_string(
            w.exchange
                .results()
                .join("22222222-2222-3333-4444-555555555555.json"),
        )
        .unwrap();
        assert_eq!(offline, r#"{"version":1,"outcome":"offline"}"#);
        let names: Vec<_> = fs::read_dir(w.exchange.results())
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(names.len(), 2, "no temporary file stays behind");
    }

    #[test]
    fn the_sweep_removes_only_what_is_stale_in_every_directory() {
        let w = world(b"hello");
        let staged = w.exchange.staged(ID);
        let result = w.exchange.results().join(format!("{ID}.json"));
        let request = w.exchange.requests().join(format!(".{ID}.tmp"));
        for path in [&staged, &result, &request] {
            fs::write(path, b"x").unwrap();
        }

        let soon = SystemTime::now() + STALE_AFTER - Duration::from_secs(60);
        assert_eq!(w.exchange.sweep(soon), 0);
        assert!(staged.exists() && result.exists() && request.exists());

        let later = SystemTime::now() + STALE_AFTER + Duration::from_secs(60);
        assert_eq!(w.exchange.sweep(later), 3);
        assert!(!staged.exists() && !result.exists() && !request.exists());
    }

    /// The extension relies on this: while a host holds the lock a shared attempt fails, and once the
    /// handle is gone, as when the process dies, it succeeds.
    #[test]
    fn the_lock_is_held_by_the_handle_and_dropped_with_it() {
        let w = world(b"hello");
        let lock = w.exchange.lock().unwrap();
        let probe = File::open(w.exchange.root.join("host.lock")).unwrap();

        assert!(probe.try_lock_shared().is_err());

        drop(lock);
        assert!(probe.try_lock_shared().is_ok());
    }

    /// The shape `Exchange.swift` writes for a double click.
    #[test]
    fn a_request_says_whether_a_person_opened_the_file() {
        let w = world(b"hello");
        write_request(
            &w,
            &format!("{ID}.json"),
            r#"{"version":1,"item_id":7,"viewer":true}"#,
        );

        let taken = w.exchange.take_pending().unwrap();

        assert_eq!(
            taken,
            vec![(
                ID.to_string(),
                Pending::Serve(Request {
                    version: 1,
                    item_id: 7,
                    viewer: true,
                })
            )]
        );
    }

    #[test]
    fn the_web_url_is_the_preview_of_the_path() {
        let url = web_preview_url("/documentos de paper/🎉 intro.paper", "🎉 Intro.paper").unwrap();
        assert_eq!(
            url.as_str(),
            "https://www.dropbox.com/preview/documentos%20de%20paper/%F0%9F%8E%89%20Intro.paper"
        );

        // The URL that opened the document live.
        let root = web_preview_url("/sin título.paper", "Sin título.paper").unwrap();
        assert_eq!(
            root.as_str(),
            "https://www.dropbox.com/preview/Sin%20t%C3%ADtulo.paper"
        );

        let tricky = web_preview_url("/a?b/c#d.paper", "c#d.paper").unwrap();
        assert_eq!(
            tricky.as_str(),
            "https://www.dropbox.com/preview/a%3Fb/c%23d.paper"
        );
    }

    #[test]
    fn one_double_click_opens_one_tab() {
        let opens = BrowserOpens::new();
        let now = Instant::now();

        assert!(opens.allow(1, now));
        assert!(!opens.allow(1, now + Duration::from_secs(1)));
        assert!(opens.allow(2, now + Duration::from_secs(2)));
        assert!(opens.allow(1, now + BROWSER_REPEAT + Duration::from_secs(3)));
    }

    #[test]
    fn a_burst_of_answers_starts_one_lift() {
        let timer = ResolveTimer::new();
        let now = Instant::now();

        assert!(timer.note_answer(now));
        assert!(!timer.note_answer(now));
    }

    /// An answer just before the lift is why it counts from the last answer: the system receives that
    /// error after the moment an earlier answer's lift would have fired.
    #[test]
    fn each_answer_moves_the_lift_later() {
        let timer = ResolveTimer::new();
        let start = Instant::now();
        let second = start + RESOLVE_DELAY - Duration::from_millis(50);
        timer.note_answer(start);
        timer.note_answer(second);

        assert_eq!(
            timer.remaining(start + RESOLVE_DELAY),
            RESOLVE_DELAY - Duration::from_millis(50)
        );
    }

    #[test]
    fn a_fired_lift_lets_the_next_answer_start_another() {
        let timer = ResolveTimer::new();
        let start = Instant::now();
        timer.note_answer(start);

        assert_eq!(timer.remaining(start + RESOLVE_DELAY), Duration::ZERO);

        assert!(timer.note_answer(start + RESOLVE_DELAY));
    }
}
