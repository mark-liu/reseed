//! The reload sentinel: `pending/<key>` plus its `.cwd` `.job` `.pid`
//! sidecars, and the arm lock. Ported from `reseed-here` and
//! `ctxstate.sentinel_armed`.

use anyhow::Result;
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const FRESH_SECS: u64 = 600;
const LOCK_STALE_SECS: u64 = 120;

/// Sanitise a session id into a sentinel key: anything but
/// `[A-Za-z0-9._-]` becomes `_`.
pub fn key(sid: &str) -> String {
    let re = Regex::new(r"[^A-Za-z0-9._-]").unwrap();
    re.replace_all(sid, "_").into_owned()
}

#[derive(Debug, Clone)]
pub struct Arm {
    pub path: PathBuf,
    pub reload: String,
    pub cwd: Option<String>,
    pub job: Option<String>,
    pub pid: Option<u32>,
    pub mtime: SystemTime,
}

/// Read a sentinel by key, or `None` if it does not exist.
pub fn read(pending: &Path, key: &str) -> Option<Arm> {
    let path = pending.join(key);
    let meta = fs::metadata(&path).ok()?;
    let reload = fs::read_to_string(&path).ok()?;
    let cwd = fs::read_to_string(pending.join(format!("{key}.cwd"))).ok();
    let job = fs::read_to_string(pending.join(format!("{key}.job"))).ok();
    let pid = fs::read_to_string(pending.join(format!("{key}.pid")))
        .ok()
        .and_then(|s| s.trim().parse().ok());
    let mtime = meta.modified().ok()?;
    Some(Arm {
        path,
        reload,
        cwd,
        job,
        pid,
        mtime,
    })
}

/// True when `arm` was written within the last 600 seconds (mirrors
/// `reseed-clear-hook.sh`'s own staleness rule).
pub fn is_fresh(arm: &Arm, now: SystemTime) -> bool {
    match now.duration_since(arm.mtime) {
        Ok(age) => age < Duration::from_secs(FRESH_SECS),
        Err(_) => true, // mtime in the future: treat as fresh rather than fail closed
    }
}

/// All armed sentinels under `pending` (sidecars excluded).
pub fn list(pending: &Path) -> Vec<Arm> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(pending) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.contains('.') {
            continue; // sidecar or lock file
        }
        if let Some(arm) = read(pending, name) {
            out.push(arm);
        }
    }
    out
}

/// Remove a sentinel and all its sidecars.
pub fn remove(arm: &Arm) {
    let pending = arm.path.parent().unwrap_or_else(|| Path::new("."));
    let Some(name) = arm.path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    for suffix in ["", ".cwd", ".job", ".pid"] {
        let _ = fs::remove_file(pending.join(format!("{name}{suffix}")));
    }
}

/// Held for the duration of a distill so two detached rearms cannot race.
/// Released automatically when dropped.
pub struct Lock {
    path: PathBuf,
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Try to acquire `pending/<key>.lock`. `None` means another arm holds a
/// fresh lock (stale after 120s, in which case it is stolen).
pub fn lock(pending: &Path, key: &str) -> Result<Option<Lock>> {
    fs::create_dir_all(pending)?;
    let path = pending.join(format!("{key}.lock"));
    if let Ok(meta) = fs::metadata(&path) {
        let age = meta
            .modified()
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .unwrap_or(Duration::ZERO);
        if age < Duration::from_secs(LOCK_STALE_SECS) {
            return Ok(None);
        }
        // Stale: steal it.
        let _ = fs::remove_file(&path);
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(_) => Ok(Some(Lock { path })),
        Err(_) => Ok(None), // lost the race to another arm
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn key_sanitises_non_word_chars() {
        assert_eq!(key("abc-123_def.ghi"), "abc-123_def.ghi");
        assert_eq!(key("a/b c:d"), "a_b_c_d");
    }

    #[test]
    fn freshness_at_599_and_601_seconds() {
        let now = SystemTime::now();
        let fresh = Arm {
            path: PathBuf::new(),
            reload: String::new(),
            cwd: None,
            job: None,
            pid: None,
            mtime: now - Duration::from_secs(599),
        };
        assert!(is_fresh(&fresh, now));
        let stale = Arm {
            mtime: now - Duration::from_secs(601),
            ..fresh
        };
        assert!(!is_fresh(&stale, now));
    }

    #[test]
    fn lock_held_then_stale() {
        let dir = tempdir().unwrap();
        let l1 = lock(dir.path(), "sid").unwrap();
        assert!(l1.is_some());
        assert!(
            lock(dir.path(), "sid").unwrap().is_none(),
            "second lock must be denied"
        );
        let lock_path = dir.path().join("sid.lock");
        let old = SystemTime::now() - Duration::from_secs(121);
        std::fs::File::options()
            .write(true)
            .open(&lock_path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        drop(l1);
        assert!(
            lock(dir.path(), "sid").unwrap().is_some(),
            "stale lock must be stealable"
        );
    }

    #[test]
    fn read_missing_sentinel_is_none() {
        let dir = tempdir().unwrap();
        assert!(read(dir.path(), "nope").is_none());
    }
}
