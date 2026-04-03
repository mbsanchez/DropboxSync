use std::fs;
use std::path::{Path, PathBuf};

use crate::models::CloudscMeta;

pub(crate) const CLOUDSC_MAGIC: &str = "CLOUDSC1";

pub(crate) fn encode_cloudsc_meta(meta: &CloudscMeta) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(meta).map_err(|e| format!("cloudsc encode json failed: {e}"))?;
    let mut out = Vec::new();
    out.extend_from_slice(CLOUDSC_MAGIC.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(&json);
    Ok(out)
}

pub(crate) fn decode_cloudsc_meta(bytes: &[u8]) -> Result<CloudscMeta, String> {
    let magic = CLOUDSC_MAGIC.as_bytes();
    if bytes.len() < magic.len() + 1 {
        return Err("invalid .cloudsc payload (too small)".to_string());
    }
    if &bytes[..magic.len()] != magic {
        return Err("invalid .cloudsc payload (bad magic)".to_string());
    }
    let json_bytes = &bytes[magic.len() + 1..];
    serde_json::from_slice(json_bytes).map_err(|e| format!("cloudsc decode json failed: {e}"))
}

pub(crate) fn cloudsc_target_path(placeholder_path: &Path) -> PathBuf {
    placeholder_path.with_extension("")
}

pub(crate) fn write_cloudsc_placeholder_file(
    placeholder_path: &Path,
    meta: &CloudscMeta,
) -> Result<(), String> {
    if let Some(parent) = placeholder_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed creating placeholder parent dir: {e}"))?;
    }
    let payload = encode_cloudsc_meta(meta)?;
    fs::write(placeholder_path, payload)
        .map_err(|e| format!("failed writing placeholder file: {e}"))?;
    Ok(())
}

pub(crate) fn read_cloudsc_placeholder_file(placeholder_path: &Path) -> Result<CloudscMeta, String> {
    let bytes = fs::read(placeholder_path)
        .map_err(|e| format!("failed reading placeholder file bytes: {e}"))?;
    decode_cloudsc_meta(&bytes)
}
