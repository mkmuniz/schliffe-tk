pub mod image;

use crate::bornes::mcp::compress;
use crate::core::secure::{read_limited, write_private};
use crate::core::{stats, store};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::ExitCode;

/// `bornes/hook` — the fourth interception point, for what the other three
/// can't reach (2026-09-25): remote MCP servers (HTTP, including OAuth-managed
/// endpoints such as Figma —
/// the stdio proxy can't sit in front of them) and images read by Claude
/// Code's own Read tool (never touch a shell or an MCP pipe).
///
/// It's a Claude Code `PostToolUse` hook: Claude Code runs
/// `schliffe hook post-tool-use` after each matching tool call, passing the
/// call as JSON on stdin; answering with `hookSpecificOutput.
/// updatedToolOutput` replaces what the model sees. Platform caveat, and the
/// reason the project avoided hooks at first: hook output mutation was
/// verified broken on native Windows (specs, "Why this project exists") —
/// supported on macOS, Linux and WSL only.
///
/// Fail-open all the way down (business rule 3): unparseable input, an
/// unknown tool, an unknown result shape or any error ⇒ print nothing and
/// exit 0, which leaves the original output untouched. For built-in tools
/// Claude Code also discards a replacement that doesn't match the tool's
/// output schema, so a wrong guess about the Read shape can't break a read.
pub fn run_post_tool_use() -> ExitCode {
    // Bounded: a hostile MCP server could otherwise hand us an unbounded
    // result. Over the cap the tool output is left untouched.
    let Some(raw) = read_limited(std::io::stdin()) else {
        return ExitCode::SUCCESS;
    };
    // Diagnostics: keep a copy of what Claude Code sent, to learn the exact
    // result shapes of tools that aren't documented (e.g. Read on images).
    if let Ok(dir) = std::env::var("SCHLIFFE_HOOK_DUMP") {
        let name = format!("{}-{}.json", now_nanos(), std::process::id());
        // Diagnostics contain whole tool results — owner-only.
        let _ = write_private(&PathBuf::from(dir).join(name), raw.as_bytes());
    }
    let Ok(input) = serde_json::from_str::<Value>(&raw) else {
        return ExitCode::SUCCESS;
    };
    if let Some(updated) = transform(&input, image::max_edge(), store::put) {
        println!(
            "{}",
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "updatedToolOutput": updated,
                }
            })
        );
    }
    ExitCode::SUCCESS
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// The replacement output, or `None` to leave the original alone.
fn transform(input: &Value, max_edge: u32, store: impl Fn(&str) -> String) -> Option<Value> {
    let tool = input.get("tool_name")?.as_str()?;
    let response = input.get("tool_response")?;
    let is_mcp = tool.starts_with("mcp__");
    if !is_mcp && tool != "Read" {
        return None;
    }

    let mut out = response.clone();
    let mut changed = false;

    // MCP text results: the same mechanical JSON compaction the stdio proxy
    // applies (nulls, long strings, big arrays — never file-reading tools).
    if is_mcp {
        let short_name = tool.rsplit("__").next().unwrap_or(tool);
        let key = format!(
            "mcp {}",
            tool.trim_start_matches("mcp__").replace("__", " ")
        );
        let blocks = match &out {
            Value::Array(b) => Some(b.clone()),
            Value::Object(o) => o.get("content").and_then(Value::as_array).cloned(),
            _ => None,
        };
        if let Some(blocks) = blocks {
            let before = serde_json::to_string(&blocks).map(|s| s.len()).unwrap_or(0);
            if let Some(new_blocks) = compress::compress_content_blocks(&blocks, short_name, &store)
            {
                let after = serde_json::to_string(&new_blocks)
                    .map(|s| s.len())
                    .unwrap_or(0);
                if after < before {
                    stats::record(&key, before, after);
                    match &mut out {
                        Value::Array(b) => *b = new_blocks,
                        Value::Object(o) => {
                            o.insert("content".into(), Value::Array(new_blocks));
                        }
                        _ => {}
                    }
                    changed = true;
                }
            }
        }
    }

    // Images (MCP image blocks, e.g. Figma's get_screenshot; Read on a PNG/JPEG).
    let mut shrunk = Vec::new();
    image::shrink_images(&mut out, max_edge, &mut shrunk);
    for s in &shrunk {
        // Stats count bytes and estimate tokens as bytes/4 — record image
        // tokens scaled ×4 so the report's token column stays right.
        let key = if is_mcp {
            "image (mcp)"
        } else {
            "image (Read)"
        };
        stats::record(
            key,
            (s.tokens_before * 4) as usize,
            (s.tokens_after * 4) as usize,
        );
        changed = true;
    }

    changed.then_some(out)
}

