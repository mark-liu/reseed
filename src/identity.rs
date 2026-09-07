//! P7: which OS process is running a session. The `.pid` sidecar `arm`
//! writes and the tier-1b comparison `reload` makes are two halves of one
//! equality, so both resolve a pid through this module.

use crate::paths;

/// The pid running `sid`, from the session registry entry whose `sessionId`
/// matches. `None` for a session the registry never recorded.
pub fn registry_pid(sid: &str) -> Option<u32> {
    let dir = paths::sessions_dir().ok()?;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
            continue;
        };
        if v.get("sessionId").and_then(|s| s.as_str()) == Some(sid) {
            return path.file_stem().and_then(|s| s.to_str())?.parse().ok();
        }
    }
    None
}

/// The calling process's parent. Only a session pid inside a hook, where
/// the parent IS the Claude process; a detached `arm` would name the hook
/// that spawned it, which is why `arm` never falls back to this.
pub fn parent_pid() -> Option<u32> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// A session's pid as seen from a hook: the registry, else this hook's own
/// parent for a session the registry has not caught up with.
pub fn session_pid(sid: &str) -> Option<u32> {
    registry_pid(sid).or_else(parent_pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    fn with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        let _held = crate::testlock::env_lock();
        std::env::set_var("HOME", home);
        let out = f();
        std::env::remove_var("HOME");
        out
    }

    fn plant(home: &Path, pid: u32, sid: &str) {
        let dir = home.join(".claude/sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{pid}.json")),
            format!(r#"{{"pid":{pid},"sessionId":"{sid}","cwd":"/x"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn the_registry_names_the_pid_of_the_matching_session() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        plant(home, 111, "aaaa-1");
        plant(home, 222, "bbbb-2");
        assert_eq!(with_home(home, || registry_pid("bbbb-2")), Some(222));
    }

    #[test]
    fn an_unregistered_session_has_no_registry_pid() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        plant(home, 111, "aaaa-1");
        assert_eq!(with_home(home, || registry_pid("cccc-3")), None);
    }

    /// The fallback is the hook's parent, so it must not be this process.
    #[test]
    fn the_parent_pid_is_a_real_other_process() {
        let ppid = parent_pid().expect("ps reports a parent");
        assert_ne!(ppid, std::process::id());
    }
}
