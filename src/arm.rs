//! Arm the reload sentinel for a session: distill in-process, build the
//! reload string, then write the sentinel and its sidecars atomically
//! (sentinel last, P6). Ported from `reseed-here`.

use crate::{atomic, identity, paths, sentinel};
use anyhow::{bail, Result};
use std::path::PathBuf;

pub struct ArmOpts {
    pub sid: Option<String>,
    pub quiet: bool,
    pub pid: Option<u32>,
}

fn operator() -> String {
    std::env::var("RESEED_OPERATOR").unwrap_or_else(|_| "Mark".to_string())
}

/// Arm the sentinel for `opts.sid` (or `$CLAUDE_CODE_SESSION_ID`). Returns
/// the bundle directory on success.
pub fn run(opts: ArmOpts) -> Result<PathBuf> {
    let sid = match opts
        .sid
        .or_else(|| std::env::var("CLAUDE_CODE_SESSION_ID").ok())
    {
        Some(s) if !s.is_empty() => s,
        _ => bail!(
            "reseed arm: $CLAUDE_CODE_SESSION_ID is unset - run this inside a Claude Code \
             session (use the '! reseed-here' bang prefix, or pass --sid)."
        ),
    };

    let pending = paths::pending()?;
    let key = sentinel::key(&sid);
    let _lock = match sentinel::lock(&pending, &key)? {
        Some(l) => l,
        None => {
            eprintln!("already distilling");
            return paths::bundle(&sid);
        }
    };

    let bundle_dir = crate::run_distill(&sid, None)?;
    let lines = std::fs::read_to_string(bundle_dir.join("narrative.md"))
        .map(|s| s.lines().count())
        .unwrap_or(0);
    let ledger_path = paths::ledger()?;
    let ledger_present = std::fs::metadata(&ledger_path)
        .map(|m| m.len() > 0)
        .unwrap_or(false);

    let reload = reload_string(&bundle_dir, lines, ledger_present, &ledger_path);

    if !opts.quiet {
        let _ = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin.take().unwrap().write_all(reload.as_bytes())?;
                c.wait()
            });
    }

    let cwd = std::env::var("PWD")
        .ok()
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
        })
        .unwrap_or_default();
    atomic::write(&pending.join(format!("{key}.cwd")), cwd.as_bytes())?;
    if let Ok(job_dir) = std::env::var("CLAUDE_JOB_DIR") {
        if !job_dir.is_empty() {
            let job = PathBuf::from(&job_dir)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            atomic::write(&pending.join(format!("{key}.job")), job.as_bytes())?;
        }
    }
    // P7: the sidecar must name the session's process. Never the parent:
    // on the detached rearm path that is the hook, not Claude.
    let pid = opts
        .pid
        .or_else(|| std::env::var("RESEED_PID").ok()?.parse().ok())
        .or_else(|| identity::registry_pid(&sid))
        .unwrap_or_else(std::process::id);
    atomic::write(
        &pending.join(format!("{key}.pid")),
        pid.to_string().as_bytes(),
    )?;

    // Sentinel LAST (P6): a reader must see either no arm or a complete one.
    let mut sentinel_body = reload.clone();
    sentinel_body.push('\n');
    atomic::write(&pending.join(&key), sentinel_body.as_bytes())?;

    if !opts.quiet {
        eprintln!(
            "\nReload armed for session {sid} (and copied to clipboard as fallback):\n  {reload}\n\n\
             Next (foreground TUI):  /clear   then type \"go\" (paste if the hook misses).\n\
             NB: /clear is a slash command, do NOT prefix it with \"!\".\n\
             Next (background/web session): same, /clear then \"go\". A background /clear mints a NEW \
             session id, so the id key misses, but the reload matches this job's lineage and falls \
             back to the cwd sidecar. Only if \"go\" injects nothing: start a fresh session and send \
             the reload line above.\n\
             (Bundle lags one turn - it captures up to your previous message.\n\
             The armed reload expires after 10 minutes.)"
        );
    }

    Ok(bundle_dir)
}

