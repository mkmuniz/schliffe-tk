pub mod logs;

use crate::core::{stats, store};
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

/// `bornes/prompt` — Claude Code `UserPromptSubmit` hook (2026-09-26). Runs
/// before a prompt reaches the model and does two things, both without any
/// model:
///
/// 1. **Pasted logs.** Recognizes the kind of log pasted into the prompt
///    (stack traces, tracebacks, timestamped app logs...) and builds a
///    compact version that keeps every error. Claude Code doesn't let a hook
///    rewrite a prompt, so in the default `block` mode the prompt is held
///    back, the compact version of the WHOLE prompt goes to the clipboard,
///    and the user pastes and sends it. Sending the same prompt again goes
///    through untouched. The original log is stored, so the compact version
///    carries a `schliffe show <hash>` line to get it back.
/// 2. **Long conversations.** Reads the session's real context size from
///    the transcript and, once per threshold (200k, 400k, 600k, 800k
///    tokens), tells the user that every reply now re-reads all of it.
///
/// Messages go to the user only (`systemMessage` / block `reason`), never
/// into the model's context. Fail-open: any error means "let the prompt
/// through, say nothing".
pub fn run_user_prompt_submit() -> ExitCode {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return ExitCode::SUCCESS;
    }
    let Ok(input) = serde_json::from_str::<Value>(&raw) else {
        return ExitCode::SUCCESS;
    };
    let prompt = input.get("prompt").and_then(Value::as_str).unwrap_or("");
    let session = input
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let mut state = State::load(session);

    let response = handle(prompt, &input, &mut state, &Env::from_process());
    state.save(session);
    if let Some(r) = response {
        println!("{r}");
    }
    ExitCode::SUCCESS
}

/// Settings, read once (and injectable in tests).
struct Env {
    log_mode: LogMode,
    warn_at: Vec<u64>,
    /// Clipboard, files and store writes. Off in tests, so they never touch
    /// the user's clipboard or ~/.schliffe.
    effects: bool,
}

#[derive(PartialEq, Clone, Copy)]
enum LogMode {
    Block,
    Tip,
    Off,
}

impl Env {
    fn from_process() -> Self {
        let log_mode = match std::env::var("SCHLIFFE_PROMPT_LOGS").as_deref() {
            Ok("tip") => LogMode::Tip,
            Ok("off") | Ok("0") => LogMode::Off,
            _ => LogMode::Block,
        };
        let warn_at = match std::env::var("SCHLIFFE_CONTEXT_WARN_AT") {
            Ok(v) if v == "0" || v == "off" => Vec::new(),
            Ok(v) => v.split(',').filter_map(|n| n.trim().parse().ok()).collect(),
            Err(_) => vec![200_000, 400_000, 600_000, 800_000],
        };
        Env {
            log_mode,
            warn_at,
            effects: true,
        }
    }
}

/// Per-session memory between prompts.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct State {
    /// Highest context threshold already warned about.
    #[serde(default)]
    warned_at: u64,
    /// Hash of the last prompt held back — sending it again lets it through.
    #[serde(default)]
    blocked_hash: String,
    /// A compact version offered and not yet seen: marker id, bytes before/after.
    #[serde(default)]
    offered: Option<(String, usize, usize)>,
    /// Language of the user's last prompt that clearly had one — short
    /// prompts ("ok, continua") don't, and reuse it.
    #[serde(default)]
    portuguese: Option<bool>,
}

fn state_path(session: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let safe: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(
        PathBuf::from(home)
            .join(".schliffe")
            .join("state")
            .join(format!("prompt-{safe}.json")),
    )
}

