//! `emit.log`: one TSV row per reload-relevant event, `ts tier=... sid=<8>
//! job=... cwd=... arm=... gen=...`.

use anyhow::Result;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    pub tier: String,
    pub sid: String,
    pub job: String,
    pub cwd: String,
    pub arm: String,
    pub gen: String,
}

/// Append one TSV row to `emit.log`, timestamped `%FT%TZ` in UTC.
pub fn log(
    log_path: &Path,
    tier: &str,
    sid: &str,
    job: &str,
    cwd: &str,
    arm: &str,
    gen: &str,
) -> Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let ts = utc_timestamp();
    let sid8: String = sid.chars().take(8).collect();
    let row =
        format!("{ts}\ttier={tier}\tsid={sid8}\tjob={job}\tcwd={cwd}\tarm={arm}\tgen={gen}\n");
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    f.write_all(row.as_bytes())?;
    Ok(())
}

/// UTC timestamp in `%FT%TZ` form (e.g. `2026-09-05T13:05:00Z`), no chrono
/// dependency: a hand rollover from Unix seconds.
fn utc_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Howard Hinnant's `civil_from_days`: days since epoch to (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Find the newest row for `arm_sid` at or after `since`, parsed into a
/// `Tier`. Used to detect a `-done` delivery after a reload.
pub fn verified_since(log_path: &Path, arm_sid: &str, since: SystemTime) -> Result<Option<Tier>> {
    let since_secs = since
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let f = match std::fs::File::open(log_path) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let sid8: String = arm_sid.chars().take(8).collect();
    let mut found = None;
    for line in BufReader::new(f).lines().map_while(std::result::Result::ok) {
        let Some(tier) = parse_row(&line) else {
            continue;
        };
        if tier.sid != sid8 {
            continue;
        }
        let Some(ts_field) = line.split('\t').next() else {
            continue;
        };
        if parse_ts_secs(ts_field).unwrap_or(0) < since_secs {
            continue;
        }
        found = Some(tier);
    }
    Ok(found)
}

fn parse_row(line: &str) -> Option<Tier> {
    let mut fields = line.split('\t');
    let _ts = fields.next()?;
    let get = |prefix: &str| -> Option<String> {
        fields
            .clone()
            .find(|f| f.starts_with(prefix))
            .map(|f| f[prefix.len()..].to_string())
    };
    Some(Tier {
        tier: get("tier=")?,
        sid: get("sid=")?,
        job: get("job=").unwrap_or_default(),
        cwd: get("cwd=").unwrap_or_default(),
        arm: get("arm=").unwrap_or_default(),
        gen: get("gen=").unwrap_or_default(),
    })
}

/// One parsed `emit.log` row, `ts` kept alongside the `Tier` fields so a
/// caller (the `watch` audit) can window or order by time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRow {
    pub ts: String,
    pub tier: String,
    pub sid: String,
    pub job: String,
    pub cwd: String,
    pub arm: String,
    pub gen: String,
}

/// Every row in `emit.log`, oldest first. A missing file is an empty vec,
/// not an error: an unseeded host has simply never emitted.
pub fn read_rows(log_path: &Path) -> Result<Vec<LogRow>> {
    let f = match std::fs::File::open(log_path) {
        Ok(f) => f,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).lines().map_while(std::result::Result::ok) {
        if let Some(row) = parse_full_row(&line) {
            out.push(row);
        }
    }
    Ok(out)
}

fn parse_full_row(line: &str) -> Option<LogRow> {
    let ts = line.split('\t').next()?.to_string();
    let t = parse_row(line)?;
    Some(LogRow {
        ts,
        tier: t.tier,
        sid: t.sid,
        job: t.job,
        cwd: t.cwd,
        arm: t.arm,
        gen: t.gen,
    })
}

/// Seconds since the Unix epoch for a `%FT%TZ` timestamp. Public so callers
/// windowing `emit.log` (the `watch` audit's `--since`) share this parser.
/// Transcript lines stamp milliseconds on the same shape, so a fractional
/// second is dropped rather than rejected.
pub fn parse_ts_secs(ts: &str) -> Option<u64> {
    // %FT%TZ, e.g. 2026-09-05T13:05:00Z. Parsed by hand to avoid a chrono dep.
    let ts = ts.strip_suffix('Z')?;
    let (date, time) = ts.split_once('T')?;
    let mut d = date.split('-');
    let (y, mo, day): (i64, u32, u32) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let mut t = time.split(':');
    let (h, mi): (u64, u64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let sec = t.next()?;
    let s: u64 = sec
        .split_once('.')
        .map_or(sec, |(whole, _)| whole)
        .parse()
        .ok()?;
    let days = days_from_civil(y, mo, day);
    Some(days as u64 * 86_400 + h * 3600 + mi * 60 + s)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Transcript lines stamp milliseconds; the `watch` audit correlates
    /// them against whole-second `emit.log` rows through this one parser.
    #[test]
    fn a_fractional_second_parses_to_the_same_whole_second() {
        let whole = parse_ts_secs("2026-08-30T22:17:28Z").unwrap();
        assert_eq!(parse_ts_secs("2026-08-30T22:17:28.651Z"), Some(whole));
        assert_eq!(parse_ts_secs("2026-08-30T22:17:28.000Z"), Some(whole));
        // The fraction is never read, so only the whole second is validated.
        assert_eq!(parse_ts_secs("2026-08-30T22:17:28.xyzZ"), Some(whole));
        assert_eq!(parse_ts_secs("2026-08-30T22:17:xxZ"), None);
    }

    #[test]
    fn log_writes_a_tsv_row() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("emit.log");
        log(&path, "1", "abcdef1234", "job1", "/cwd", "yes", "3").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("tier=1"));
        assert!(content.contains("sid=abcdef12")); // 8-char truncation
        assert!(content.contains("job=job1"));
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn log_appends_multiple_rows() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("emit.log");
        log(&path, "1", "sid", "j", "c", "a", "g").unwrap();
        log(&path, "2", "sid", "j", "c", "a", "g").unwrap();
        let lines: Vec<_> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn verified_since_finds_a_done_row() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("emit.log");
        let before = SystemTime::now();
        log(&path, "2", "abcdef1234", "j", "c", "a", "g").unwrap();
        log(&path, "2-done", "abcdef1234", "j", "c", "a", "g").unwrap();
        let found = verified_since(&path, "abcdef1234", before).unwrap();
        assert_eq!(found.unwrap().tier, "2-done");
    }

    #[test]
    fn verified_since_ignores_other_sessions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("emit.log");
        let before = SystemTime::now();
        log(&path, "1-done", "other1234", "j", "c", "a", "g").unwrap();
        assert!(verified_since(&path, "abcdef1234", before)
            .unwrap()
            .is_none());
    }

    #[test]
    fn missing_log_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.log");
        assert!(verified_since(&path, "sid", SystemTime::now())
            .unwrap()
            .is_none());
    }
}
