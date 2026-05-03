//! Atomic whole-file replace: write a sibling temp file, then [`std::fs::rename`] into place.
//!
//! Readers never see a half-written destination (same pattern as balance snapshots in [`crate::risk`]).
//! The temp file lives in the **same directory** as the destination so `rename` stays on one filesystem.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Temp path next to `dest`: `file.ext` → `file.ext.tmp` (extension becomes `ext.tmp`).
fn sibling_temp_path(dest: &Path) -> PathBuf {
    match dest.extension().and_then(|e| e.to_str()) {
        Some(ext) => dest.with_extension(format!("{ext}.tmp")),
        None => dest.with_extension("tmp"),
    }
}

/// Write `bytes` to `dest` by renaming a fully written temp file into place.
pub fn write_path_atomic(dest: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = sibling_temp_path(dest);
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to write temp {}", tmp.display()));
    }
    std::fs::rename(&tmp, dest).with_context(|| {
        format!(
            "failed to rename {} -> {}",
            tmp.display(),
            dest.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn round_trip_overwrites_atomically() {
        let dir = std::env::temp_dir().join(format!("fs_atomic_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("state.json");
        write_path_atomic(&dest, b"v1").unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "v1");
        write_path_atomic(&dest, b"v2-longer").unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "v2-longer");
        assert!(!sibling_temp_path(&dest).exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
