use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};
use crate::models::CloudscMeta;
use crate::path_util::fold;

pub(crate) const CLOUDSC_MAGIC: &str = "CLOUDSC1";

pub(crate) fn encode_cloudsc_meta(meta: &CloudscMeta) -> AppResult<Vec<u8>> {
    let json = serde_json::to_vec(meta)
        .map_err(|e| AppError::Other(format!("cloudsc encode json failed: {e}")))?;
    let mut out = Vec::new();
    out.extend_from_slice(CLOUDSC_MAGIC.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(&json);
    Ok(out)
}

pub(crate) fn decode_cloudsc_meta(bytes: &[u8]) -> AppResult<CloudscMeta> {
    let magic = CLOUDSC_MAGIC.as_bytes();
    if bytes.len() < magic.len() + 1 {
        return Err(AppError::Other(
            "invalid .cloudsc payload (too small)".to_string(),
        ));
    }
    if &bytes[..magic.len()] != magic {
        return Err(AppError::Other(
            "invalid .cloudsc payload (bad magic)".to_string(),
        ));
    }
    let json_bytes = &bytes[magic.len() + 1..];
    serde_json::from_slice(json_bytes)
        .map_err(|e| AppError::Other(format!("cloudsc decode json failed: {e}")))
}

pub(crate) fn cloudsc_target_path(placeholder_path: &Path) -> PathBuf {
    placeholder_path.with_extension("")
}

pub(crate) fn write_cloudsc_placeholder_file(
    placeholder_path: &Path,
    meta: &CloudscMeta,
) -> AppResult<()> {
    if let Some(parent) = placeholder_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| AppError::Io(format!("failed creating placeholder parent dir: {e}")))?;
    }
    let payload = encode_cloudsc_meta(meta)?;
    fs::write(placeholder_path, payload)
        .map_err(|e| AppError::Io(format!("failed writing placeholder file: {e}")))?;
    Ok(())
}

pub(crate) fn read_cloudsc_placeholder_file(placeholder_path: &Path) -> AppResult<CloudscMeta> {
    let bytes = fs::read(placeholder_path)
        .map_err(|e| AppError::Io(format!("failed reading placeholder file bytes: {e}")))?;
    decode_cloudsc_meta(&bytes)
}

/// What the `.cloudsc` sidecars inside a folder say about a vanished folder (DBSYNC-111).
///
/// A folder that holds only placeholders carries no tracked files, so a Finder rename of it
/// cannot be recognised by the file correlator. The sidecars are the evidence instead: each one
/// names the Dropbox path it stands for, so a renamed folder is one whose sidecars all still
/// name the OLD folder, at the same position inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidecarEvidence {
    /// At least one readable sidecar, and every readable sidecar points at the old folder plus
    /// the same relative tail it has here. The count is the number of sidecars that agreed.
    AllPointInto(usize),
    /// At least one sidecar points into the old folder but the readable sidecars do not all
    /// agree (a different tail, or a sidecar for somewhere else). Never a rename; and because a
    /// sidecar does point into the old folder, that folder must not be deleted either.
    Mismatch,
    /// No readable sidecar points into the old folder: this folder says nothing about it.
    None,
}

/// Every `.cloudsc` regular file under `root` (real subfolders included, symlinks not followed)
/// with its tail: the path from `root`, `/`-separated, without the `.cloudsc` suffix.
fn sidecars_under(root: &Path) -> impl Iterator<Item = (PathBuf, String)> + '_ {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(move |e| {
            let rel = e.path().strip_prefix(root).ok()?;
            let tail = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let tail = tail.strip_suffix(".cloudsc")?.to_string();
            Some((e.path().to_path_buf(), tail))
        })
}

/// Classify `(tail, folded remote)` pairs against the folded `old_remote` prefix.
fn classify_sidecars<'a>(
    items: impl Iterator<Item = (String, &'a str)>,
    old_remote: &str,
) -> SidecarEvidence {
    let into_prefix = format!("{}/", fold(old_remote.trim_end_matches('/')));
    let (mut matched, mut other_into, mut foreign) = (0usize, 0usize, 0usize);
    for (tail, remote) in items {
        if *remote == format!("{into_prefix}{}", fold(&tail)) {
            matched += 1;
        } else if remote.starts_with(&into_prefix) {
            other_into += 1;
        } else {
            foreign += 1;
        }
    }
    match (matched, other_into, foreign) {
        (0, 0, _) => SidecarEvidence::None,
        (n, 0, 0) => SidecarEvidence::AllPointInto(n),
        _ => SidecarEvidence::Mismatch,
    }
}

/// Every readable sidecar under a root, read ONCE: its path and the folded Dropbox path it
/// names. A scan with several vanished placeholder-only folders asks many questions of the
/// same sidecars; this answers all of them from one walk of the sync root.
#[derive(Debug, Clone)]
pub(crate) struct SidecarIndex {
    entries: Vec<(PathBuf, String)>,
}

