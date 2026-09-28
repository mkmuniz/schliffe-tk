use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// `schliffe report` (M10, 2026-09-27) — where the tokens of Claude Code
/// sessions actually go, from the session transcripts
/// (`~/.claude/projects/*/*.jsonl`). Read-only, nothing leaves the machine.
///
/// `schliffe stats` only sees what passes through Schliffe (a small slice);
/// this is the whole bill: re-reads vs new input vs output, the sessions
/// that cost the most, and what kind of content fills the conversations —
/// so the user can change the habits that matter (usually: session length).
///
/// Costs are "cost-equivalent tokens": each token type weighted by its
/// price relative to a fresh input token (cache read 0.1×, cache write
/// 1.25×, output 5×). Absolute prices vary by model; the proportions are
/// what matters here.
const W_CACHE_READ: f64 = 0.1;
const W_CACHE_WRITE: f64 = 1.25;
const W_OUTPUT: f64 = 5.0;

#[derive(Default, Clone, Copy)]
struct Usage {
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    /// Part of `output` spent thinking — billed, but not kept in the
    /// conversation, so never re-read.
    thinking: u64,
}

impl Usage {
    fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.output += o.output;
        self.thinking += o.thinking;
    }
    fn cost(&self) -> f64 {
        self.input as f64
            + self.cache_write as f64 * W_CACHE_WRITE
            + self.cache_read as f64 * W_CACHE_READ
            + self.output as f64 * W_OUTPUT
    }
    fn context(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

/// What kind of content entered the conversation (estimated tokens).
#[derive(Default)]
struct Sources {
    by: HashMap<String, u64>,
}

impl Sources {
    fn add(&mut self, key: impl Into<String>, tokens: u64) {
        *self.by.entry(key.into()).or_default() += tokens;
    }
}

struct Session {
    project: String,
    responses: u64,
    usage: Usage,
    peak_context: u64,
}

pub struct Report {
    days: u64,
    total: Usage,
    sessions: Vec<Session>,
    sources: Sources,
    schliffe_saved_tokens: u64,
}

pub fn projects_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SCHLIFFE_CLAUDE_PROJECTS") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn run(days: u64) -> String {
    let Some(dir) = projects_dir() else {
        return "schliffe: no HOME — can't find Claude Code's transcripts\n".into();
    };
    let since = now_secs().saturating_sub(days * 86_400);
    let report = build(
        &dir,
        since,
        days,
        crate::core::stats::saved_bytes_since(since) / 4,
    );
    render(&report)
}

fn build(dir: &Path, since: u64, days: u64, schliffe_saved_tokens: u64) -> Report {
    let mut total = Usage::default();
    let mut sessions = Vec::new();
    let mut sources = Sources::default();
    let Ok(projects) = std::fs::read_dir(dir) else {
        return Report {
            days,
            total,
            sessions,
            sources,
            schliffe_saved_tokens,
        };
    };
    for project in projects.flatten() {
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            // Files untouched since the period started can't contain it.
            let modified = file
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(u64::MAX);
            if modified < since {
                continue;
            }
            if let Some(s) = read_session(&path, since, &mut sources) {
                total.add(&s.usage);
                let project = if s.project.is_empty() {
                    project_name(&project.file_name().to_string_lossy())
                } else {
                    s.project.clone()
                };
                sessions.push(Session { project, ..s });
            }
        }
    }
    sessions.sort_by(|a, b| b.usage.cost().total_cmp(&a.usage.cost()));
    Report {
        days,
        total,
        sessions,
        sources,
        schliffe_saved_tokens,
    }
}

/// `-Users-jane-Desktop-acme-site` → `acme-site` (the last two segments,
/// which is where the project name usually is).
fn project_name(dir: &str) -> String {
    let parts: Vec<&str> = dir.split('-').filter(|p| !p.is_empty()).collect();
    match parts.len() {
        0 => dir.to_string(),
        1 => parts[0].to_string(),
        n => format!("{}-{}", parts[n - 2], parts[n - 1]),
    }
}

