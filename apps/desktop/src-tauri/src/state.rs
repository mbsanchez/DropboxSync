use std::sync::{Arc, Mutex};

use crate::storage::db::Db;
use crate::storage::secure_store::SecureStore;
use crate::sync::engine::SyncEngine;

#[derive(Clone)]
pub(crate) struct AppState {
    pub secure_store: SecureStore,
    pub db: Arc<Db>,
    pub sync_engine: Arc<Mutex<SyncEngine>>,
    pub token_cache: Arc<Mutex<Option<crate::storage::secure_store::TokenSession>>>,
    pub scheduler_started: Arc<Mutex<bool>>,
}
