//! reseed: distill a Claude Code session into a slim, reloadable bundle.
//!
//! A long Claude Code session is dominated by tool input/output: in
//! typical transcripts the `tool_use` + `tool_result` blocks are 60-65%
//! of the bytes, while the actual user/assistant narrative is ~12%.
//! `reseed` strips the tool I/O into an addressable, defang-on-read
//! archive, keeps the narrative with `[tool#NNN]` pointers, lists the
//! files the session touched, and reports the token savings, so you can
//! `/clear` and reseed a fresh context window without losing the thread.

// Phase B (`reload`) and phase C (`watch`) consume several items defined
// here early; unused in phase A alone is expected, not a defect.
#![allow(dead_code)]

mod arm;
mod atomic;
mod defang;
mod distill;
mod emit;
mod hooks;
mod msg;
mod park;
mod parse;
mod paths;
mod reload;
mod sentinel;
mod spawn;
#[cfg(test)]
mod testlock;
mod tokens;
mod usage;
mod watch;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "reseed",
    version,
    about = "Distill a Claude Code session into a reloadable bundle"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Distill a session into a bundle (narrative + tool archive + savings).
    Distill {
        /// Session id (prefix ok) or a path to a transcript .jsonl.
        session: String,
        /// Output directory (default: ~/.claude/reseed/<session-id>).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Print one archived tool call, defanged by default.
    Fetch {
        /// Session id (prefix ok) or path to a bundle directory.
        session: String,
        /// Pointer number, e.g. 42 (matches [tool#042]).
        n: usize,
        /// Print raw, un-defanged bytes (re-injection risk, debug only).
        #[arg(long)]
        raw: bool,
    },
    /// Distill, then launch a fresh `claude` seeded to read the narrative.
    Launch {
        /// Session id (prefix ok) or a path to a transcript .jsonl.
        session: String,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Print a transcript's context tier (bare, as ctxstate.py's CLI did).
    Ctx {
        /// Path to a transcript .jsonl.
        transcript: PathBuf,
        /// Print the full snapshot as JSON instead of the bare tier.
        #[arg(long)]
        json: bool,
    },
    /// Arm the reload sentinel for the current (or given) session.
    Arm {
        #[arg(long)]
        quiet: bool,
        #[arg(long)]
        sid: Option<String>,
        #[arg(long)]
        pid: Option<u32>,
    },
    /// SessionStart(clear) hook: emit an armed reload, if any. Reads the
    /// SessionStart payload from stdin when stdin is not a terminal.
    Reload,
    /// Run one of the reset-workflow hooks, reading its event payload on stdin.
    Hook {
        #[command(subcommand)]
        which: HookName,
    },
    /// Print park-ledger lines matching a bundle's subject.
    ParkMatch {
        /// The bundle directory (holds narrative.md).
        bundle_dir: PathBuf,
        /// Ledger path (default: RESEED_PARK_LEDGER or ~/scratch/parked/<Host>.md).
        ledger: Option<PathBuf>,
    },
    /// Report-only reload detector: live sessions past the context line,
    /// and an audit of whether recent reload emissions were delivered.
    /// Never types into a session (spec 11c: detector half only).
    Watch {
        /// The only supported mode for now; accepted, always a single pass.
        #[arg(long)]
        once: bool,
        /// Window the emit.log audit to the last N days.
        #[arg(long)]
        since: Option<u64>,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
        /// Override the watch.log path (default: ~/.claude/reseed/watch.log).
        #[arg(long)]
        log: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum HookName {
    Nudge,
    Guard,
    Halt,
    Override,
    ProbeGuard,
    LedgerGuard,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Distill { session, out } => {
            let bundle_dir = run_distill(&session, out)?;
            println!("Bundle written to {}", bundle_dir.display());
            println!(
                "Reseed with:  claude  then  \"Read {}/narrative.md and continue\"",
                bundle_dir.display()
            );
            Ok(())
        }
        Command::Fetch { session, n, raw } => run_fetch(&session, n, raw),
        Command::Launch { session, out } => {
            let bundle_dir = run_distill(&session, out)?;
            launch_claude(&bundle_dir)
        }
        Command::Ctx { transcript, json } => run_ctx(&transcript, json),
        Command::Arm { quiet, sid, pid } => {
            let dir = arm::run(arm::ArmOpts { sid, quiet, pid })?;
            if quiet {
                Ok(())
            } else {
                println!("Bundle written to {}", dir.display());
                Ok(())
            }
        }
        Command::Hook { which } => {
            let payload = hooks::read_payload().unwrap_or_default();
            let code = match which {
                HookName::Nudge => hooks::nudge::run(payload),
                HookName::Guard => hooks::guard::run(payload),
                HookName::Halt => hooks::halt::run(payload),
                HookName::Override => hooks::override_::run(payload),
                HookName::ProbeGuard => hooks::probe_guard::run(payload),
                HookName::LedgerGuard => hooks::ledger_guard::run(payload),
            };
            std::process::exit(code);
        }
        Command::ParkMatch { bundle_dir, ledger } => run_park_match(&bundle_dir, ledger),
        Command::Reload => reload::run(reload::read_payload()),
        Command::Watch {
            once,
            since,
            json,
            log,
        } => watch::run(watch::WatchOpts {
            once,
            since_days: since,
            json,
            log_path: log,
        }),
    }
}

/// `reseed ctx`: print a transcript's context tier, or the full snapshot as JSON.
fn run_ctx(transcript: &Path, json: bool) -> Result<()> {
    if json {
        let (tier, ctx, line, early) = usage::context_state(transcript);
        println!(
            "{}",
            serde_json::json!({"tier": tier, "ctx": ctx, "line": line, "early": early})
        );
    } else {
        let (tier, _ctx, _line) = usage::context_tier(transcript);
        println!("{tier}");
    }
    Ok(())
}

/// `reseed park-match`: print ledger lines matching a bundle's subject.
fn run_park_match(bundle_dir: &Path, ledger: Option<PathBuf>) -> Result<()> {
    let narrative = bundle_dir.join("narrative.md");
    let ledger_path = match ledger {
        Some(p) => p,
        None => paths::ledger()?,
    };
    let Ok(text) = fs::read_to_string(&narrative) else {
        return Ok(());
    };
    let Ok(ledger_text) = fs::read_to_string(&ledger_path) else {
        return Ok(());
    };
    let lines: Vec<String> = ledger_text.lines().map(String::from).collect();
    let m = park::matches(&text, &lines);
    let out = park::render(&m);
    if !out.is_empty() {
        println!("{out}");
    }
    Ok(())
}

/// Distill a session and write the bundle. Returns the bundle directory.
pub(crate) fn run_distill(session: &str, out: Option<PathBuf>) -> Result<PathBuf> {
    let transcript = resolve_transcript(session)?;
    let session_id = transcript
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session")
        .to_string();

    let file = fs::File::open(&transcript)
        .with_context(|| format!("opening transcript {}", transcript.display()))?;
    let items = parse::parse_reader(file)?;
    if items.is_empty() {
        bail!("no conversation content found in {}", transcript.display());
    }

    let bundle = distill::distill(&items, &session_id);
    let dir = match out {
        Some(d) => d,
        None => default_bundle_dir(&session_id)?,
    };
    write_bundle(&dir, &bundle)?;

    eprintln!(
        "[reseed] {} tool calls archived · ~{} → ~{} tokens ({:.0}% saved) · ~{} tokens of harness boilerplate dropped from narrative",
        bundle.calls.len(),
        bundle.full_tokens,
        bundle.distilled_tokens,
        savings_pct(bundle.full_tokens, bundle.distilled_tokens),
        bundle.harness_stripped_tokens,
    );
    Ok(dir)
}

/// Write a bundle atomically (P6): the four top-level files each go through
/// `atomic::write`, and `calls/` is built at `calls.tmp/` then swapped in,
/// so a reader mid-write sees either the previous complete bundle or the new
/// one, never a partial one.
fn write_bundle(dir: &Path, bundle: &distill::Bundle) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating bundle dir {}", dir.display()))?;

    atomic::write(&dir.join("narrative.md"), bundle.narrative.as_bytes())?;
    atomic::write(
        &dir.join("context-files.md"),
        distill::render_context_files(&bundle.context_files).as_bytes(),
    )?;
    atomic::write(&dir.join("index.json"), bundle.index_json.as_bytes())?;
    atomic::write(&dir.join("savings.md"), bundle.savings_md.as_bytes())?;

    let calls_tmp = dir.join("calls.tmp");
    if calls_tmp.exists() {
        fs::remove_dir_all(&calls_tmp)
            .with_context(|| format!("clearing stale {}", calls_tmp.display()))?;
    }
    fs::create_dir_all(&calls_tmp).with_context(|| format!("creating {}", calls_tmp.display()))?;
    for call in &bundle.calls {
        let path = calls_tmp.join(format!("{:03}.json", call.n));
        let json = serde_json::to_string_pretty(call)?;
        fs::write(path, json)?;
    }
    atomic::replace_dir(&calls_tmp, &dir.join("calls"))?;
    Ok(())
}