fn read_session(path: &Path, since: u64, sources: &mut Sources) -> Option<Session> {
    let file = std::fs::File::open(path).ok()?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut cwds: HashMap<String, u64> = HashMap::new();
    let mut s = Session {
        project: String::new(),
        responses: 0,
        usage: Usage::default(),
        peak_context: 0,
    };
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(e) = serde_json::from_str::<Value>(&line) else {
            continue; // fail-open on unknown lines
        };
        let in_period = e
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso)
            .is_none_or(|t| t >= since);
        if let Some(c) = e.get("cwd").and_then(Value::as_str) {
            *cwds.entry(c.to_string()).or_default() += 1;
        }
        let Some(msg) = e.get("message") else {
            continue;
        };

        // Tool names, needed to attribute tool results (keep even if the
        // call itself is just before the period).
        if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
            for b in blocks {
                if b.get("type").and_then(Value::as_str) == Some("tool_use")
                    && let (Some(id), Some(name)) = (
                        b.get("id").and_then(Value::as_str),
                        b.get("name").and_then(Value::as_str),
                    )
                {
                    tool_names.insert(id.to_string(), name.to_string());
                }
            }
        }
        if !in_period {
            continue;
        }

        // One model reply spans several lines (thinking, text, tool_use),
        // each repeating the same usage: count it once per message id.
        if let Some(u) = msg.get("usage") {
            let id = msg
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    e.get("requestId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| line.len().to_string());
            if seen.insert(id) {
                let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                let usage = Usage {
                    input: n("input_tokens"),
                    cache_read: n("cache_read_input_tokens"),
                    cache_write: n("cache_creation_input_tokens"),
                    output: n("output_tokens"),
                    thinking: u
                        .pointer("/output_tokens_details/thinking_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                };
                s.responses += 1;
                s.peak_context = s.peak_context.max(usage.context());
                s.usage.add(&usage);
                sources.add("model replies", usage.output.saturating_sub(usage.thinking));
            }
        }

        if e.get("type").and_then(Value::as_str) == Some("user") {
            attribute_user_content(msg, &tool_names, sources);
        }
    }
    // The folder the session mostly ran in; the transcript directory name
    // is a lossy encoding of it ("/" and "-" both become "-").
    if let Some((c, _)) = cwds
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
    {
        let home = std::env::var("HOME").unwrap_or_default();
        s.project = if c.trim_end_matches('/') == home.trim_end_matches('/') {
            "~".to_string()
        } else {
            c.trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|n| !n.is_empty())
                .unwrap_or(&c)
                .to_string()
        };
    }
    (s.responses > 0).then_some(s)
}

