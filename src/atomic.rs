//! Atomic writes: tmp file beside the target, then rename. A reader must
//! always see either the previous complete file or the new one, never a
//! partial write (P6).

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Write `bytes` to `path` atomically: a sibling `.tmp` file, then rename.
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_path(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Swap a fully-built temp directory into place, replacing `dst` atomically.
pub fn replace_dir(tmp: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        let mut name = dst
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(".bak"); // distinct from tmp's own ".tmp" name, or the rename below collides
        let backup = dst.with_file_name(name);
        fs::rename(dst, &backup).with_context(|| format!("backing up {}", dst.display()))?;
        fs::rename(tmp, dst).with_context(|| format!("swapping in {}", tmp.display()))?;
        fs::remove_dir_all(&backup).ok();
    } else {
        fs::rename(tmp, dst).with_context(|| format!("swapping in {}", tmp.display()))?;
    }
    Ok(())
}

fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_leaves_no_tmp_residue() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("f.txt");
        write(&target, b"hello").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello");
        assert!(!tmp_path(&target).exists());
    }

    #[test]
    fn write_overwrites_existing() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("f.txt");
        write(&target, b"first").unwrap();
        write(&target, b"second").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "second");
    }

    #[test]
    fn a_missing_write_leaves_the_old_file_intact() {
        // Simulates "killed mid-write": the tmp file is written but rename
        // never happens. The original target must be untouched.
        let dir = tempdir().unwrap();
        let target = dir.path().join("f.txt");
        write(&target, b"original").unwrap();
        fs::write(tmp_path(&target), b"partial").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "original");
    }
}