impl State {
    fn load(session: &str) -> Self {
        state_path(session)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
    fn save(&self, session: &str) {
        if let Some(p) = state_path(session) {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(s) = serde_json::to_string(self) {
                let _ = std::fs::write(p, s);
            }
        }
    }
}

/// Minimum size for a pasted block to be worth compacting.
const MIN_LINES: usize = 20;
const MIN_BYTES: usize = 1500;
/// Only offer the compact version when it's at most this share of the original.
const MAX_RATIO: f64 = 0.6;

fn handle(prompt: &str, input: &Value, state: &mut State, env: &Env) -> Option<Value> {
    let pt = match language(prompt) {
        Some(lang) => {
            state.portuguese = Some(lang);
            lang
        }
        None => state.portuguese.unwrap_or_else(system_prefers_portuguese),
    };

    // The user sent a compact version we offered: count the saving.
    if let Some((id, before, after)) = state.offered.clone()
        && prompt.contains(&id)
    {
        stats::record("prompt: pasted log", before, after);
        state.offered = None;
    }

    if env.log_mode != LogMode::Off
        && let Some(offer) = build_offer(prompt, input.get("cwd").and_then(Value::as_str), |s| {
            if env.effects {
                store::put(s)
            } else {
                short_hash(s)
            }
        })
    {
        let hash = short_hash(prompt);
        if env.log_mode == LogMode::Block && state.blocked_hash != hash {
            state.blocked_hash = hash;
            state.offered = Some((offer.id.clone(), offer.before, offer.after));
            let (copied, saved) = if env.effects {
                (
                    copy_to_clipboard(&offer.prompt),
                    save_prompts(prompt, &offer.prompt),
                )
            } else {
                (true, None)
            };
            return Some(json!({
                "decision": "block",
                "reason": block_message(&offer, copied, saved.as_deref(), pt),
            }));
        }
        if env.log_mode == LogMode::Tip {
            return Some(json!({ "systemMessage": tip_message(&offer, pt) }));
        }
        // Same prompt sent again after a block: the user wants the original.
        state.blocked_hash.clear();
    }

    let transcript = input.get("transcript_path").and_then(Value::as_str)?;
    let context = context_tokens(transcript)?;
    let level = env
        .warn_at
        .iter()
        .copied()
        .filter(|t| context >= *t)
        .max()?;
    if level <= state.warned_at {
        return None;
    }
    state.warned_at = level;
    Some(json!({ "systemMessage": context_message(context, pt) }))
}

// ---- pasted logs ----------------------------------------------------------

struct Offer {
    prompt: String,
    id: String,
    before: usize,
    after: usize,
    kinds: Vec<&'static str>,
    lines_before: usize,
    lines_after: usize,
}

/// Finds pasted blocks (Claude Code wraps them in `<pasted_content id=…>`
/// lines; without markers, the whole prompt is considered only when it's
/// clearly a log) and compacts the ones worth it.
/// `cwd`: the session's project folder — paths inside it become relative in
/// the compacted log (stack traces repeat the absolute path on every frame).
fn build_offer(prompt: &str, cwd: Option<&str>, store: impl Fn(&str) -> String) -> Option<Offer> {
    let blocks = pasted_blocks(prompt);
    let candidates: Vec<(usize, usize)> = if blocks.is_empty() {
        let kind = logs::detect(prompt);
        if kind == logs::LogKind::Generic {
            return None;
        }
        vec![(0, prompt.len())]
    } else {
        blocks
    };

    let mut out = String::new();
    let mut cursor = 0;
    let (mut before, mut after, mut lines_before, mut lines_after) = (0, 0, 0, 0);
    let mut kinds = Vec::new();
    let id = format!("schliffe#{}", &short_hash(prompt)[..6]);
    for (start, end) in candidates {
        let block = &prompt[start..end];
        out.push_str(&strip_markers(&prompt[cursor..start]));
        cursor = end;
        let n_lines = block.lines().count();
        if n_lines < MIN_LINES || block.len() < MIN_BYTES {
            out.push_str(block);
            continue;
        }
        let kind = logs::detect(block);
        let compacted = relative_paths(&logs::compact(block, kind), cwd);
        if (compacted.len() as f64) > (block.len() as f64) * MAX_RATIO {
            out.push_str(block);
            continue;
        }
        let hash = store(block);
        let header = format!(
            "({} compacted by {id}: {} → {} lines; full original: `schliffe show {hash}`)\n",
            kind.label(),
            n_lines,
            compacted.lines().count()
        );
        before += block.len();
        after += header.len() + compacted.len();
        lines_before += n_lines;
        lines_after += compacted.lines().count();
        if !kinds.contains(&kind.label()) {
            kinds.push(kind.label());
        }
        out.push_str(&header);
        out.push_str(&compacted);
        out.push('\n');
    }
    out.push_str(&strip_markers(&prompt[cursor..]));
    if kinds.is_empty() {
        return None;
    }
    Some(Offer {
        prompt: out.trim().to_string(),
        id,
        before,
        after,
        kinds,
        lines_before,
        lines_after,
    })
}

/// Byte ranges of the content between `<pasted_content id="x">` and
/// `</pasted_content id="x">` lines.
fn pasted_blocks(prompt: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = prompt[search..].find("<pasted_content") {
        let open = search + rel;
        let Some(open_end) = prompt[open..].find('\n').map(|n| open + n + 1) else {
            break;
        };
        let Some(close_rel) = prompt[open_end..].find("</pasted_content") else {
            break;
        };
        let close = open_end + close_rel;
        out.push((open_end, close));
        search = prompt[close..]
            .find('\n')
            .map_or(prompt.len(), |n| close + n + 1);
    }
    out
}

fn relative_paths(text: &str, cwd: Option<&str>) -> String {
    let mut out = text.to_string();
    if let Some(cwd) = cwd.map(|c| c.trim_end_matches('/')).filter(|c| c.len() > 1) {
        out = out.replace(&format!("{cwd}/"), "");
    }
    if let Ok(home) = std::env::var("HOME")
        && home.len() > 1
    {
        out = out.replace(&format!("{home}/"), "~/");
    }
    out
}

fn strip_markers(text: &str) -> String {
    text.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("<pasted_content") && !t.starts_with("</pasted_content")
        })
        .map(|l| format!("{l}\n"))
        .collect()
}

