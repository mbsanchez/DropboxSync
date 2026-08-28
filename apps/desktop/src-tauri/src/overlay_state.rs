//! Writes `overlay_state.json` for native shell integrations (Finder Sync on macOS,
//! shell icon overlay DLL on Windows). See `native/macos/FinderSyncExtension/README.md`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use serde::Serialize;

use crate::error::AppResult;
use crate::state::AppState;
use crate::storage::db;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverlayTier {
    /// Matches remote index; no active job for this path.
    Synced,
    /// Local and remote differ, missing remote row, or unresolved conflict.
    OutOfSync,
    /// Queued, running, or waiting retry for this path.
    Syncing,
    /// A `.cloudsc` sidecar: the content lives only in the cloud and takes no local
    /// disk. Deliberately NOT `Synced` — the file is in step with the remote, but
    /// painting the same badge as a downloaded copy would hide the one fact the user
    /// cannot otherwise see. Never emitted on Windows, where CfAPI placeholders are the
    /// real files and no sidecar exists (DBSYNC-80).
    CloudOnly,
}

/// Versioned shell **status contract** (DBSYNC-51): the single source of per-path
/// sync state read by every native surface — macOS Finder Sync badges, Linux
/// file-manager emblems, and the Windows status column. Written atomically to
/// `overlay_state.json` beside the database that produced it (DBSYNC-75).
///
/// Schema (`version = 1`):
/// - `version`: contract version; bump on a breaking change.
/// - `updated_at`: RFC3339 timestamp of this snapshot.
/// - `sync_folder`: absolute sync-root path (so extensions can resolve keys).
/// - `paths`: map of `/`-relative path (under `sync_folder`) → [`OverlayTier`]
///   (`synced` | `syncing` | `out_of_sync` | `cloud_only`). A path absent from the map
///   is `unknown` (not tracked / outside the root).
///
/// `cloud_only` was added by DBSYNC-80 without bumping `version`: it is purely
/// additive, and a reader that does not know the value finds no badge registered for it
/// and renders nothing — exactly what it did before the value existed. The bump rule
/// above is for changes that would make an existing reader wrong, not merely incomplete.
///
/// **Bumping `version` means changing both readers in the same commit (DBSYNC-91):**
/// - `native/macos/FinderSyncExtension/BadgeDiff.swift` — `OverlayState.supportedVersion`.
///   It refuses anything else outright: no badges, one log line. Leaving it behind does not
///   degrade macOS, it turns macOS off.
/// - `native/windows/shell-menu/src/scope.rs` — reads only `sync_folder`, draws no badges,
///   and has no version guard by decision, not by oversight. It cannot show a wrong badge,
///   and a `sync_folder` it fails to find already costs nothing worse than a missing
///   context menu. If a bump moves or renames that field, this is where it bites, silently.
#[derive(Serialize)]
struct OverlayStateFile {
    version: u32,
    updated_at: String,
    sync_folder: Option<String>,
    paths: HashMap<String, OverlayTier>,
}

fn active_job_paths(db: &db::Db) -> AppResult<HashSet<String>> {
    // DBSYNC-31: single indexed SQL query instead of scanning list_recent_jobs(10_000).
    db.active_job_paths()
}

fn unresolved_conflict_paths(db: &db::Db) -> AppResult<HashSet<String>> {
    Ok(db
        .list_unresolved_conflict_local_paths()?
        .into_iter()
        .collect())
}

/// Recomputes per-file overlay tiers and atomically writes `overlay_state.json` under [`db::app_data_dir`].
pub(crate) fn refresh_overlay_state_internal(state: &AppState) {
    if let Err(e) = refresh_overlay_state_inner(state) {
        tracing::error!(error = %e, "refresh overlay state failed");
    }
}

/// Per-path tiers for the current state — the whole decision, with no I/O of its own
/// beyond reading the database and the sync folder.
///
/// Split out of [`refresh_overlay_state_inner`] so it can be tested directly. That was
/// originally a workaround: the writer resolved its destination globally, so any test
/// calling it clobbered the running user's `overlay_state.json`. Fixed in DBSYNC-75 — the
/// writer now follows the database — so the split is no longer load-bearing, but it is
/// kept: a pure decision function is worth having on its own terms.
fn compute_overlay_paths(state: &AppState) -> AppResult<HashMap<String, OverlayTier>> {
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

    // Placeholders are invisible to the loop above: they are not rows in
    // `local_file_index` at all, so nothing to skip and nothing to rewrite — they have
    // to be enumerated from disk. Windows is excluded because a CfAPI placeholder IS the
    // real file there; the `.cloudsc` skip above and the blank status column below are
    // deliberate on that platform (DBSYNC-41) and must stay byte-identical.
    #[cfg(not(windows))]
    {
        for rel in crate::cloudsc_ops::list_cloudsc_placeholder_rels(state)? {
            // An active job wins: a placeholder being hydrated should read as `syncing`,
            // not as still-online-only. `job_paths` is keyed by the same on-disk
            // relative path.
            let tier = if job_paths.contains(&rel) {
                OverlayTier::Syncing
            } else {
                OverlayTier::CloudOnly
            };
            paths.insert(rel, tier);
        }
    }

    Ok(paths)
}