fn attribute_user_content(
    msg: &Value,
    tool_names: &HashMap<String, String>,
    sources: &mut Sources,
) {
    match msg.get("content") {
        Some(Value::String(text)) => sources.add("your prompts", text_tokens(text)),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => sources.add(
                        "your prompts",
                        text_tokens(b.get("text").and_then(Value::as_str).unwrap_or("")),
                    ),
                    Some("image") => sources.add("images (pasted)", image_block_tokens(b)),
                    Some("tool_result") => {
                        let tool = b
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .and_then(|id| tool_names.get(id))
                            .map(String::as_str)
                            .unwrap_or("?");
                        let (text, images) = tool_result_tokens(b.get("content"));
                        sources.add(source_for(tool), text);
                        if images > 0 {
                            sources.add("images (tools, screenshots)", images);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Groups tools into what a user can act on.
fn source_for(tool: &str) -> String {
    if let Some(rest) = tool.strip_prefix("mcp__") {
        let server = rest.split("__").next().unwrap_or(rest);
        return format!("MCP: {server}");
    }
    match tool {
        "Read" => "file reads (Read)".into(),
        "Bash" => "shell commands (Bash)".into(),
        "Grep" | "Glob" => "code search (Grep/Glob)".into(),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => "edits (Edit/Write)".into(),
        "WebFetch" | "WebSearch" => "web (WebFetch/WebSearch)".into(),
        "Agent" | "Task" => "subagents".into(),
        _ => "other tools".into(),
    }
}

fn text_tokens(s: &str) -> u64 {
    (s.len() / 4) as u64
}

/// (text tokens, image tokens) of a tool result's content.
fn tool_result_tokens(content: Option<&Value>) -> (u64, u64) {
    match content {
        Some(Value::String(s)) => (text_tokens(s), 0),
        Some(Value::Array(blocks)) => {
            let mut text = 0;
            let mut images = 0;
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("image") => images += image_block_tokens(b),
                    _ => text += text_tokens(b.get("text").and_then(Value::as_str).unwrap_or("")),
                }
            }
            (text, images)
        }
        _ => (0, 0),
    }
}

/// Billed tokens of an image block: by pixel area (after the API's own
/// downscale), NOT by the length of its base64 — a screenshot is ~1.5k
/// tokens, not the ~100k its base64 would suggest. PNG dimensions are read
/// from the header; anything else counts as a full-size image.
fn image_block_tokens(block: &Value) -> u64 {
    let data = block
        .pointer("/source/data")
        .or_else(|| block.get("data"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match png_dimensions(data) {
        Some((w, h)) => crate::bornes::hook::image::image_tokens(w, h),
        None => crate::bornes::hook::image::image_tokens(1568, 1000),
    }
}

fn png_dimensions(b64: &str) -> Option<(u32, u32)> {
    use base64::Engine;
    let head = b64.get(..44)?; // 33 bytes → covers the IHDR width/height
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(head)
        .ok()?;
    if bytes.get(..8)? != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let h = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    Some((w, h))
}

/// `2026-09-27T09:24:06.123Z` → Unix seconds (UTC). No timezone offsets
/// other than `Z` appear in transcripts.
fn parse_iso(s: &str) -> Option<u64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.trim_end_matches('Z').split(':');
    let (hh, mm) = (
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
    );
    let ss = t.next()?.split('.').next()?.parse::<i64>().ok()?;
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

// ---- rendering ----------------------------------------------------------

fn human(n: f64) -> String {
    match n {
        n if n >= 1e6 => format!("{:.1}M", n / 1e6),
        n if n >= 1e3 => format!("{:.0}k", n / 1e3),
        n => format!("{n:.0}"),
    }
}

fn pct(part: f64, whole: f64) -> String {
    if whole <= 0.0 {
        "—".into()
    } else {
        let p = part * 100.0 / whole;
        if p > 0.0 && p < 10.0 {
            format!("{p:.1}%")
        } else {
            format!("{p:.0}%")
        }
    }
}

fn render(r: &Report) -> String {
    let mut out = String::new();
    if r.sessions.is_empty() {
        return format!(
            "schliffe report — no Claude Code activity in the last {} day(s) (looked in ~/.claude/projects)\n",
            r.days
        );
    }
    let cost = r.total.cost();
    out.push_str(&format!(
        "schliffe report — last {} day(s), {} sessions, {} replies\n",
        r.days,
        r.sessions.len(),
        r.sessions.iter().map(|s| s.responses).sum::<u64>()
    ));
    out.push_str("(cost-equivalent tokens: cache re-read 0.1×, new input 1.25×, output 5×)\n\n");

    out.push_str("where the cost is\n");
    let parts = [
        (
            "re-reading the conversation (cache)",
            r.total.cache_read as f64 * W_CACHE_READ,
        ),
        (
            "new content entering it",
            r.total.cache_write as f64 * W_CACHE_WRITE + r.total.input as f64,
        ),
        (
            "the model's replies",
            r.total.output.saturating_sub(r.total.thinking) as f64 * W_OUTPUT,
        ),
        ("the model's thinking", r.total.thinking as f64 * W_OUTPUT),
    ];
    for (label, c) in parts.into_iter().filter(|(_, c)| *c > 0.0) {
        out.push_str(&format!(
            "  {label:<38} {:>8}  {:>4}\n",
            human(c),
            pct(c, cost)
        ));
    }
    out.push_str(&format!("  {:<38} {:>8}\n\n", "total", human(cost)));

    out.push_str("sessions that cost the most\n");
    for s in r.sessions.iter().take(5) {
        out.push_str(&format!(
            "  {:<30} {:>5} replies  peak context {:>6}  {:>8}  {:>4}\n",
            truncate(&s.project, 30),
            s.responses,
            human(s.peak_context as f64),
            human(s.usage.cost()),
            pct(s.usage.cost(), cost)
        ));
    }

    let mut sources: Vec<(&String, &u64)> = r.sources.by.iter().filter(|(_, t)| **t > 0).collect();
    sources.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let src_total: u64 = sources.iter().map(|(_, t)| **t).sum();
    out.push_str(
        "\nwhat fills the conversations (tokens added; each is then re-read every reply)\n",
    );
    for (label, t) in sources.iter().take(8) {
        out.push_str(&format!(
            "  {:<38} {:>8}  {:>4}\n",
            label,
            human(**t as f64),
            pct(**t as f64, src_total as f64)
        ));
    }

    // The slice Schliffe works on: command, MCP and tool image output.
    let tool_output: u64 = r
        .sources
        .by
        .iter()
        .filter(|(k, _)| {
            k.starts_with("MCP: ")
                || k.starts_with("shell commands")
                || k.starts_with("images (tools")
        })
        .map(|(_, t)| *t)
        .sum();
    let saved = r.schliffe_saved_tokens as f64;
    out.push_str(&format!(
        "\nschliffe cut ~{} tokens of command/MCP/image output ({} of what that output would have been) — {} of the total cost directly, more counting re-reads\n",
        human(saved),
        pct(saved, saved + tool_output as f64),
        pct(saved, cost)
    ));

    let tips = tips(r, cost);
    if !tips.is_empty() {
        out.push_str("\nbiggest levers\n");
        for t in tips {
            out.push_str(&format!("  • {t}\n"));
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}

/// At most three, each tied to a number in the report.
fn tips(r: &Report, cost: f64) -> Vec<String> {
    let mut out = Vec::new();
    let reread = r.total.cache_read as f64 * W_CACHE_READ;
    let long: Vec<&Session> = r
        .sessions
        .iter()
        .filter(|s| s.peak_context >= 300_000)
        .collect();
    if !long.is_empty() && reread / cost > 0.5 {
        out.push(format!(
            "{} session(s) passed 300k tokens of context and re-reading is {} of the cost: start a new conversation per task, /compact at milestones",
            long.len(),
            pct(reread, cost)
        ));
    }
    let src_total: u64 = r.sources.by.values().sum();
    if let Some((mcp, t)) = r
        .sources
        .by
        .iter()
        .filter(|(k, _)| k.starts_with("MCP: "))
        .max_by_key(|(_, t)| **t)
        && (*t as f64) > src_total as f64 * 0.2
    {
        out.push(format!(
            "{mcp} is {} of the content added: request smaller pieces (a section instead of a whole page) and keep that work in its own session",
            pct(*t as f64, src_total as f64)
        ));
    }
    if let Some(t) = r.sources.by.get("file reads (Read)")
        && (*t as f64) > src_total as f64 * 0.3
    {
        out.push(format!(
            "file reads are {} of the content added: point the model at specific files/lines instead of letting it read broadly",
            pct(*t as f64, src_total as f64)
        ));
    }
    out.truncate(3);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: Value) -> String {
        format!("{v}\n")
    }

    /// `name` keeps each test in its own folder: tests run in parallel, and
    /// a shared folder was deleted by one test while another wrote to it
    /// (failed on CI, 2026-09-27).
    fn fixture(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("schliffe-report-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("-Users-jane-Desktop-acme-site");
        std::fs::create_dir_all(&p).unwrap();
        let usage = |cr: u64, cw: u64, out: u64| serde_json::json!({"input_tokens": 1, "cache_read_input_tokens": cr, "cache_creation_input_tokens": cw, "output_tokens": out});
        let mut s = String::new();
        // One reply split over two lines (thinking + tool_use), same id.
        s += &line(
            serde_json::json!({"type":"assistant","cwd":"/Users/jane/work/acme-site","timestamp":"2026-09-27T10:00:00.000Z","message":{"id":"m1","usage":usage(100_000,2_000,400),"content":[{"type":"thinking","thinking":"x"}]}}),
        );
        s += &line(
            serde_json::json!({"type":"assistant","timestamp":"2026-09-27T10:00:00.000Z","message":{"id":"m1","usage":usage(100_000,2_000,400),"content":[{"type":"tool_use","id":"t1","name":"mcp__figma__get_design_context","input":{}}]}}),
        );
        s += &line(
            serde_json::json!({"type":"user","timestamp":"2026-09-27T10:00:05.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"x".repeat(40_000)}]}}),
        );
        s += &line(
            serde_json::json!({"type":"assistant","timestamp":"2026-09-27T10:01:00.000Z","message":{"id":"m2","usage":usage(350_000,1_000,200),"content":[{"type":"text","text":"ok"}]}}),
        );
        s += &line(
            serde_json::json!({"type":"user","timestamp":"2026-09-27T10:02:00.000Z","message":{"role":"user","content":"please fix the header"}}),
        );
        s += "not json at all\n";
        std::fs::write(p.join("s1.jsonl"), s).unwrap();
        dir
    }

    #[test]
    fn totals_dedupe_split_replies_and_attribute_sources() {
        let dir = fixture("totals");
        let r = build(&dir, 0, 7, 500);
        assert_eq!(r.sessions.len(), 1);
        let s = &r.sessions[0];
        assert_eq!(s.project, "acme-site");
        assert_eq!(s.responses, 2); // m1 counted once
        assert_eq!(r.total.cache_read, 450_000);
        assert_eq!(s.peak_context, 351_001);
        assert_eq!(r.sources.by["MCP: figma"], 10_000);
        assert_eq!(r.sources.by["your prompts"], 5);
        assert_eq!(r.sources.by["model replies"], 600);
        let text = render(&r);
        assert!(text.contains("re-reading the conversation"));
        assert!(text.contains("acme-site"));
        assert!(text.contains("MCP: figma"));
        assert!(text.contains("passed 300k tokens"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn period_filter_uses_timestamps() {
        let dir = fixture("period");
        let after_all = parse_iso("2026-09-28T00:00:00Z").unwrap();
        let r = build(&dir, after_all, 1, 0);
        assert!(r.sessions.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn iso_timestamps() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-09-27T09:24:06.123Z"), Some(1_790_501_046));
        assert_eq!(parse_iso("garbage"), None);
    }

    #[test]
    fn images_count_by_pixels_not_base64_length() {
        use base64::Engine;
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        png.extend_from_slice(&1440u32.to_be_bytes());
        png.extend_from_slice(&466u32.to_be_bytes());
        png.extend_from_slice(&[0u8; 200_000]);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let block = serde_json::json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":b64}});
        let t = image_block_tokens(&block);
        assert!((800..=1000).contains(&t), "{t}"); // ~1440×466/750, not ~66k
    }
}