fn short_hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Tries the platform's clipboard tools in order; true if one worked.
fn copy_to_clipboard(text: &str) -> bool {
    let candidates: &[&[&str]] = &[
        &["pbcopy"],
        &["wl-copy"],
        &["xclip", "-selection", "clipboard"],
        &["xsel", "--clipboard", "--input"],
        &["clip.exe"],
    ];
    for cmd in candidates {
        let Ok(mut child) = Command::new(cmd[0])
            .args(&cmd[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        let ok = child
            .stdin
            .take()
            .map(|mut s| s.write_all(text.as_bytes()).is_ok())
            .unwrap_or(false);
        if ok && child.wait().is_ok_and(|s| s.success()) {
            return true;
        }
    }
    false
}

/// Keeps both versions on disk; returns the folder.
fn save_prompts(original: &str, compact: &str) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".schliffe").join("prompts");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::write(dir.join("last-original.txt"), original).ok()?;
    std::fs::write(dir.join("last-compact.txt"), compact).ok()?;
    Some(dir.to_string_lossy().into_owned())
}

// ---- conversation size --------------------------------------------------

/// Context size of the latest model reply: input + cache read + cache write.
/// Reads only the tail of the transcript (it can be tens of MB).
fn context_tokens(transcript: &str) -> Option<u64> {
    let mut f = std::fs::File::open(transcript).ok()?;
    let len = f.metadata().ok()?.len();
    let tail = 4 * 1024 * 1024;
    f.seek(SeekFrom::Start(len.saturating_sub(tail))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines().rev().find_map(|line| {
        let v: Value = serde_json::from_str(line).ok()?;
        let u = v.pointer("/message/usage")?;
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let total =
            n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens");
        (total > 0).then_some(total)
    })
}

// ---- messages -------------------------------------------------------------

/// `Some(true)` = Portuguese, `Some(false)` = English, `None` = can't tell.
fn language(text: &str) -> Option<bool> {
    let t = format!(" {} ", text.to_lowercase());
    let hits = |words: &[&str]| words.iter().filter(|w| t.contains(*w)).count();
    let pt = hits(&[
        " que ", " não ", " para ", " com ", " uma ", " está ", "ção", " você ", " isso ", " por ",
    ]);
    let en = hits(&[
        " the ", " is ", " and ", " to ", " what ", " why ", " this ", " with ", " it ",
    ]);
    match (pt >= 2, en >= 2) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        (true, true) => Some(pt >= en),
        _ => None,
    }
}

