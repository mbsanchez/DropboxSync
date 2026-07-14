//! Writes `overlay_state.json` for native shell integrations (Finder Sync on macOS,
//! shell icon overlay DLL on Windows). See `native/macos/FinderSyncExtension/README.md`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use serde::Serialize;

use crate::error::AppResult;
use crate::state::AppState;
use crate::storage::db;

#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverlayTier {
    /// Matches remote index; no active job for this path.
    Synced,
    /// Local and remote differ, missing remote row, or unresolved conflict.
    OutOfSync,
    /// Queued, running, or waiting retry for this path.
    Syncing,
}

/// Versioned shell **status contract** (DBSYNC-51): the single source of per-path
/// sync state read by every native surface — macOS Finder Sync badges, Linux
/// file-manager emblems, and the Windows status column. Written atomically to
/// `overlay_state.json` under [`db::app_data_dir`].
///
/// Schema (`version = 1`):
/// - `version`: contract version; bump on a breaking change.
/// - `updated_at`: RFC3339 timestamp of this snapshot.
/// - `sync_folder`: absolute sync-root path (so extensions can resolve keys).
/// - `paths`: map of `/`-relative path (under `sync_folder`) → [`OverlayTier`]
///   (`synced` | `syncing` | `out_of_sync`). A path absent from the map is
///   `unknown` (not tracked / outside the root).
#[derive(Serialize)]
struct OverlayStateFile {
    version: u32,
    updated_at: String,
    sync_folder: Option<String>,
    paths: HashMap<String, OverlayTier>,
}

fn active_job_paths(db: &db::Db) -> AppResult<HashSet<String>> {
    let jobs = db.list_recent_jobs(10_000)?;
    let mut set = HashSet::new();
    for j in jobs {
        if j.status != "queued" && j.status != "retry_wait" && j.status != "running" {
            continue;
        }
        if let Some(p) = j.target_path.as_ref() {
            if !p.is_empty() {
                set.insert(p.clone());
            }
        }
        if let Some(p) = j.source_path.as_ref() {
            if !p.is_empty() {
                set.insert(p.clone());
            }
        }
    }
    Ok(set)
}

fn unresolved_conflict_paths(db: &db::Db) -> AppResult<HashSet<String>> {
    Ok(db.list_unresolved_conflict_local_paths()?.into_iter().collect())
}

/// Recomputes per-file overlay tiers and atomically writes `overlay_state.json` under [`db::app_data_dir`].
pub(crate) fn refresh_overlay_state_internal(state: &AppState) {
    if let Err(e) = refresh_overlay_state_inner(state) {
        tracing::error!(error = %e, "refresh overlay state failed");
    }
}

fn refresh_overlay_state_inner(state: &AppState) -> AppResult<()> {
    let sync_folder = state.db.get_sync_folder()?;
    let job_paths = active_job_paths(&state.db)?;
    let conflict_paths = unresolved_conflict_paths(&state.db)?;
    let locals = state.db.list_local_files()?;

    let mut paths: HashMap<String, OverlayTier> = HashMap::new();

    for row in locals {
        if row.relative_path.ends_with(".cloudsc") {
            continue;
        }

        let tier = if job_paths.contains(&row.relative_path) {
            OverlayTier::Syncing
        } else if conflict_paths.contains(&row.relative_path) {
            OverlayTier::OutOfSync
        } else {
            match state.db.get_remote_file(&row.relative_path)? {
                None => OverlayTier::OutOfSync,
                Some(remote) => {
                    if row.hash == remote.content_hash {
                        OverlayTier::Synced
                    } else {
                        OverlayTier::OutOfSync
                    }
                }
            }
        };
        paths.insert(row.relative_path, tier);
    }

    let payload = OverlayStateFile {
        version: 1,
        updated_at: chrono::Utc::now().to_rfc3339(),
        sync_folder,
        paths,
    };

    let dir = db::app_data_dir()?;
    let dest = overlay_state_path(&dir);
    let json = serde_json::to_string_pretty(&payload)?;

    let tmp = dest.with_extension("json.tmp");
    fs::write(&tmp, json.as_bytes())?;
    fs::rename(&tmp, &dest)?;

    // DBSYNC-41: drive Explorer's built-in cloud "Statut" column (CfAPI) from the
    // same per-file state — no second source of truth. Fails soft (no package
    // identity / non-NTFS). `.cloudsc` sidecars are skipped (blank by design).
    #[cfg(windows)]
    {
        let items: Vec<(&str, bool)> = payload
            .paths
            .iter()
            .map(|(rel, tier)| (rel.as_str(), *tier == OverlayTier::Synced))
            .collect();
        crate::cloud_filter::sync_placeholder_states(payload.sync_folder.as_deref(), &items);
    }
    Ok(())
}

pub fn overlay_state_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("overlay_state.json")
}