impl SidecarIndex {
    /// Walk `root` and read every sidecar (unreadable ones are skipped).
    pub(crate) fn scan(root: &Path) -> Self {
        #[cfg(test)]
        SIDECAR_ROOT_WALKS.with(|c| c.set(c.get() + 1));
        let entries = sidecars_under(root)
            .filter_map(|(path, _)| {
                let meta = read_cloudsc_placeholder_file(&path).ok()?;
                Some((path, fold(&meta.remote_path_display)))
            })
            .collect();
        Self { entries }
    }

    /// Does any sidecar point into the folder `old_remote`?
    pub(crate) fn any_points_into(&self, old_remote: &str) -> bool {
        let into_prefix = format!("{}/", fold(old_remote.trim_end_matches('/')));
        self.entries
            .iter()
            .any(|(_, r)| r.starts_with(&into_prefix))
    }

    /// Every directory strictly below `root` that holds (at any depth) a sidecar pointing into
    /// `old_remote`: the places a renamed copy of that folder could be.
    pub(crate) fn dirs_holding_sidecars_into(&self, old_remote: &str, root: &Path) -> Vec<PathBuf> {
        let into_prefix = format!("{}/", fold(old_remote.trim_end_matches('/')));
        let mut out: Vec<PathBuf> = Vec::new();
        for (path, remote) in &self.entries {
            if !remote.starts_with(&into_prefix) {
                continue;
            }
            for dir in path.ancestors().skip(1) {
                if dir == root || !dir.starts_with(root) {
                    break;
                }
                if !out.iter().any(|d| d == dir) {
                    out.push(dir.to_path_buf());
                }
            }
        }
        out
    }

    /// [`sidecar_evidence`] for the sidecars under `dir`, answered from the index.
    pub(crate) fn evidence(&self, dir: &Path, old_remote: &str) -> SidecarEvidence {
        classify_sidecars(
            self.entries
                .iter()
                .filter(|(p, _)| p.starts_with(dir))
                .filter_map(|(p, r)| {
                    let rel = p.strip_prefix(dir).ok()?;
                    let tail = rel
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/");
                    Some((tail.strip_suffix(".cloudsc")?.to_string(), r.as_str()))
                }),
            old_remote,
        )
    }
}

/// Classify the sidecars under `new_abs` against the folder `old_remote` (a Dropbox path such as
/// `/Old`). Pure path arithmetic plus one read per sidecar; folded comparison (case and
/// normalization insensitive, like Dropbox). Unreadable sidecars are ignored.
pub(crate) fn sidecar_evidence(new_abs: &Path, old_remote: &str) -> SidecarEvidence {
    SidecarIndex::scan(new_abs).evidence(new_abs, old_remote)
}

/// `remote` with the folder prefix `old_remote` replaced by `new_remote`, segment by segment
/// (folded compare, so a case or normalization difference still matches and the rest keeps its
/// own spelling). `None` when `remote` is not inside `old_remote`.
fn rebase_remote_path(remote: &str, old_remote: &str, new_remote: &str) -> Option<String> {
    let old_segs: Vec<&str> = old_remote.trim_matches('/').split('/').collect();
    let segs: Vec<&str> = remote.trim_start_matches('/').split('/').collect();
    if old_segs.iter().all(|s| s.is_empty()) || segs.len() < old_segs.len() {
        return None;
    }
    if !old_segs.iter().zip(&segs).all(|(a, b)| fold(a) == fold(b)) {
        return None;
    }
    let mut out = new_remote.trim_end_matches('/').to_string();
    for seg in &segs[old_segs.len()..] {
        out.push('/');
        out.push_str(seg);
    }
    Some(out)
}

/// After Dropbox confirmed a folder move, point every sidecar under the folder's new location
/// at the new Dropbox path. Sidecars pointing elsewhere and unreadable ones are left alone; a
/// sidecar that cannot be rewritten is logged, not fatal (the move has already happened).
/// Returns how many were rewritten. No network.
pub(crate) fn rewrite_sidecars_after_move(
    new_abs: &Path,
    old_remote: &str,
    new_remote: &str,
) -> usize {
    let mut rewritten = 0usize;
    for (path, _) in sidecars_under(new_abs) {
        let Ok(mut meta) = read_cloudsc_placeholder_file(&path) else {
            continue;
        };
        let Some(rebased) = rebase_remote_path(&meta.remote_path_display, old_remote, new_remote)
        else {
            continue;
        };
        if rebased == meta.remote_path_display {
            continue;
        }
        meta.remote_path_display = rebased;
        match write_cloudsc_placeholder_file(&path, &meta) {
            Ok(()) => rewritten += 1,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not rewrite a sidecar after a confirmed folder move");
            }
        }
    }
    rewritten
}

#[cfg(test)]
thread_local! {
    /// Test-only: how many whole-root sidecar walks this thread has done.
    pub(crate) static SIDECAR_ROOT_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
