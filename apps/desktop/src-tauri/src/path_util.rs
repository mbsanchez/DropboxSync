use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::Utc;
use sha2::{Digest, Sha256};

pub(crate) fn should_ignore_local_path(relative: &str) -> bool {
    let p = relative.replace('\\', "/");
    p == ".DS_Store" || p.ends_with("/.DS_Store") || p.starts_with("._") || p.contains("/._")
}

pub(crate) fn relpath_under(sync_folder: &Path, absolute: &Path) -> Result<String, String> {
    Ok(absolute
        .strip_prefix(sync_folder)
        .map_err(|e| format!("failed to compute relative path: {e}"))?
        .to_string_lossy()
        .to_string())
}

pub(crate) fn normalize_dropbox_path(input: &str) -> String {
    if input.is_empty() {
        return "".to_string();
    }
    if input.starts_with('/') {
        input.to_string()
    } else {
        format!("/{}", input)
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

pub(crate) fn hash_file(path: &Path) -> Result<(String, i64, i64), String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open file for hash: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
    let size = metadata.len() as i64;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|ts| ts.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    Ok((format!("{:x}", hasher.finalize()), size, modified))
}

pub(crate) fn create_conflicted_copy(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "file has no parent for conflict copy".to_string())?;
    let stem = path
        .file_stem()
        .ok_or_else(|| "missing file stem".to_string())?
        .to_string_lossy();
    let ext = path.extension().map(|e| e.to_string_lossy().to_string());
    let ts = Utc::now().format("%Y%m%d%H%M%S");

    let name = match ext {
        Some(ext) => format!("{} (conflicted copy {}).{}", stem, ts, ext),
        None => format!("{} (conflicted copy {})", stem, ts),
    };

    let dest = parent.join(name);
    fs::copy(path, &dest).map_err(|e| format!("failed to create conflicted copy: {e}"))?;
    Ok(dest)
}

pub(crate) fn backoff_seconds(attempt: i64) -> i64 {
    let safe_attempt = attempt.clamp(1, 10) as u32;
    2_i64.pow(safe_attempt)
}

#[cfg(test)]
mod tests {
    use super::backoff_seconds;

    #[test]
    fn backoff_grows_exponentially() {
        assert_eq!(backoff_seconds(1), 2);
        assert_eq!(backoff_seconds(2), 4);
        assert_eq!(backoff_seconds(3), 8);
    }
}