// ---- `schliffe hook install|uninstall` -------------------------------------

const HOOK_ARGS: &str = "hook post-tool-use";
const MATCHER: &str = "Read|mcp__.*";
const PROMPT_HOOK_ARGS: &str = "hook user-prompt-submit";

/// Every Claude Code event Schliffe registers: (event, matcher, args, timeout s).
const ENTRIES: &[(&str, Option<&str>, &str, u64)] = &[
    ("PostToolUse", Some(MATCHER), HOOK_ARGS, 30),
    // bornes/prompt: pasted logs + long-conversation warnings.
    ("UserPromptSubmit", None, PROMPT_HOOK_ARGS, 10),
];

fn settings_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SCHLIFFE_CLAUDE_SETTINGS") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude").join("settings.json"))
}

fn schliffe_bin() -> String {
    // Prefer the installed copy (stable across rebuilds / `cargo clean`).
    if let Some(home) = std::env::var_os("HOME") {
        let installed = PathBuf::from(home)
            .join(".schliffe")
            .join("bin")
            .join("schliffe");
        if installed.exists() {
            return installed.to_string_lossy().into_owned();
        }
    }
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "schliffe".into())
}

fn is_ours(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        // "elagix": the project's former name — so reinstalling replaces
        // a hook registered before the rename instead of adding a second one.
        .is_some_and(|c| {
            (c.contains("schliffe") || c.contains("elagix"))
                && (c.contains(HOOK_ARGS) || c.contains(PROMPT_HOOK_ARGS))
        })
}

/// Adds (or refreshes) Schliffe's PostToolUse entry in Claude Code's user
/// settings, keeping every other setting and hook as it was. Idempotent.
pub fn install() -> ExitCode {
    if cfg!(windows) {
        eprintln!(
            "schliffe: the Claude Code hook isn't supported on native Windows (hook output\n\
             replacement doesn't work there). Use it from WSL instead."
        );
        return ExitCode::FAILURE;
    }
    let Some(path) = settings_path() else {
        eprintln!("schliffe: couldn't locate Claude Code's settings (no HOME)");
        return ExitCode::FAILURE;
    };
    let mut settings = match read_settings(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("schliffe: {e}");
            return ExitCode::FAILURE;
        }
    };
    remove_ours(&mut settings);
    let Some(root) = settings.as_object_mut() else {
        eprintln!("schliffe: {} isn't a JSON object", path.display());
        return ExitCode::FAILURE;
    };
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        eprintln!("schliffe: \"hooks\" in {} isn't an object", path.display());
        return ExitCode::FAILURE;
    };
    for (event, matcher, args, timeout) in ENTRIES {
        let mut entry = json!({
            "hooks": [{
                "type": "command",
                "command": format!("{} {args}", schliffe_bin()),
                "timeout": timeout,
            }],
        });
        if let Some(m) = matcher {
            entry["matcher"] = json!(m);
        }
        let list = hooks.entry(*event).or_insert_with(|| json!([]));
        let Some(list) = list.as_array_mut() else {
            eprintln!(
                "schliffe: \"hooks.{event}\" in {} isn't a list",
                path.display()
            );
            return ExitCode::FAILURE;
        };
        list.push(entry);
    }
    match write_settings(&path, &settings) {
        Ok(backup) => {
            println!("schliffe: hook installed in {}", path.display());
            if let Some(b) = backup {
                println!("schliffe: previous settings backed up to {}", b.display());
            }
            println!(
                "schliffe: open a NEW Claude Code session (or reload the VS Code window) to\n\
                 activate it. Remote MCP results (e.g. Figma), large images, pasted logs\n\
                 and long-conversation warnings are handled; check with `schliffe stats`."
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("schliffe: failed to write {}: {e}", path.display());
            ExitCode::FAILURE
        }
    }
}

/// Removes Schliffe's entry, leaving everything else in place.
pub fn uninstall() -> ExitCode {
    let Some(path) = settings_path() else {
        return ExitCode::FAILURE;
    };
    let mut settings = match read_settings(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("schliffe: {e}");
            return ExitCode::FAILURE;
        }
    };
    if !remove_ours(&mut settings) {
        println!(
            "schliffe: hook not installed in {} (nothing to do)",
            path.display()
        );
        return ExitCode::SUCCESS;
    }
    match write_settings(&path, &settings) {
        Ok(_) => {
            println!("schliffe: hook removed from {}", path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("schliffe: failed to write {}: {e}", path.display());
            ExitCode::FAILURE
        }
    }
}

