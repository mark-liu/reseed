//! P5: rearm a session's reload bundle from a hook, detached.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// Kick off `reseed arm --quiet` for `sid`, detached in its own process
/// group so it outlives the calling hook. True if the process was launched.
pub fn rearm(sid: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Ok(home) = crate::paths::home() else {
        return false;
    };
    Command::new(exe)
        .args(["arm", "--quiet"])
        .env("CLAUDE_CODE_SESSION_ID", sid)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0) // own group: must outlive the calling hook's exit
        .spawn()
        .is_ok()
}