fn system_prefers_portuguese() -> bool {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .is_some_and(|v| v.starts_with("pt"))
}

fn thousands(n: u64) -> String {
    format!("{}k", n / 1000)
}

fn block_message(o: &Offer, copied: bool, saved: Option<&str>, pt: bool) -> String {
    let kinds = o.kinds.join(", ");
    let pct = 100 - (o.after * 100 / o.before.max(1));
    let saved = saved.unwrap_or("~/.schliffe/prompts");
    match (pt, copied) {
        (true, true) => format!(
            "✂️ Schliffe: {kinds} detectado ({} → {} linhas, −{pct}%). A versão enxuta do seu prompt foi copiada — cole (Cmd+V) e envie. Os erros foram mantidos; o log completo fica recuperável. Para enviar o original, envie a mesma mensagem de novo (cópia em {saved}/last-original.txt).",
            o.lines_before, o.lines_after
        ),
        (true, false) => format!(
            "✂️ Schliffe: {kinds} detectado ({} → {} linhas, −{pct}%). Versão enxuta salva em {saved}/last-compact.txt — cole o conteúdo e envie. Para enviar o original, envie a mesma mensagem de novo.",
            o.lines_before, o.lines_after
        ),
        (false, true) => format!(
            "✂️ Schliffe: {kinds} detected ({} → {} lines, −{pct}%). A compact version of your prompt was copied — paste it and send. Errors are kept; the full log stays recoverable. To send the original, send the same message again (copy at {saved}/last-original.txt).",
            o.lines_before, o.lines_after
        ),
        (false, false) => format!(
            "✂️ Schliffe: {kinds} detected ({} → {} lines, −{pct}%). Compact version saved at {saved}/last-compact.txt — paste it and send. To send the original, send the same message again.",
            o.lines_before, o.lines_after
        ),
    }
}

fn tip_message(o: &Offer, pt: bool) -> String {
    if pt {
        format!(
            "💡 Schliffe: o {} colado tem {} linhas; só os erros e as primeiras linhas do seu código caberiam em {}. Da próxima vez, cole só esse trecho.",
            o.kinds.join(", "),
            o.lines_before,
            o.lines_after
        )
    } else {
        format!(
            "💡 Schliffe: the pasted {} has {} lines; the errors and your own frames fit in {}. Next time, paste just that part.",
            o.kinds.join(", "),
            o.lines_before,
            o.lines_after
        )
    }
}

