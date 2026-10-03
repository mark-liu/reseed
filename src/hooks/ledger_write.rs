//! Direct-write detection for the park ledger: a redirect, `tee`, `cp`/`mv`,
//! `sed -i` or an interpreter opening the file for write is denied, because only
//! the sanctioned append helper takes the lock the expiry rotation holds.
//! Ported from `park-ledger-scope-guard.py` (`is_raw_ledger_write`); the two
//! must agree on the shared case table.

use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

fn ledger_ref() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?:~|\$HOME|\$\{HOME\}|/Users/[^/\s]+|/home/[^/\s]+)/scratch/parked(?:/|$)|(?:^|/)claude-memory/parked(?:/|$)",
        )
        .unwrap()
    })
}

fn assignment() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z_]\w*=").unwrap())
}

fn interpreter_write() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"['"](?:a|w|ab|wb|a\+|w\+|r\+)['"]|write_text|write_bytes|>>|\bprint\s*>|\bcat\s*>|O_APPEND|O_WRONLY|O_RDWR|O_CREAT|-i\b|appendFile|writeFile|createWriteStream|File\.(?:write|open)|IO\.write|\bopen\s*\(?[^,)]*,\s*['"]?\+?[>]"#,
        )
        .unwrap()
    })
}

fn shell_c_flag() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^-[A-Za-z]*c[A-Za-z]*$").unwrap())
}

fn sed_i_flag() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^-[A-Za-z]*i").unwrap())
}

fn fd_target() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:\d+-?|-)$").unwrap())
}

fn remote_text() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s|[>|;&]").unwrap())
}

const SANCTIONED: [&str; 2] = ["park-append.sh", "park"];
const REDIRECT_OPS: [&str; 7] = [">", ">>", ">|", "&>", "&>>", ">>&", "<>"];
const SEPARATORS: [&str; 9] = ["|", "||", "&&", "&", ";", ";;", "\n", "(", ")"];
const WRAPPERS: [&str; 9] = [
    "env", "sudo", "command", "exec", "nohup", "time", "builtin", "nice", "stdbuf",
];
const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "dash", "ksh"];
const INTERPRETERS: [&str; 8] = [
    "python",
    "python3",
    "perl",
    "ruby",
    "node",
    "osascript",
    "awk",
    "gawk",
];
const OPS: [&str; 13] = [
    "&>>", "<<<", "<<-", ">>&", ">>", "&&", "||", "<<", ">|", "&>", ">&", "<&", "<>",
];

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Op(String),
}

/// Shell-ish tokens: words with quotes stripped, operators, heredoc bodies and
/// `#` comments skipped. Not a full parser: it finds redirect targets and
/// command words.
fn tokenize(cmd: &str) -> Vec<Tok> {
    let c: Vec<char> = cmd.chars().collect();
    let n = c.len();
    let mut toks = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut pend: Vec<String> = Vec::new();
    let mut expect_delim = false;
    let mut i = 0;

    macro_rules! flush {
        () => {
            if started {
                let text = std::mem::take(&mut word);
                if expect_delim {
                    pend.push(text.clone());
                    expect_delim = false;
                }
                toks.push(Tok::Word(text));
            }
            word.clear();
            started = false;
        };
    }

    while i < n {
        let ch = c[i];
        if ch == '\'' {
            let mut j = i + 1;
            while j < n && c[j] != '\'' {
                j += 1;
            }
            word.extend(&c[i + 1..j.min(n)]);
            started = true;
            i = j + 1;
        } else if ch == '"' {
            i += 1;
            started = true;
            while i < n && c[i] != '"' {
                if c[i] == '\\' && i + 1 < n && matches!(c[i + 1], '"' | '\\' | '$' | '`') {
                    i += 1;
                }
                word.push(c[i]);
                i += 1;
            }
            i += 1;
        } else if ch == '\\' && i + 1 < n {
            if c[i + 1] != '\n' {
                word.push(c[i + 1]);
                started = true;
            }
            i += 2;
        } else if ch == ' ' || ch == '\t' {
            flush!();
            i += 1;
        } else if ch == '#' && !started {
            while i < n && c[i] != '\n' {
                i += 1;
            }
        } else if ch == '\n' {
            flush!();
            toks.push(Tok::Op("\n".into()));
            i += 1;
            for delim in std::mem::take(&mut pend) {
                while i < n {
                    let mut j = i;
                    while j < n && c[j] != '\n' {
                        j += 1;
                    }
                    let line: String = c[i..j].iter().collect();
                    i = (j + 1).min(n);
                    if line.trim() == delim {
                        break;
                    }
                }
            }
        } else if ";|&<>()`".contains(ch) {
            flush!();
            let rest: String = c[i..n.min(i + 3)].iter().collect();
            let op = OPS
                .iter()
                .find(|o| rest.starts_with(**o))
                .map(|o| o.to_string())
                .or_else(|| rest.starts_with(";;").then(|| ";;".to_string()))
                .unwrap_or_else(|| ch.to_string());
            if op == "<<" || op == "<<-" {
                expect_delim = true;
            }
            i += op.chars().count();
            // A backtick opens or closes a nested command, like a separator.
            toks.push(Tok::Op(if op == "`" { "(".into() } else { op }));
        } else {
            word.push(ch);
            started = true;
            i += 1;
        }
    }
    if started {
        toks.push(Tok::Word(word));
    }
    toks
}