fn refresh_overlay_state_inner(state: &AppState) -> AppResult<()> {
    let sync_folder = state.db.get_sync_folder()?;
    let paths = compute_overlay_paths(state)?;

    let payload = OverlayStateFile {
        version: 1,
        updated_at: chrono::Utc::now().to_rfc3339(),
        sync_folder,
        paths,
    };

    // Beside the database, NOT via the global `app_data_dir()` (DBSYNC-75). In production
    // these are the same directory. Under `cargo test` they are not: the harness builds its
    // `AppState` on a `tempdir()`, and resolving globally here overwrote the running user's
    // real `overlay_state.json` with a sync folder the test then deleted — leaving the
    // Finder Sync extension pointed at nothing and every badge gone until it was repaired.
    let dest = overlay_state_path(state.db.data_dir());
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
        // DBSYNC-59: give real directories a placeholder identity too, so their status
        // column shows the aggregate (cloud when all children are online-only) instead
        // of a perpetual "syncing" glyph. Files are placeholders; folders were plain dirs.
        let folder_rels = state.db.list_known_folders().unwrap_or_default();
        crate::cloud_filter::sync_folder_states(payload.sync_folder.as_deref(), &folder_rels);
    }
    Ok(())
}

pub fn overlay_state_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("overlay_state.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    fn build_state(sync_folder: &Path) -> AppState {
        std::fs::create_dir_all(sync_folder).expect("create sync folder");
        let dbdir = tempfile::tempdir().expect("db tempdir");
        let db_path = dbdir.path().join("app.db");
        std::mem::forget(dbdir);
        let db = crate::storage::db::Db::new_at(&db_path).expect("db");
        db.set_sync_folder(&sync_folder.to_string_lossy())
            .expect("set folder");
        AppState {
            secure_store: crate::storage::secure_store::SecureStore::new(),
            db: Arc::new(db),
            sync_engine: Arc::new(Mutex::new(crate::sync::engine::SyncEngine::new())),
            token_cache: Arc::new(Mutex::new(None)),
            scheduler_started: Arc::new(Mutex::new(false)),
            oauth_listener: Arc::new(Mutex::new(None)),
            sync_running: Arc::new(AtomicBool::new(false)),
            token_refresh_lock: Arc::new(Mutex::new(())),
            http_client: crate::state::build_http_client(),
        }
    }

    fn write_placeholder(sync: &Path, rel: &str) {
        let abs = sync.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&abs, b"CLOUDSC1 placeholder").expect("write placeholder");
    }

    /// The whole point of DBSYNC-80: a sidecar has no row in `local_file_index`, so it can
    /// only reach the map by being walked off disk.
    #[cfg(not(windows))]
    #[test]
    fn cloudsc_placeholder_is_emitted_as_cloud_only() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("sync");
        let state = build_state(&sync);
        write_placeholder(&sync, "UNET/Planilla.docx.cloudsc");

        let paths = compute_overlay_paths(&state).expect("compute");

        assert_eq!(
            paths.get("UNET/Planilla.docx.cloudsc"),
            Some(&OverlayTier::CloudOnly)
        );
    }

    /// Finder derives the key from the on-disk name, so the suffix must survive. Stripping
    /// it was tried once and produced a key nothing ever looks up.
    #[cfg(not(windows))]
    #[test]
    fn cloudsc_key_keeps_its_suffix_and_the_logical_name_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("sync");
        let state = build_state(&sync);
        write_placeholder(&sync, "Planilla.docx.cloudsc");

        let paths = compute_overlay_paths(&state).expect("compute");

        assert!(paths.contains_key("Planilla.docx.cloudsc"));
        assert!(
            !paths.contains_key("Planilla.docx"),
            "the logical name must not be a key: Finder never asks for it"
        );
    }

    /// A dehydrated directory is itself a regular file holding the placeholder record, so
    /// it must be badged like any other sidecar.
    #[cfg(not(windows))]
    #[test]
    fn dehydrated_folder_placeholder_is_included() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("sync");
        let state = build_state(&sync);
        write_placeholder(&sync, "UNET/Ascensos.cloudsc");

        let paths = compute_overlay_paths(&state).expect("compute");

        assert_eq!(
            paths.get("UNET/Ascensos.cloudsc"),
            Some(&OverlayTier::CloudOnly)
        );
    }

    /// While it is being hydrated the placeholder is in motion, and that beats "online
    /// only" — otherwise the badge would sit still through the whole download.
    #[cfg(not(windows))]
    #[test]
    fn placeholder_with_an_active_job_reads_as_syncing() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("sync");
        let state = build_state(&sync);
        write_placeholder(&sync, "Planilla.docx.cloudsc");
        state
            .db
            .enqueue_job("hydrate_cloudsc", None, Some("Planilla.docx.cloudsc"))
            .expect("enqueue");

        let paths = compute_overlay_paths(&state).expect("compute");

        assert_eq!(
            paths.get("Planilla.docx.cloudsc"),
            Some(&OverlayTier::Syncing)
        );
    }

    /// The skip in the `local_file_index` loop is deliberate and stays: on Windows the
    /// CfAPI placeholder IS the real file and the sidecar is noise. This locks in that a
    /// stray indexed sidecar never yields `Synced` — on every platform. Where the walk
    /// runs it owns the key instead, and reports `CloudOnly`.
    #[test]
    fn indexed_cloudsc_row_never_yields_synced() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("sync");
        let state = build_state(&sync);
        state
            .db
            .upsert_local_file("Planilla.docx.cloudsc", "deadbeef", 42, 0)
            .expect("upsert local");
        state
            .db
            .upsert_remote_file("Planilla.docx.cloudsc", "deadbeef", "rev1", 0)
            .expect("upsert remote");

        let paths = compute_overlay_paths(&state).expect("compute");

        assert_ne!(
            paths.get("Planilla.docx.cloudsc"),
            Some(&OverlayTier::Synced),
            "a hash-matching sidecar row must not be reported as a downloaded, in-sync file"
        );
    }

    /// A real tracked file is unaffected by any of the above.
    #[test]
    fn real_file_matching_remote_is_still_synced() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("sync");
        let state = build_state(&sync);
        state
            .db
            .upsert_local_file("Anteproyecto.docx", "abc123", 10, 0)
            .expect("upsert local");
        state
            .db
            .upsert_remote_file("Anteproyecto.docx", "abc123", "rev1", 0)
            .expect("upsert remote");

        let paths = compute_overlay_paths(&state).expect("compute");

        assert_eq!(paths.get("Anteproyecto.docx"), Some(&OverlayTier::Synced));
    }

    /// The guard for DBSYNC-75, and the only test here that exercises the **writer**.
    ///
    /// Before the fix this function resolved its destination with `db::app_data_dir()`, so
    /// running it under `cargo test` overwrote the running user's real `overlay_state.json`
    /// with a sync folder pointing at a tempdir the test then deleted. The Finder Sync
    /// extension reads that file every two seconds, so the user's badges silently stopped
    /// rendering — and the next person to validate badges by hand would have been debugging
    /// the wrong thing entirely.
    ///
    /// Asserting the destination lands under the tempdir is what makes the regression
    /// impossible to reintroduce quietly: resolving globally again puts the file somewhere
    /// else, and this fails. It deliberately does NOT call `db::app_data_dir()` to compare
    /// against — that function creates the directory, and a test for "never touch the real
    /// data dir" must not touch the real data dir.
    #[test]
    fn the_writer_follows_the_database_and_never_the_global_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let sync = tmp.path().join("sync");
        let state = build_state(&sync);

        refresh_overlay_state_inner(&state).expect("refresh");

        let dest = overlay_state_path(state.db.data_dir());
        assert!(
            dest.is_file(),
            "no overlay file beside the database at {}",
            dest.display()
        );

        // The database lives in its own tempdir (see `build_state`), which is not this
        // test's `tmp` — so the check is "somewhere temporary", not "under this handle".
        let written = std::fs::read_to_string(&dest).expect("read written overlay");
        assert!(
            written.contains(&sync.to_string_lossy().replace('\\', "\\\\")),
            "the written file should describe this test's sync folder, got: {written}"
        );
    }

    /// No sync folder configured must be an empty map, not an error: the overlay refresh
    /// runs unattended from the sync queue and startup, before onboarding completes.
    #[test]
    fn no_sync_folder_configured_yields_no_paths() {
        let dbdir = tempfile::tempdir().expect("db tempdir");
        let db = crate::storage::db::Db::new_at(&dbdir.path().join("app.db")).expect("db");
        let state = AppState {
            secure_store: crate::storage::secure_store::SecureStore::new(),
            db: Arc::new(db),
            sync_engine: Arc::new(Mutex::new(crate::sync::engine::SyncEngine::new())),
            token_cache: Arc::new(Mutex::new(None)),
            scheduler_started: Arc::new(Mutex::new(false)),
            oauth_listener: Arc::new(Mutex::new(None)),
            sync_running: Arc::new(AtomicBool::new(false)),
            token_refresh_lock: Arc::new(Mutex::new(())),
            http_client: crate::state::build_http_client(),
        };

        let paths = compute_overlay_paths(&state).expect("compute");

        assert!(paths.is_empty());
    }
}