fn context_message(tokens: u64, pt: bool) -> String {
    if pt {
        format!(
            "⚠️ Schliffe: esta conversa já tem ~{} tokens de contexto, e cada resposta relê tudo isso. Mudou de tarefa? Abra uma conversa nova. Terminou uma etapa? Use /compact.",
            thousands(tokens)
        )
    } else {
        format!(
            "⚠️ Schliffe: this conversation is at ~{} tokens of context, and every reply re-reads all of it. New task? Start a new conversation. Finished a step? Use /compact.",
            thousands(tokens)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(mode: LogMode) -> Env {
        Env {
            log_mode: mode,
            warn_at: vec![200_000, 400_000],
            effects: false,
        }
    }

    fn trace(lines: usize) -> String {
        let mut s = String::from("TypeError: x is undefined\n    at run (/app/src/run.ts:3:9)\n");
        for i in 0..lines {
            s.push_str(&format!(
                "    at step (/app/node_modules/lib/index.js:{i}:1)\n"
            ));
        }
        s
    }

    fn pasted(text: &str) -> String {
        format!(
            "o build quebrou, pode ver o que é isso? não sei o que mudou\n<pasted_content id=\"a1\">\n{text}</pasted_content id=\"a1\">\n"
        )
    }

    #[test]
    fn big_pasted_trace_is_held_back_then_allowed_on_resend() {
        // SAFETY: tests in this module don't read these vars concurrently.
        unsafe { std::env::set_var("SCHLIFFE_NO_STATS", "1") };
        let prompt = pasted(&trace(60));
        let input = json!({"prompt": prompt});
        let mut state = State::default();
        let r = handle(&prompt, &input, &mut state, &env(LogMode::Block)).unwrap();
        assert_eq!(r["decision"], "block");
        assert!(
            r["reason"]
                .as_str()
                .unwrap()
                .contains("Node.js/TypeScript stack trace")
        );
        assert!(r["reason"].as_str().unwrap().contains("linhas")); // Portuguese prompt
        // Same prompt again: goes through.
        assert!(handle(&prompt, &input, &mut state, &env(LogMode::Block)).is_none());
    }

    #[test]
    fn offer_keeps_user_text_and_errors_and_drops_markers() {
        let prompt = pasted(&trace(60));
        let offer = build_offer(&prompt, Some("/app/src/"), short_hash).unwrap();
        assert!(offer.prompt.starts_with("o build quebrou"));
        assert!(offer.prompt.contains("TypeError: x is undefined"));
        assert!(
            offer.prompt.contains("at run (run.ts:3:9)"),
            "{}",
            offer.prompt
        ); // relative to cwd
        assert!(offer.prompt.contains("schliffe show "));
        assert!(!offer.prompt.contains("pasted_content"));
        assert!(offer.after * 3 < offer.before);
    }

    #[test]
    fn small_pastes_and_plain_prompts_pass() {
        let input = json!({});
        let mut state = State::default();
        for p in [pasted(&trace(3)), "sim, pode fazer".to_string()] {
            assert!(handle(&p, &input, &mut state, &env(LogMode::Block)).is_none());
        }
    }

    #[test]
    fn short_prompts_reuse_the_last_known_language() {
        let mut state = State::default();
        assert_eq!(language("ok, continua"), None);
        let _ = handle(
            "por que isso não funciona com o build?",
            &json!({}),
            &mut state,
            &env(LogMode::Block),
        );
        assert_eq!(state.portuguese, Some(true));
    }

    #[test]
    fn tip_mode_never_blocks() {
        let prompt = pasted(&trace(60));
        let r = handle(
            &prompt,
            &json!({}),
            &mut State::default(),
            &env(LogMode::Tip),
        )
        .unwrap();
        assert!(r.get("decision").is_none());
        assert!(r["systemMessage"].as_str().unwrap().contains("Schliffe"));
    }

    #[test]
    fn context_warning_once_per_threshold() {
        let dir = std::env::temp_dir().join(format!("schliffe-ctx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = dir.join("t.jsonl");
        let write = |n: u64| {
            std::fs::write(
                &t,
                format!("{{\"message\":{{\"usage\":{{\"input_tokens\":1,\"cache_read_input_tokens\":{n},\"cache_creation_input_tokens\":0}}}}}}\n"),
            )
            .unwrap()
        };
        let input = json!({"transcript_path": t.to_str().unwrap()});
        let mut state = State::default();
        write(150_000);
        assert!(handle("ok", &input, &mut state, &env(LogMode::Block)).is_none());
        write(250_000);
        let r = handle("ok", &input, &mut state, &env(LogMode::Block)).unwrap();
        assert!(r["systemMessage"].as_str().unwrap().contains("250k"));
        assert!(handle("ok", &input, &mut state, &env(LogMode::Block)).is_none()); // already warned
        write(420_000);
        assert!(handle("ok", &input, &mut state, &env(LogMode::Block)).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