/// Split on separators; per statement return (words, redirect targets).
fn statements(toks: &[Tok]) -> Vec<(Vec<String>, Vec<String>)> {
    let mut out = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut targets: Vec<String> = Vec::new();
    let mut k = 0;
    while k <= toks.len() {
        let sep =
            k == toks.len() || matches!(&toks[k], Tok::Op(o) if SEPARATORS.contains(&o.as_str()));
        if sep {
            if !words.is_empty() || !targets.is_empty() {
                out.push((std::mem::take(&mut words), std::mem::take(&mut targets)));
            }
        } else {
            match &toks[k] {
                Tok::Op(o) => {
                    if let Some(Tok::Word(w)) = toks.get(k + 1) {
                        let file_dup = o == ">&" && !fd_target().is_match(w);
                        if REDIRECT_OPS.contains(&o.as_str()) || file_dup {
                            targets.push(w.clone());
                            k += 1;
                        }
                    }
                }
                Tok::Word(w) => words.push(w.clone()),
            }
        }
        k += 1;
    }
    out
}

fn basename(w: &str) -> &str {
    w.rsplit('/').next().unwrap_or(w)
}

/// (basename of the command, its args), skipping assignments and wrappers.
fn command(words: &[String]) -> (String, Vec<String>) {
    let mut k = 0;
    while k < words.len() {
        let w = &words[k];
        if assignment().is_match(w)
            || WRAPPERS.contains(&w.as_str())
            || (k > 0 && w.starts_with('-'))
        {
            k += 1;
            continue;
        }
        return (basename(w).to_string(), words[k + 1..].to_vec());
    }
    (String::new(), Vec::new())
}

struct Ctx {
    in_dir: bool,
    assigned: HashSet<String>, // names set by VAR=... in this call
    tainted: HashSet<String>,  // the subset whose value references the ledger
    cwd: String,
}

/// `parked` as a whole path component: ~/scratch/parked-audit-x is not the ledger dir.
fn parked_component() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|/)parked(?:/|$|[^\w.\-])").unwrap())
}

fn assign_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?:^|[;&|(){\n`\s])(?:(?:export|local|declare|typeset|readonly)[ \t]+(?:-\w+[ \t]+)?)?([A-Za-z_]\w*)\+?=((?:\$\([^)]*\)|`[^`]*`|"[^"]*"|'[^']*'|[^;\n&|\s])*)"#,
        )
        .unwrap()
    })
}

fn var_use() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\$\{?([A-Za-z_]\w*)").unwrap())
}

