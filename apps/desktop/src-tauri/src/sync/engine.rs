use serde::Serialize;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub health: String,
    pub queue_depth: usize,
    pub tracked_path: Option<String>,
    pub last_error: Option<String>,
    pub last_scan_at: Option<String>,
    pub processed_jobs: usize,
    pub conflicts_detected: usize,
    pub sync_running: bool,
}

pub struct SyncEngine {
    pending_oauth_state: Option<String>,
    pending_pkce_verifier: Option<String>,
    tracked_path: Option<String>,
    queue_depth: usize,
    last_error: Option<String>,
    last_scan_at: Option<String>,
    processed_jobs: usize,
    conflicts_detected: usize,
}

impl SyncEngine {
    pub fn new() -> Self {
        Self {
            pending_oauth_state: None,
            pending_pkce_verifier: None,
            tracked_path: None,
            queue_depth: 0,
            last_error: None,
            last_scan_at: None,
            processed_jobs: 0,
            conflicts_detected: 0,
        }
    }

    pub fn set_oauth_context(&mut self, state: String, verifier: String) {
        self.pending_oauth_state = Some(state);
        self.pending_pkce_verifier = Some(verifier);
    }

    pub fn pending_oauth_state(&self) -> Option<String> {
        self.pending_oauth_state.clone()
    }

    pub fn pending_pkce_verifier(&self) -> Option<String> {
        self.pending_pkce_verifier.clone()
    }

    /// Clears in-flight OAuth state (e.g. user cancelled or will retry login).
    pub fn clear_oauth_pending(&mut self) {
        self.pending_oauth_state = None;
        self.pending_pkce_verifier = None;
    }

    pub fn set_tracked_path(&mut self, path: String) {
        self.tracked_path = Some(path);
    }

    pub fn set_queue_depth(&mut self, queue_depth: usize) {
        self.queue_depth = queue_depth;
    }

    pub fn set_last_scan_at(&mut self, ts: String) {
        self.last_scan_at = Some(ts);
    }

    pub fn record_job_processed(&mut self) {
        self.processed_jobs += 1;
    }

    pub fn record_conflict(&mut self) {
        self.conflicts_detected += 1;
    }

    pub fn set_last_error(&mut self, message: String) {
        self.last_error = Some(message);
    }

    pub fn clear_last_error(&mut self) {
        self.last_error = None;
    }

    pub fn current_status(&self, sync_running: bool) -> SyncStatus {
        SyncStatus {
            health: if self.last_error.is_some() {
                "error".to_string()
            } else if self.queue_depth > 0 {
                "syncing".to_string()
            } else {
                "idle".to_string()
            },
            queue_depth: self.queue_depth,
            tracked_path: self.tracked_path.clone(),
            last_error: self.last_error.clone(),
            last_scan_at: self.last_scan_at.clone(),
            processed_jobs: self.processed_jobs,
            conflicts_detected: self.conflicts_detected,
            sync_running,
        }
    }
}