fn read_settings(path: &PathBuf) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) if raw.trim().is_empty() => Ok(json!({})),
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| {
            format!(
                "{} isn't valid JSON ({e}) — not touching it",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(format!("couldn't read {}: {e}", path.display())),
    }
}

/// Writes atomically (temp file + rename), backing up the previous file.
fn write_settings(path: &PathBuf, settings: &Value) -> std::io::Result<Option<PathBuf>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let backup = if path.exists() {
        let b = path.with_extension("json.schliffe-backup");
        std::fs::copy(path, &b)?;
        Some(b)
    } else {
        None
    };
    let tmp = path.with_extension("json.schliffe-tmp");
    let text = serde_json::to_string_pretty(settings).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, format!("{text}\n"))?;
    std::fs::rename(&tmp, path)?;
    Ok(backup)
}

/// Drops Schliffe's hooks from every event it registers (and groups left
/// empty). Returns whether anything was removed.
fn remove_ours(settings: &mut Value) -> bool {
    let mut removed = false;
    for (event, ..) in ENTRIES {
        let Some(list) = settings
            .pointer_mut(&format!("/hooks/{event}"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        let before = list.len();
        for group in list.iter_mut() {
            if let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let n = hooks.len();
                hooks.retain(|h| !is_ours(h));
                removed |= hooks.len() != n;
            }
        }
        list.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|h| !h.is_empty())
        });
        removed |= list.len() != before;
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats_off() {
        // Tests must not write to the real stats log.
        // SAFETY: set once, before any thread reads it; same value everywhere.
        unsafe { std::env::set_var("SCHLIFFE_NO_STATS", "1") };
    }

    #[test]
    fn mcp_json_text_is_compressed() {
        stats_off();
        let items: Vec<Value> = (0..200)
            .map(|i| json!({"id": i, "name": format!("node {i}"), "gone": null}))
            .collect();
        let payload = json!({"items": items});
        let input = json!({
            "tool_name": "mcp__figma__get_metadata",
            "tool_response": [{"type": "text", "text": payload.to_string()}],
        });
        let out = transform(&input, 1280, |_| "h1".into()).unwrap();
        let text = out[0]["text"].as_str().unwrap();
        assert!(!text.contains("gone"));
        assert!(
            out[1]["text"]
                .as_str()
                .unwrap()
                .contains("schliffe show h1")
        );
    }

    #[test]
    fn mcp_image_blocks_are_shrunk() {
        stats_off();
        let input = json!({
            "tool_name": "mcp__figma__get_screenshot",
            "tool_response": {"content": [{"type": "image", "data": image::tests::png_b64(900, 600), "mimeType": "image/png"}]},
        });
        let out = transform(&input, 300, |_| "h".into()).unwrap();
        assert!(
            out["content"][0]["data"].as_str().unwrap().len()
                < input["tool_response"]["content"][0]["data"]
                    .as_str()
                    .unwrap()
                    .len()
        );
    }

    #[test]
    fn other_tools_and_plain_results_are_left_alone() {
        stats_off();
        let bash = json!({"tool_name": "Bash", "tool_response": {"stdout": "x"}});
        assert!(transform(&bash, 1280, |_| "h".into()).is_none());
        let read_text = json!({"tool_name": "Read", "tool_response": {"type": "text", "file": {"content": "hello"}}});
        assert!(transform(&read_text, 1280, |_| "h".into()).is_none());
        let file_tool = json!({
            "tool_name": "mcp__fs__read_text_file",
            "tool_response": [{"type": "text", "text": "{\"a\":null}"}],
        });
        assert!(transform(&file_tool, 1280, |_| "h".into()).is_none());
    }

    #[test]
    fn install_is_idempotent_and_keeps_other_settings() {
        let mut s = json!({"model": "opus", "hooks": {"PostToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "my-linter"}]}
        ]}});
        assert!(!remove_ours(&mut s));
        let ours = json!({"matcher": MATCHER, "hooks": [{"type": "command", "command": "/x/schliffe hook post-tool-use"}]});
        s["hooks"]["PostToolUse"].as_array_mut().unwrap().push(ours);
        assert!(remove_ours(&mut s));
        assert_eq!(s["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(s["model"], "opus");
        assert_eq!(
            s["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "my-linter"
        );
    }

    #[test]
    fn hook_registered_under_the_old_name_is_recognized() {
        let mut s = json!({"hooks": {"PostToolUse": [
            {"matcher": MATCHER, "hooks": [{"type": "command", "command": "/u/.elagix/bin/elagix hook post-tool-use"}]}
        ]}});
        assert!(remove_ours(&mut s));
        assert!(s["hooks"]["PostToolUse"].as_array().unwrap().is_empty());
    }
}
