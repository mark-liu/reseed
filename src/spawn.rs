//! P5: rearm a session's reload bundle from a hook, detached.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// Kick off `reseed arm --quiet` for `sid`, detached in its own process
/// group so it outlives the calling hook. True if the process was launched.
///
/// The pid is resolved HERE and passed down: this runs in the hook, whose
/// parent is the Claude process, while the detached child's own pid is a
/// process that exits in seconds and can never match tier 1b (P7).
pub fn rearm(sid: &str) -> bool {
    let Some(mut cmd) = arm_command(sid) else {
        return false;
    };
    cmd.process_group(0) // own group: must outlive the calling hook's exit
        .spawn()
        .is_ok()
}

/// Run `reseed arm --quiet` for `sid` and WAIT for it. True if it exited 0.
///
/// The stale reload path needs the wait: the bash re-arms synchronously and
/// then prints what the sentinel says afterwards, so a detached spawn would
/// print the arm that went stale and drop the fresh one in after the remove.
pub fn rearm_sync(sid: &str) -> bool {
    let Some(mut cmd) = arm_command(sid) else {
        return false;
    };
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// The re-arm invocation both callers share, or `None` when this process
/// cannot name itself or its home.
fn arm_command(sid: &str) -> Option<Command> {
    let exe = std::env::current_exe().ok()?;
    let home = crate::paths::home().ok()?;
    let mut cmd = Command::new(exe);
    cmd.args(arm_args(crate::identity::session_pid(sid)))
        .env("CLAUDE_CODE_SESSION_ID", sid)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Some(cmd)
}

/// The `arm` argv, with `--pid` only when a session pid resolved: passing
/// none is better than passing a wrong one, which would arm a sentinel that
/// another session's tier 1b could claim.
fn arm_args(pid: Option<u32>) -> Vec<String> {
    let mut args = vec!["arm".to_string(), "--quiet".to_string()];
    if let Some(pid) = pid {
        args.push("--pid".to_string());
        args.push(pid.to_string());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resolved_pid_reaches_the_detached_arm() {
        assert_eq!(arm_args(Some(4242)), ["arm", "--quiet", "--pid", "4242"]);
    }

    #[test]
    fn an_unresolved_pid_is_omitted_rather_than_guessed() {
        assert_eq!(arm_args(None), ["arm", "--quiet"]);
    }
}
