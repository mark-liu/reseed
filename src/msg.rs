//! Operator-facing message texts, read at runtime rather than compiled in.
//! This crate is public, so a site's own wording lives in `$RESEED_MESSAGES`
//! (else `~/.claude/reseed/messages`) as `<name>.txt`, and the literal in the
//! caller is the public-safe fallback. Editing a message needs no rebuild.

use std::path::PathBuf;

fn dir() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RESEED_MESSAGES") {
        return Some(PathBuf::from(p));
    }
    crate::paths::reseed_dir().ok().map(|d| d.join("messages"))
}

/// The template for `name`, or `default` when no override file exists. A
/// trailing newline is stripped: the call site owns the layout around it.
pub fn text(name: &str, default: &str) -> String {
    let Some(d) = dir() else {
        return default.to_string();
    };
    match std::fs::read_to_string(d.join(format!("{name}.txt"))) {
        Ok(s) => s.trim_end_matches('\n').to_string(),
        Err(_) => default.to_string(),
    }
}

/// Substitute `{key}` placeholders. An unknown placeholder is left alone, so
/// a typo in an override shows up in the output instead of vanishing.
pub fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_override_yields_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        temp_env(tmp.path(), || {
            assert_eq!(text("nothing-here", "fallback"), "fallback");
        });
    }

    #[test]
    fn an_override_file_wins_and_loses_its_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("banner.txt"), "site wording\n\n").unwrap();
        temp_env(tmp.path(), || {
            assert_eq!(text("banner", "compiled default"), "site wording");
        });
    }

    #[test]
    fn fill_substitutes_known_keys_and_leaves_unknown_ones() {
        let out = fill(
            "gen {gen} for {op}, {mystery}",
            &[("gen", "3"), ("op", "Mark")],
        );
        assert_eq!(out, "gen 3 for Mark, {mystery}");
    }

    /// `RESEED_MESSAGES` is process-global, so these tests must not overlap.
    fn temp_env(dir: &std::path::Path, f: impl FnOnce()) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("RESEED_MESSAGES", dir);
        f();
        std::env::remove_var("RESEED_MESSAGES");
    }
}
