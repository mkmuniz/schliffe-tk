use crate::core::secure::{open_append_private, write_private};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Savings log behind `schliffe stats` — answers "is Schliffe actually saving
/// anything while I work?" (2026-09-24). One tab-separated line per agent
/// command that reached Schliffe: `<unix ts>\t<key>\t<bytes before>\t<bytes
/// after>`, where a passthrough (no filter for that command) is logged with
/// both sizes as `-`. Only sizes and the command name (e.g. `git log`,
/// `pnpm build`) are stored — never arguments or output, which may contain
/// secrets. Kept outside the store (`schliffe store clear` doesn't wipe it);
/// `SCHLIFFE_NO_STATS=1` turns it off.
const MAX_AGE_DAYS: u64 = 90;
/// Past this size the log is compacted (entries older than MAX_AGE_DAYS
/// dropped) on the next write — keeps it bounded without a daemon.
const COMPACT_ABOVE_BYTES: u64 = 4 * 1024 * 1024;

fn stats_file() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SCHLIFFE_STATS_FILE") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".schliffe").join("stats.log"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn disabled() -> bool {
    std::env::var_os("SCHLIFFE_NO_STATS").is_some_and(|v| !v.is_empty())
}

/// Short, argument-free name for a command: `git log`, `pnpm build`,
/// `npm run build`, `docker compose build`. Flags and paths never make it in.
pub fn command_key(invoked_name: &str, args: &[String]) -> String {
    let mut key = invoked_name.to_string();
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .take_while(|a| !a.starts_with('-'))
        .take(2)
        .collect();
    if let Some(first) = words.first() {
        key.push(' ');
        key.push_str(first);
        if matches!(*first, "run" | "compose" | "run-script")
            && let Some(second) = words.get(1)
        {
            key.push(' ');
            key.push_str(second);
        }
    }
    // Keep the log line well-formed whatever the arguments were.
    key.replace(['\t', '\n'], " ")
}

/// Records a filtered command: sizes before and after Schliffe.
pub fn record(key: &str, before: usize, after: usize) {
    append(&format!("{}\t{key}\t{before}\t{after}\n", now_secs()));
}

/// Records a command an agent ran that had no filter (passed through).
pub fn record_passthrough(key: &str) {
    append(&format!("{}\t{key}\t-\t-\n", now_secs()));
}

fn append(line: &str) {
    if disabled() {
        return;
    }
    let Some(path) = stats_file() else { return };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::metadata(&path).is_ok_and(|m| m.len() > COMPACT_ABOVE_BYTES) {
        compact(&path);
    }
    // Fail-open: a stats write error never affects the command itself.
    // A single small `write` in append mode, so concurrent shims don't
    // interleave within a line. Owner-only: the log lists which commands
    // ran and when.
    if let Ok(mut f) = open_append_private(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn compact(path: &PathBuf) {
    let cutoff = now_secs().saturating_sub(MAX_AGE_DAYS * 24 * 60 * 60);
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    let kept: String = content
        .lines()
        .filter(|l| parse(l).is_some_and(|e| e.ts >= cutoff))
        .map(|l| format!("{l}\n"))
        .collect();
    let _ = write_private(path, kept.as_bytes());
}

struct Entry<'a> {
    ts: u64,
    key: &'a str,
    /// `None` for a passthrough.
    sizes: Option<(u64, u64)>,
}

fn parse(line: &str) -> Option<Entry<'_>> {
    let mut parts = line.split('\t');
    let ts = parts.next()?.parse().ok()?;
    let key = parts.next()?;
    let before = parts.next()?;
    let after = parts.next()?;
    let sizes = match (before.parse(), after.parse()) {
        (Ok(b), Ok(a)) => Some((b, a)),
        _ if before == "-" => None,
        _ => return None,
    };
    Some(Entry { ts, key, sizes })
}

#[derive(Default)]
struct Totals {
    commands: u64,
    filtered: u64,
    before: u64,
    after: u64,
}

impl Totals {
    fn add(&mut self, e: &Entry) {
        self.commands += 1;
        if let Some((b, a)) = e.sizes {
            self.filtered += 1;
            self.before += b;
            self.after += a;
        }
    }
    fn saved(&self) -> u64 {
        self.before.saturating_sub(self.after)
    }
    fn pct(&self) -> String {
        if self.before == 0 {
            "—".into()
        } else {
            format!("-{:.0}%", self.saved() as f64 * 100.0 / self.before as f64)
        }
    }
}