/// Build the reload instruction: `readverb` (P6/P7-agnostic), a provenance
/// step, and a park-ledger step when the ledger is non-empty.
pub fn reload_string(
    bundle: &std::path::Path,
    lines: usize,
    ledger_present: bool,
    ledger_path: &std::path::Path,
) -> String {
    let readverb = if lines > 900 {
        format!(
            "in <=600-line chunks with offset until EOF ({lines} lines; the Read tool caps at 25k \
             TOKENS and silently returns a PARTIAL view, so ONE bare Read misses the tail and a \
             larger limit makes it worse. Map turn boundaries FIRST with awk, never grep. Then read \
             the LAST user+assistant pair, and skip multi-thousand-line gaps between markers, which \
             are inlined skill payloads. The FINAL lines of the file are usually a skill payload, not \
             the last turn)"
        )
    } else {
        format!("in full ({lines} lines)")
    };

    let provstep = format!(
        " Before any of that, `sed -n '1,20p' {bundle}/narrative.md`: if that bundle's OWN first \
         user turn is a real prompt, it owns its subject and you resume inside it. If it is a bare \
         /clear plus \"go\" (or context-files.md lists ANOTHER reseed bundle), this bundle is a CHAIN \
         LINK - its subject was INHERITED from a park line, so its AUTHORISATION is stale, not its \
         SUBJECT. Read narrative.md anyway: it is still the best evidence of what this thread is \
         about, and the park-file tail is NOT a substitute for it. Local work (reading, analysing, \
         drafting to ~/scratch/) proceeds; but name the inherited subject back to {op} in one line and \
         get a yes BEFORE any outward-facing write, because a resumed authorisation is not a live \
         authorisation.",
        bundle = bundle.display(),
        op = operator(),
    );

    let parkedstep = if ledger_present {
        format!(
            " Then, holding the subject you just read from narrative.md, read the 'Park-ledger \
             CANDIDATE lines' block printed below this reload: those are keyword matches, NOT a \
             verified list - the matcher has adopted other threads' lines before - so check each one \
             against your subject and keep only the lines that name it; they are pre-grepped so \
             {ledger} itself stays closed (a direct `grep -n -iE '<subject keywords>' {ledger} | cut \
             -c1-3000` is only for a resource you are about to re-derive). That file is a HOST-WIDE \
             append-only queue that EVERY session on this box writes to, including ones still \
             running, so its newest line is more likely another live thread's work than yours - \
             recency is not relevance, and a positional read of it is blocked by the ledger scope \
             guard. A line that does not match your subject is ANOTHER THREAD'S WORK: its `resume:` \
             pointer is an address, not an assignment, even when it is newest and even when it shouts \
             OWED. Grep it for any resource you are about to re-derive, since the narrative's prose \
             records WHAT was done, not HOW, and is not a spec for redoing anything. Whole-host \
             survey, only when {op} asked what is parked: `tail -8 {ledger} | cut -c1-400  # \
             ledger-survey`.",
            ledger = ledger_path.display(),
            op = operator(),
        )
    } else {
        String::new()
    };

    format!(
        "Read {bundle}/narrative.md {readverb}, then read {bundle}/context-files.md, and continue \
         where we left off.{provstep}{parkedstep}",
        bundle = bundle.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn short_narrative_reads_in_full() {
        let s = reload_string(Path::new("/b"), 42, false, Path::new("/l"));
        assert!(s.contains("in full (42 lines)"));
        assert!(!s.contains("Park-ledger"));
    }

    #[test]
    fn long_narrative_uses_chunked_readverb() {
        let s = reload_string(Path::new("/b"), 901, false, Path::new("/l"));
        assert!(s.contains("<=600-line chunks"));
    }

    #[test]
    fn ledger_present_adds_the_parked_step() {
        let s = reload_string(Path::new("/b"), 10, true, Path::new("/l/parked.md"));
        assert!(s.contains("Park-ledger"));
        assert!(s.contains("/l/parked.md"));
    }
}