/// (assigned names, names whose value references the ledger) for VAR=... in a call.
fn assignments(cmd: &str) -> (HashSet<String>, HashSet<String>) {
    let found: Vec<(String, String)> = assign_re()
        .captures_iter(cmd)
        .map(|m| (m[1].to_string(), m[2].to_string()))
        .collect();
    let mut tainted: HashSet<String> = found
        .iter()
        .filter(|(_, v)| parked_component().is_match(v))
        .map(|(n, _)| n.clone())
        .collect();
    // P=$D/x.md where D is tainted
    loop {
        let before = tainted.len();
        for (n, v) in &found {
            if var_use()
                .captures_iter(v)
                .any(|c| tainted.contains(c.get(1).unwrap().as_str()))
            {
                tainted.insert(n.clone());
            }
        }
        if tainted.len() == before {
            break;
        }
    }
    (found.into_iter().map(|(n, _)| n).collect(), tainted)
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn expand(arg: &str) -> String {
    let h = home();
    let a = arg.replace("${HOME}", &h).replace("$HOME", &h);
    if a == "~" {
        h
    } else if let Some(rest) = a.strip_prefix("~/") {
        format!("{h}/{rest}")
    } else {
        a
    }
}

/// Lexical normalisation, then the nearest existing ancestor canonicalised, so
/// symlinks resolve for a path whose last components do not exist yet.
fn real(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    let mut keep = parts.len();
    loop {
        let base = format!("/{}", parts[..keep].join("/"));
        if let Ok(canon) = std::fs::canonicalize(&base) {
            let rest = parts[keep..].join("/");
            let c = canon.to_string_lossy().to_string();
            return if rest.is_empty() {
                c
            } else {
                format!("{}/{}", c.trim_end_matches('/'), rest)
            };
        }
        if keep == 0 {
            return format!("/{}", parts.join("/"));
        }
        keep -= 1;
    }
}

fn resolve(arg: &str, cwd: &str) -> Option<String> {
    let p = expand(arg);
    if p.is_empty() || p.contains(['$', '`', '*', '?', '[']) {
        return None;
    }
    if p.starts_with('/') {
        return Some(real(&p));
    }
    if cwd.is_empty() {
        return None;
    }
    Some(real(&format!("{cwd}/{p}")))
}

fn ledger_dir() -> String {
    real(&format!("{}/scratch/parked", home()))
}

/// True when `arg` (a write target or victim) names a ledger file.
fn hit(arg: &str, ctx: &Ctx, dest: bool) -> bool {
    if let Some(full) = resolve(arg, &ctx.cwd) {
        let dir = ledger_dir();
        if full == dir {
            return dest;
        }
        if full.ends_with(".md") && full.starts_with(&format!("{dir}/")) {
            return true;
        }
    }
    if let Some(m) = ledger_ref().find(arg) {
        let rest = &arg[m.end()..];
        if rest.is_empty() {
            return dest;
        }
        return rest.ends_with(".md") || rest.contains(['$', '*', '?', '[']);
    }
    if ctx.in_dir && !arg.starts_with(['/', '~', '-']) {
        return arg.ends_with(".md") || arg.contains('$') || arg.contains('*');
    }
    let used: Vec<&str> = var_use()
        .captures_iter(arg)
        .map(|c| c.get(1).unwrap().as_str())
        .collect();
    used.iter().any(|v| ctx.tainted.contains(*v))
        || (parked_component().is_match(arg) && used.iter().any(|v| ctx.assigned.contains(*v)))
}

fn sed_in_place(args: &[String]) -> bool {
    args.iter()
        .filter(|a| a.starts_with('-'))
        .any(|a| a == "--in-place" || a.starts_with("--in-place=") || sed_i_flag().is_match(a))
}

/// True when the command writes the park ledger by any path but the helper.
pub fn is_raw_ledger_write(cmd: &str, cwd: &str) -> bool {
    raw_write(cmd, 0, cwd)
}

fn raw_write(cmd: &str, depth: u8, cwd: &str) -> bool {
    if depth > 3 {
        return false;
    }
    let stmts = statements(&tokenize(cmd));
    // A `$`-built target is suspect only via a variable whose value names the ledger
    // (or $D/parked/x.md with D assigned in this call).
    let (assigned, tainted) = assignments(cmd);
    let mut ctx = Ctx {
        in_dir: false,
        assigned,
        tainted,
        cwd: cwd.to_string(),
    };
    for (words, targets) in &stmts {
        if write_stmt(words, targets, &ctx, depth) {
            return true;
        }
        // cd takes effect for the statements after it.
        let (name, args) = command(words);
        if (name == "cd" || name == "pushd") && !args.is_empty() {
            if args.iter().any(|a| ledger_ref().is_match(a)) {
                ctx.in_dir = true;
            }
            if let Some(t) = resolve(&args[0], &ctx.cwd) {
                ctx.cwd = t;
            }
        }
    }
    false
}

/// The command xargs runs: the words after its own flags.
fn xargs_command(args: &[String]) -> Vec<String> {
    let mut k = 0;
    while k < args.len() && args[k].starts_with('-') {
        k += if matches!(
            args[k].as_str(),
            "-I" | "-n" | "-P" | "-L" | "-s" | "-d" | "-E"
        ) {
            2
        } else {
            1
        };
    }
    args.get(k..).unwrap_or(&[]).to_vec()
}

/// True when one statement (command words plus redirect targets) writes the ledger.
fn write_stmt(words: &[String], targets: &[String], ctx: &Ctx, depth: u8) -> bool {
    for w in words {
        if (w.contains("$(") || w.contains('`')) && raw_write(w, depth + 1, &ctx.cwd) {
            return true;
        }
    }
    // Even the sanctioned helper must not be redirected.
    if targets.iter().any(|t| hit(t, ctx, false)) {
        return true;
    }
    let (name, args) = command(words);
    if SANCTIONED.contains(&name.as_str()) {
        return false;
    }
    let plain: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let n = name.as_str();
    if n == "tee" && plain.iter().any(|a| hit(a, ctx, false)) {
        return true;
    }
    if matches!(n, "cp" | "install" | "rsync") && !plain.is_empty() {
        // A copy OUT of the ledger to a $-built name is a backup, not a write into it.
        let copy_out = plain[..plain.len() - 1].iter().any(|a| hit(a, ctx, false));
        let cctx = Ctx {
            in_dir: ctx.in_dir,
            assigned: ctx.assigned.clone(),
            tainted: if copy_out {
                HashSet::new()
            } else {
                ctx.tainted.clone()
            },
            cwd: ctx.cwd.clone(),
        };
        if hit(plain[plain.len() - 1], &cctx, true) {
            return true;
        }
    }
    // sort -o FILE / --output=FILE write a named file; patch and sponge rewrite theirs.
    let mut outs: Vec<&str> = Vec::new();
    for (k, a) in args.iter().enumerate() {
        if (a == "-o" || a == "--output") && k + 1 < args.len() {
            outs.push(&args[k + 1]);
        }
        if let Some(v) = a.strip_prefix("--output=") {
            outs.push(v);
        }
    }
    if outs.iter().any(|o| hit(o, ctx, false)) {
        return true;
    }
    if matches!(n, "patch" | "sponge") && plain.iter().any(|a| hit(a, ctx, false)) {
        return true;
    }
    // ln makes an alias that a later write reaches the ledger through.
    if matches!(
        n,
        "mv" | "rm" | "unlink" | "truncate" | "shred" | "ed" | "ex" | "vi" | "vim" | "ln"
    ) && plain.iter().any(|a| hit(a, ctx, true))
    {
        return true;
    }
    if n == "git" && plain.iter().any(|a| GIT_REWRITERS.contains(&a.as_str())) {
        // `git -C dir` moves where relative paths land.
        let mut gctx = Ctx {
            in_dir: ctx.in_dir,
            assigned: ctx.assigned.clone(),
            tainted: ctx.tainted.clone(),
            cwd: ctx.cwd.clone(),
        };
        for k in 0..args.len().saturating_sub(1) {
            if args[k] == "-C" {
                if let Some(t) = resolve(&args[k + 1], &gctx.cwd) {
                    gctx.cwd = t;
                }
            }
        }
        if plain.iter().any(|a| hit(a, &gctx, true)) {
            return true;
        }
    }
    if n == "dd"
        && args
            .iter()
            .any(|a| a.strip_prefix("of=").is_some_and(|t| hit(t, ctx, false)))
    {
        return true;
    }
    if matches!(n, "sed" | "perl" | "ruby")
        && sed_in_place(&args)
        && plain.iter().any(|a| hit(a, ctx, false))
    {
        return true;
    }
    if INTERPRETERS.contains(&n)
        && args
            .iter()
            .any(|a| ledger_ref().is_match(a) || a.contains("parked") || hit(a, ctx, false))
        && args.iter().any(|a| interpreter_write().is_match(a))
    {
        return true;
    }
    if SHELLS.contains(&n) {
        for k in 0..args.len().saturating_sub(1) {
            if shell_c_flag().is_match(&args[k]) && raw_write(&args[k + 1], depth + 1, &ctx.cwd) {
                return true;
            }
        }
    }
    if n == "eval" && raw_write(&args.join(" "), depth + 1, &ctx.cwd) {
        return true;
    }
    if (n == "ssh" || n == "scp")
        && args
            .iter()
            .any(|a| remote_text().is_match(a) && raw_write(a, depth + 1, &ctx.cwd))
    {
        return true;
    }
    if n == "xargs" {
        let sub = xargs_command(&args);
        if !sub.is_empty() && write_stmt(&sub, &[], ctx, depth + 1) {
            return true;
        }
    }
    if n == "find" {
        for (k, a) in args.iter().enumerate() {
            if matches!(a.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
                let sub: Vec<String> = args[k + 1..]
                    .iter()
                    .filter(|x| *x != ";" && *x != "+")
                    .cloned()
                    .collect();
                if write_stmt(&sub, &[], ctx, depth + 1) {
                    return true;
                }
            }
        }
    }
    false
}

const GIT_REWRITERS: [&str; 8] = [
    "checkout", "restore", "rm", "mv", "apply", "stash", "clean", "reset",
];

#[cfg(test)]
mod tests {
    use super::*;

    const L: &str = "~/scratch/parked/host-a.md";

    /// Run `f` under a fake HOME whose ~/scratch/parked links into ~/repos/claude-memory,
    /// holding the env lock: other tests set and clear HOME in parallel.
    fn with_ledger_home(f: impl FnOnce()) {
        let _held = crate::testlock::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("repos/claude-memory/parked");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(tmp.path().join("scratch")).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("scratch/parked")).unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        f();
        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn denied_write_shapes() {
        let deny = [
            format!("echo x >> {L}"),
            "echo x >> \"$HOME/scratch/parked/host-a.md\"".to_string(),
            format!("printf 'a\\n' | tee -a {L}"),
            format!("cat >> {L} <<'EOF'\nline\nEOF"),
            format!("cat <<'EOF' >> {L}\nline\nEOF"),
            format!("printf '%s\\n' x > {L}"),
            "cd ~/scratch/parked && echo x >> host-a.md".to_string(),
            format!("ssh host-b 'echo x >> {L}'"),
            format!("sh -c 'echo x >> {L}'"),
            "H=$(hostname -s); echo x >> ~/scratch/parked/$H.md".to_string(),
            "D=~/scratch/parked; echo x >> $D/host-a.md".to_string(),
            format!("cp /tmp/a {L}"),
            format!("sed -i '' s/a/b/ {L}"),
            format!("python3 -c \"open('{L}','a').write('x')\""),
            "mv /tmp/a ~/repos/claude-memory/parked/host-a.md".to_string(),
            format!("echo $(echo x >> {L})"),
            format!("~/scripts/park-append.sh >> {L}"),
            format!("printf x | xargs tee -a {L}"),
            format!("find /tmp -name a -exec tee -a {L} \\;"),
            format!("git checkout -- {L}"),
            format!("ln {L} /tmp/alias.md"),
            format!("echo x >& {L}"),
            "echo x >> ~/scratch/./parked/host-a.md".to_string(),
            "D=~/scratch; echo x >> \"$D/parked/host-a.md\"".to_string(),
            "git -C ~/repos/claude-memory restore parked/host-a.md".to_string(),
            format!("sort -o {L} {L}"),
            format!("patch {L} fix.diff"),
            "L=~/scratch/parked/$(scutil --get ComputerName).md; echo x >> \"$L\"".to_string(),
            "D=~/scratch/parked; echo x >> $D/bender.md".to_string(),
            "P=\"$HOME/scratch/parked\"; echo x >> \"$P/host-a.md\"".to_string(),
            "L=$(echo ~/scratch/parked/host-a.md); echo x >> \"$L\"".to_string(),
            "export D=~/scratch/parked; tee -a $D/host-a.md <<< x".to_string(),
            "A=1 B=~/scratch/parked/host-a.md; printf x > \"$B\"".to_string(),
            "{ P=~/scratch/parked/host-a.md; printf x > \"$P\"; }".to_string(),
            "D=~/scratch/parked; P=$D/host-a.md; printf x > \"$P\"".to_string(),
            "P=~/scratch; P+=/parked/host-a.md; printf x > \"$P\"".to_string(),
        ];
        with_ledger_home(|| {
            for c in deny {
                assert!(is_raw_ledger_write(&c, ""), "should deny: {c}");
            }
        });
    }

    #[test]
    fn allowed_shapes() {
        let allow = [
            "~/repos/claude-memory/scripts/park-append.sh <<'EOF'\n2026-10-03 | x\nEOF".to_string(),
            format!("cat {L} > /tmp/copy.md"),
            format!("echo 'x >> {L}'"),
            format!("git commit -m 'never echo >> {L}'"),
            format!("grep -c foo {L}"),
            format!("cp {L} /tmp/b.md"),
            format!("cat > /tmp/n.md <<'EOF'\necho x >> {L}\nEOF"),
            "ls ~/scratch/parked/".to_string(),
            format!("python3 -c \"print(open('{L}').read())\""),
            "cd ~/scratch/parked && grep -c foo host-a.md".to_string(),
            "echo x >> ~/scratch/notes.md".to_string(),
            format!("git add {L}"),
            format!("xargs grep -c foo {L}"),
            "echo x >&2".to_string(),
            "cp ~/scratch/parked/host-a.md copy.md; cd ~/scratch/parked".to_string(),
            "B=/tmp/bk; cp ~/scratch/parked/host-a.md \"$B/host-a.md\"".to_string(),
            "python3 -c 'import sys;sys.stdout.write(open(sys.argv[1]).read())' ~/scratch/parked/host-a.md".to_string(),
            "OUT=~/scratch/parked-audit-x; mkdir -p \"$OUT\"; python3 ~/scripts/parked-rotate.py --list > \"$OUT/dry.txt\"".to_string(),
            "TMP=$(mktemp); python3 ~/scripts/parked-rotate.py > $TMP; wc -l $TMP; rm -f $TMP".to_string(),
            "H=$(scutil --get ComputerName); grep -n foo ~/scratch/parked/$H.md | tee ~/scratch/out-$H.txt".to_string(),
            "export X=1; grep -rn parked project-memory > \"$HOME/scratch/hits-$X.txt\"".to_string(),
            "A=~/scratch/parked-audit; echo x >> \"$A/out.txt\"".to_string(),
        ];
        with_ledger_home(|| {
            for c in allow {
                assert!(!is_raw_ledger_write(&c, ""), "should allow: {c}");
            }
        });
    }
}