/// Read one archived call and print it. Defangs input + result unless
/// `--raw`, mirroring the transcript-as-backdoor threat model.
fn run_fetch(session: &str, n: usize, raw: bool) -> Result<()> {
    let dir = resolve_bundle_dir(session)?;
    let path = dir.join("calls").join(format!("{n:03}.json"));
    let body = fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {}: has this session been distilled?",
            path.display()
        )
    })?;
    let call: distill::Call = serde_json::from_str(&body)
        .with_context(|| format!("parsing archived call {}", path.display()))?;

    let input = call.input.to_string();
    let (input, result) = if raw {
        eprintln!("[reseed] --raw: printing un-defanged bytes (re-injection risk)");
        (input, call.result)
    } else {
        (defang::defang(&input), defang::defang(&call.result))
    };

    println!("tool#{n:03} {}", call.tool_name);
    println!("--- input ---\n{input}");
    println!("--- result ---\n{result}");
    Ok(())
}

/// Launch `claude` seeded with an instruction to read the bundle. Best
/// effort: if `claude` is not on PATH we report the manual command.
fn launch_claude(bundle_dir: &Path) -> Result<()> {
    let seed = format!(
        "Read {}/narrative.md and {}/context-files.md, then continue where we left off. \
         Fetch archived tool calls with `reseed fetch <session> <N>` if you need detail.",
        bundle_dir.display(),
        bundle_dir.display()
    );
    eprintln!("[reseed] launching: claude \"<seed>\"");
    let status = std::process::Command::new("claude").arg(&seed).status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => bail!("claude exited with status {s}"),
        Err(_) => {
            println!("`claude` not found on PATH. Start it manually and paste:\n\n{seed}");
            Ok(())
        }
    }
}

