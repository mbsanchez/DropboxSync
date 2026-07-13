use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};

pub(crate) fn should_ignore_local_path(relative: &str) -> bool {
    let p = relative.replace('\\', "/");
    p == ".DS_Store" || p.ends_with("/.DS_Store") || p.starts_with("._") || p.contains("/._")
}

pub(crate) fn relpath_under(sync_folder: &Path, absolute: &Path) -> AppResult<String> {
    Ok(absolute
        .strip_prefix(sync_folder)
        .map_err(|e| AppError::Other(format!("failed to compute relative path: {e}")))?
        .to_string_lossy()
        // Canonicalize to '/' (Dropbox convention) so relative-path keys match
        // across the local index, remote index and placeholder logic on Windows,
        // where `to_string_lossy` yields '\' separators (DBSYNC-45).
        .replace('\\', "/"))
}

pub(crate) fn normalize_dropbox_path(input: &str) -> String {
    if input.is_empty() {
        return "".to_string();
    }
    // Windows relative paths use `\` separators; Dropbox requires `/`.
    let forward = input.replace('\\', "/");
    if forward.starts_with('/') {
        forward
    } else {
        format!("/{}", forward)
    }
}

pub(crate) fn parse_prefix_csv(csv: Option<String>) -> Vec<String> {
    match csv {
        None => Vec::new(),
        Some(s) => s
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| p.trim_start_matches('/').to_string())
            .collect(),
    }
}

pub(crate) fn is_path_allowed(
    relative: &str,
    include_prefixes: &[String],
    exclude_prefixes: &[String],
) -> bool {
    let included = if include_prefixes.is_empty() {
        true
    } else {
        include_prefixes.iter().any(|p| relative.starts_with(p))
    };

    if !included {
        return false;
    }

    if exclude_prefixes.iter().any(|p| relative.starts_with(p)) {
        return false;
    }

    true
}

/// Block size used by the Dropbox content_hash algorithm (4 MiB).
const DROPBOX_BLOCK_SIZE: usize = 4 * 1024 * 1024;

/// Compute a [Dropbox content_hash](https://www.dropbox.com/developers/reference/content-hash)
/// for the file at `path`.
///
/// The algorithm:
/// 1. Split the file into 4 MiB blocks (last block may be smaller).
/// 2. SHA-256 each block individually.
/// 3. Concatenate the raw 32-byte block digests.
/// 4. SHA-256 the concatenation.
/// 5. Return the result as a lowercase hex string.
///
/// Returns `(content_hash_hex, file_size_bytes, modified_unix_timestamp)`.
///
/// # Errors
///
/// Returns an error string if the file cannot be opened, read, or stat-ed.
pub(crate) fn hash_file(path: &Path) -> AppResult<(String, i64, i64)> {
    let mut file =
        File::open(path).map_err(|e| AppError::Io(format!("cannot open file for hash: {e}")))?;
    let mut buffer = [0_u8; 8192];

    // Accumulates the raw 32-byte SHA-256 digest of each 4 MiB block.
    let mut block_digests: Vec<u8> = Vec::new();
    let mut block_hasher = Sha256::new();
    let mut bytes_in_block: usize = 0;

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }

        let mut remaining = &buffer[..read];
        while !remaining.is_empty() {
            let space_in_block = DROPBOX_BLOCK_SIZE - bytes_in_block;
            let to_consume = remaining.len().min(space_in_block);
            block_hasher.update(&remaining[..to_consume]);
            bytes_in_block += to_consume;
            remaining = &remaining[to_consume..];

            if bytes_in_block == DROPBOX_BLOCK_SIZE {
                // Finalise this block and start the next one.
                let digest = block_hasher.finalize_reset();
                block_digests.extend_from_slice(&digest);
                bytes_in_block = 0;
            }
        }
    }

    // Finalise the trailing partial block (if any bytes were buffered).
    // An empty file has zero blocks: block_digests stays empty, and
    // SHA-256(b"") = e3b0c44298fc1c149afbf4c8996fb924... — which is the
    // correct Dropbox content_hash for an empty file.
    if bytes_in_block > 0 {
        let digest = block_hasher.finalize();
        block_digests.extend_from_slice(&digest);
    }

    // SHA-256 the concatenation of all block digests.
    let content_hash = Sha256::digest(&block_digests);

    let metadata = file.metadata()?;
    let size = metadata.len() as i64;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|ts| ts.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    Ok((format!("{:x}", content_hash), size, modified))
}

pub(crate) fn create_conflicted_copy(path: &Path) -> AppResult<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Other("file has no parent for conflict copy".to_string()))?;
    let stem = path
        .file_stem()
        .ok_or_else(|| AppError::Other("missing file stem".to_string()))?
        .to_string_lossy();
    let ext = path.extension().map(|e| e.to_string_lossy().to_string());
    let ts = Utc::now().format("%Y%m%d%H%M%S");

    let name = match ext {
        Some(ext) => format!("{} (conflicted copy {}).{}", stem, ts, ext),
        None => format!("{} (conflicted copy {})", stem, ts),
    };

    let dest = parent.join(name);
    fs::copy(path, &dest)
        .map_err(|e| AppError::Io(format!("failed to create conflicted copy: {e}")))?;
    Ok(dest)
}

