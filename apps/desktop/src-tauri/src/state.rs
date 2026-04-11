use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::storage::db::Db;
use crate::storage::secure_store::SecureStore;
use crate::sync::engine::SyncEngine;

/// Stops the localhost OAuth redirect listener and joins its thread (releases the TCP port).
pub(crate) struct OauthListenerControl {
    pub cancel: Arc<AtomicBool>,
    pub handle: JoinHandle<()>,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub secure_store: SecureStore,
    pub db: Arc<Db>,
    pub sync_engine: Arc<Mutex<SyncEngine>>,
    pub token_cache: Arc<Mutex<Option<crate::storage::secure_store::TokenSession>>>,
    pub scheduler_started: Arc<Mutex<bool>>,
    pub oauth_listener: Arc<Mutex<Option<OauthListenerControl>>>,
}
