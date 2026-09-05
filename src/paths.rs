//! All reseed state paths, derived from `$HOME` so tests run under a temp
//! HOME exactly like the Python suites they replace.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME not set")
}

pub fn projects() -> Result<PathBuf> {
    Ok(home()?.join(".claude/projects"))
}

pub fn reseed_dir() -> Result<PathBuf> {
    Ok(home()?.join(".claude/reseed"))
}

pub fn pending() -> Result<PathBuf> {
    Ok(reseed_dir()?.join("pending"))
}

pub fn bundle(sid: &str) -> Result<PathBuf> {
    Ok(reseed_dir()?.join(sid))
}

pub fn nudge_state() -> Result<PathBuf> {
    Ok(home()?.join(".claude/cache/reseed-nudge"))
}

pub fn guard_state() -> Result<PathBuf> {
    Ok(home()?.join(".claude/cache/context-reset-guard"))
}

pub fn tasks() -> Result<PathBuf> {
    Ok(home()?.join(".claude/tasks"))
}

/// Session registry dir: one `<pid>.json` per live session, `sessionId` inside.
pub fn sessions_dir() -> Result<PathBuf> {
    Ok(home()?.join(".claude/sessions"))
}

pub fn emit_log() -> Result<PathBuf> {
    Ok(reseed_dir()?.join("emit.log"))
}

pub fn watch_state() -> Result<PathBuf> {
    Ok(reseed_dir()?.join("watch-state.json"))
}

pub fn watch_log() -> Result<PathBuf> {
    Ok(reseed_dir()?.join("watch.log"))
}

pub fn kill_file() -> Result<PathBuf> {
    Ok(reseed_dir()?.join("watch.off"))
}

/// The host's park ledger: `RESEED_PARK_LEDGER`, else
/// `~/scratch/parked/<ComputerName>.md` (falls back to `hostname -s`).
pub fn ledger() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("RESEED_PARK_LEDGER") {
        return Ok(PathBuf::from(p));
    }
    Ok(home()?
        .join("scratch/parked")
        .join(format!("{}.md", computer_name())))
}

fn computer_name() -> String {
    let scutil = Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    scutil.unwrap_or_else(|| {
        Command::new("hostname")
            .arg("-s")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "localhost".to_string())
    })
}