pub(crate) fn backoff_seconds(attempt: i64) -> i64 {
    let safe_attempt = attempt.clamp(1, 10) as u32;
    2_i64.pow(safe_attempt)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use sha2::{Digest, Sha256};
    use tempfile::NamedTempFile;

    use super::{backoff_seconds, hash_file, DROPBOX_BLOCK_SIZE};

    // ---------------------------------------------------------------------------
    // Helper: compute the Dropbox content_hash for an in-memory byte slice so
    // that tests can build the expected value without touching disk.
    // ---------------------------------------------------------------------------
    fn expected_content_hash(data: &[u8]) -> String {
        let mut block_digests: Vec<u8> = Vec::new();
        let mut block_hasher = Sha256::new();
        let mut bytes_in_block: usize = 0;

        let mut remaining = data;
        while !remaining.is_empty() {
            let space_in_block = DROPBOX_BLOCK_SIZE - bytes_in_block;
            let to_consume = remaining.len().min(space_in_block);
            block_hasher.update(&remaining[..to_consume]);
            bytes_in_block += to_consume;
            remaining = &remaining[to_consume..];

            if bytes_in_block == DROPBOX_BLOCK_SIZE {
                let digest = block_hasher.finalize_reset();
                block_digests.extend_from_slice(&digest);
                bytes_in_block = 0;
            }
        }
        if bytes_in_block > 0 {
            let digest = block_hasher.finalize();
            block_digests.extend_from_slice(&digest);
        }
        format!("{:x}", Sha256::digest(&block_digests))
    }

    // ---------------------------------------------------------------------------
    // Existing test
    // ---------------------------------------------------------------------------

    #[test]
    fn backoff_grows_exponentially() {
        assert_eq!(backoff_seconds(1), 2);
        assert_eq!(backoff_seconds(2), 4);
        assert_eq!(backoff_seconds(3), 8);
    }

    // ---------------------------------------------------------------------------
    // content_hash tests
    // ---------------------------------------------------------------------------

    #[test]
    fn content_hash_empty_file() {
        let tmp = NamedTempFile::new().expect("tempfile");
        let (hash, size, mtime) = hash_file(tmp.path()).expect("hash_file");
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "empty file must match SHA-256 of empty bytes"
        );
        assert_eq!(size, 0);
        // mtime may legally be 0 on some platforms, so just assert it is non-negative.
        assert!(mtime >= 0);
    }

    #[test]
    fn content_hash_small_file() {
        // A file smaller than 4 MiB has exactly one block.
        // content_hash = SHA-256( SHA-256(content) )
        let content = b"hello, dropbox content hash!";
        let mut tmp = NamedTempFile::new().expect("tempfile");
        tmp.write_all(content).expect("write");
        tmp.flush().expect("flush");

        let (hash, size, mtime) = hash_file(tmp.path()).expect("hash_file");

        let expected = expected_content_hash(content);
        assert_eq!(hash, expected);
        assert_eq!(size, content.len() as i64);
        assert!(mtime > 0, "modified timestamp should be non-zero");
    }

    #[test]
    fn content_hash_exactly_one_block() {
        // Exactly 4 MiB of zeros: one full block, no remainder.
        let content = vec![0_u8; DROPBOX_BLOCK_SIZE];
        let mut tmp = NamedTempFile::new().expect("tempfile");
        tmp.write_all(&content).expect("write");
        tmp.flush().expect("flush");

        let (hash, size, _mtime) = hash_file(tmp.path()).expect("hash_file");

        let expected = expected_content_hash(&content);
        assert_eq!(hash, expected);
        assert_eq!(size, DROPBOX_BLOCK_SIZE as i64);
    }

    #[test]
    fn content_hash_one_block_plus_one_byte() {
        // 4 MiB + 1 byte: two blocks (one full, one 1-byte).
        let mut content = vec![0_u8; DROPBOX_BLOCK_SIZE];
        content.push(0_u8);

        let mut tmp = NamedTempFile::new().expect("tempfile");
        tmp.write_all(&content).expect("write");
        tmp.flush().expect("flush");

        let (hash, size, _mtime) = hash_file(tmp.path()).expect("hash_file");

        let expected = expected_content_hash(&content);
        assert_eq!(hash, expected);
        assert_eq!(size, (DROPBOX_BLOCK_SIZE + 1) as i64);
    }

    #[test]
    fn content_hash_known_value_independent() {
        // Independent cross-check: compute expected hash WITHOUT using the
        // helper function, to avoid the "same oracle" testing pitfall.
        // For a single-block file: content_hash = SHA-256(SHA-256(content)).
        let content = b"hello, dropbox content hash!";
        let inner_digest = Sha256::digest(content);
        let expected = format!("{:x}", Sha256::digest(inner_digest));

        let mut tmp = NamedTempFile::new().expect("tempfile");
        tmp.write_all(content).expect("write");
        tmp.flush().expect("flush");

        let (hash, _, _) = hash_file(tmp.path()).expect("hash_file");
        assert_eq!(hash, expected);
    }

    #[test]
    fn content_hash_returns_correct_size_and_mtime() {
        let content = b"size and mtime check";
        let mut tmp = NamedTempFile::new().expect("tempfile");
        tmp.write_all(content).expect("write");
        tmp.flush().expect("flush");

        let (_hash, size, mtime) = hash_file(tmp.path()).expect("hash_file");
        assert_eq!(size, content.len() as i64);
        assert!(mtime > 0, "modified timestamp must be non-zero for a newly created file");
    }
}