fn savings_pct(full: usize, distilled: usize) -> f64 {
    if full == 0 {
        0.0
    } else {
        full.saturating_sub(distilled) as f64 / full as f64 * 100.0
    }
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME not set")
}

fn default_bundle_dir(session_id: &str) -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude/reseed").join(session_id))
}

/// List the immediate children of a directory as paths. Returns an empty
/// vec on any IO error (missing dir, permission): callers treat "no
/// children" and "unreadable" the same: nothing matched.
fn children(dir: &Path) -> Vec<PathBuf> {
    match fs::read_dir(dir) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Resolve a session argument to a transcript file. Accepts a direct path
/// to a `.jsonl`, or a session-id prefix searched under
/// `~/.claude/projects/*/`.
fn resolve_transcript(session: &str) -> Result<PathBuf> {
    let direct = Path::new(session);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }
    let projects = home_dir()?.join(".claude/projects");
    let mut matches = Vec::new();
    for project in children(&projects) {
        for p in children(&project) {
            let is_jsonl = p.extension().is_some_and(|e| e == "jsonl");
            let stem_matches = p
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.starts_with(session));
            if is_jsonl && stem_matches {
                matches.push(p);
            }
        }
    }
    select_unique_session(matches, session, &projects)
}

/// Pick the transcript to distill from prefix matches. Files sharing the
/// same stem are the *same* logical session living under more than one
/// project dir (resumed from a different cwd, or a sync copy): pick the
/// most recently modified. Distinct stems are a genuinely ambiguous prefix.
fn select_unique_session(
    mut matches: Vec<PathBuf>,
    session: &str,
    projects: &Path,
) -> Result<PathBuf> {
    if matches.is_empty() {
        bail!(
            "no transcript found for '{session}' under {}",
            projects.display()
        );
    }
    let stems: std::collections::HashSet<_> = matches
        .iter()
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .collect();
    if stems.len() > 1 {
        bail!(
            "'{session}' is ambiguous: {} distinct sessions match; pass a longer id or a full path",
            stems.len()
        );
    }
    // Same session in multiple project dirs: newest mtime wins.
    matches.sort_by_key(|p| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    Ok(matches.pop().expect("non-empty checked above"))
}

/// Resolve a session argument to a bundle directory (for `fetch`). Accepts
/// a direct directory path or a session-id under `~/.claude/reseed/`.
fn resolve_bundle_dir(session: &str) -> Result<PathBuf> {
    let direct = Path::new(session);
    if direct.is_dir() && direct.join("calls").is_dir() {
        return Ok(direct.to_path_buf());
    }
    let reseed = home_dir()?.join(".claude/reseed");
    let mut matches = Vec::new();
    for p in children(&reseed) {
        let name_matches = p
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| name.starts_with(session));
        if p.is_dir() && name_matches {
            matches.push(p);
        }
    }
    match matches.len() {
        0 => bail!(
            "no distilled bundle for '{session}' under {}",
            reseed.display()
        ),
        1 => Ok(matches.remove(0)),
        _ => bail!("'{session}' is ambiguous across {} bundles", matches.len()),
    }
}