fn human_bytes(b: u64) -> String {
    match b {
        b if b >= 1024 * 1024 => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
        b if b >= 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// Tokens estimated as bytes/4 — the same approximation used everywhere else
/// in this project (specs §5.4.1), not a real tokenizer.
fn human_tokens(bytes: u64) -> String {
    let t = bytes / 4;
    match t {
        t if t >= 1_000_000 => format!("{:.1}M", t as f64 / 1_000_000.0),
        t if t >= 1_000 => format!("{:.1}k", t as f64 / 1_000.0),
        t => t.to_string(),
    }
}

/// Bytes Schliffe removed since `since` (for `schliffe report`).
pub fn saved_bytes_since(since: u64) -> u64 {
    let content = stats_file()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();
    content
        .lines()
        .filter_map(parse)
        .filter(|e| e.ts >= since)
        .filter_map(|e| e.sizes)
        .map(|(b, a)| b.saturating_sub(a))
        .sum()
}

/// The `schliffe stats` report.
pub fn report() -> String {
    let path = stats_file();
    let content = path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();
    report_from(&content, now_secs(), disabled())
}

fn report_from(content: &str, now: u64, disabled: bool) -> String {
    let entries: Vec<Entry> = content.lines().filter_map(parse).collect();
    let mut out = String::new();
    if disabled {
        out.push_str(
            "schliffe: stats are OFF (SCHLIFFE_NO_STATS is set) — nothing new is recorded\n\n",
        );
    }
    if entries.is_empty() {
        out.push_str(
            "schliffe: no agent commands recorded yet.\n\
             Commands are recorded when an AI agent (Claude Code, or anything setting\n\
             AI_AGENT / SCHLIFFE_FORCE) runs a shimmed command. Check it's active in the\n\
             agent's shell with `which git` — it should print ~/.schliffe/shims/git.\n",
        );
        return out;
    }

    let periods = [
        ("last 24h", now.saturating_sub(24 * 3600)),
        ("last 7 days", now.saturating_sub(7 * 24 * 3600)),
        ("all time", 0),
    ];
    out.push_str("schliffe stats — commands run by AI agents\n\n");
    out.push_str(&format!(
        "{:<12} {:>8} {:>8} {:>10} {:>10} {:>6} {:>13}\n",
        "", "commands", "filtered", "before", "after", "saved", "~tokens saved"
    ));
    for (label, since) in periods {
        let mut t = Totals::default();
        entries
            .iter()
            .filter(|e| e.ts >= since)
            .for_each(|e| t.add(e));
        out.push_str(&format!(
            "{:<12} {:>8} {:>8} {:>10} {:>10} {:>6} {:>13}\n",
            label,
            t.commands,
            t.filtered,
            human_bytes(t.before),
            human_bytes(t.after),
            t.pct(),
            human_tokens(t.saved()),
        ));
    }

    let mut per_key: HashMap<&str, Totals> = HashMap::new();
    for e in &entries {
        per_key.entry(e.key).or_default().add(e);
    }

    let mut savers: Vec<(&&str, &Totals)> =
        per_key.iter().filter(|(_, t)| t.filtered > 0).collect();
    // Ties broken by name so the report is deterministic.
    savers.sort_by(|a, b| b.1.saved().cmp(&a.1.saved()).then(a.0.cmp(b.0)));
    if !savers.is_empty() {
        out.push_str("\ntop savings (all time):\n");
        for (key, t) in savers.iter().take(10) {
            out.push_str(&format!(
                "  {:<24} {:>5}×  {:>10} → {:>10}  {:>5}\n",
                key,
                t.filtered,
                human_bytes(t.before),
                human_bytes(t.after),
                t.pct()
            ));
        }
    }

    let mut unfiltered: Vec<(&&str, u64)> = per_key
        .iter()
        .map(|(k, t)| (k, t.commands - t.filtered))
        .filter(|(_, n)| *n > 0)
        .collect();
    unfiltered.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    if !unfiltered.is_empty() {
        out.push_str("\npassed through with no filter (candidates for a new rule):\n  ");
        let list: Vec<String> = unfiltered
            .iter()
            .take(12)
            .map(|(k, n)| format!("{k} {n}×"))
            .collect();
        out.push_str(&list.join(", "));
        out.push('\n');
    }
    out.push_str("\n(tokens estimated as bytes/4)\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_key_never_includes_flags_or_extra_args() {
        assert_eq!(
            command_key("git", &args(&["log", "-5", "--stat"])),
            "git log"
        );
        assert_eq!(command_key("pnpm", &args(&["build"])), "pnpm build");
        assert_eq!(
            command_key("npm", &args(&["run", "build", "--", "x"])),
            "npm run build"
        );
        assert_eq!(
            command_key("docker", &args(&["compose", "build", "api"])),
            "docker compose build"
        );
        assert_eq!(command_key("git", &args(&["show", "abc123"])), "git show");
        assert_eq!(command_key("git", &args(&["--version"])), "git");
    }

    #[test]
    fn report_totals_and_rankings() {
        let now = 10 * 24 * 3600;
        let log = format!(
            "{now}\tgit log\t1000\t100\n\
             {now}\tgit log\t1000\t100\n\
             {now}\tpnpm build\t800\t200\n\
             {now}\tpnpm dev\t-\t-\n\
             {old}\tgit log\t4000\t400\n\
             garbage line\n",
            old = now - 9 * 24 * 3600
        );
        let r = report_from(&log, now, false);
        let line = |label: &str| {
            r.lines()
                .find(|l| l.starts_with(label))
                .unwrap()
                .to_string()
        };
        // last 24h: 4 commands, 3 filtered, 2800 -> 400 bytes
        assert!(line("last 24h").contains("2.7 KB"), "{r}");
        assert!(line("last 24h").contains("-86%"), "{r}");
        assert!(line("all time").contains("6.6 KB"), "{r}");
        let top = r.split("top savings").nth(1).unwrap();
        assert!(top.find("git log").unwrap() < top.find("pnpm build").unwrap());
        assert!(r.contains("pnpm dev 1×"));
    }

    #[test]
    fn empty_log_explains_how_to_check() {
        let r = report_from("", 0, false);
        assert!(r.contains("which git"));
    }
}
