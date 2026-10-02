use serde_json::Value;

const MIN_DEDUP_BYTES: usize = 2 * 1024;

/// MCP tool call result compression (specs.md §6.2, reuses the techniques
/// from §5.5). Only the three safe, purely mechanical ones make it into v1 —
/// none of them "guesses" anything, they only remove an explicitly absent
/// value (`null`) or trim what's too large to keep in full:
///
///   - recursively drops `null` fields (not inference: the value was already
///     "absent", it was only costing framing tokens);
///   - truncates a long string, keeping a prefix + a count of what's left;
///   - caps a large array to the first N items + an omission marker.
///
/// Pruning by field's semantic relevance (pagination, HATEOAS) is out of
/// scope for v1 — it would require knowing the specific API, which would go
/// against business rule 5.
const MAX_STRING_CHARS: usize = 300;
const MAX_ARRAY_ITEMS: usize = 10;

/// Tools whose result is file content, not API data — never compressed.
/// Found testing against the real `@modelcontextprotocol/server-filesystem`
/// (2026-09-24): `read_text_file` on a `config.json` came back with its nulls
/// dropped, arrays capped and strings cut — an agent that edited and saved
/// it would corrupt the file. Matched by name, since MCP carries no "this is
/// a file" flag; extend with `SCHLIFFE_MCP_RAW_TOOLS=name1,name2`.
const RAW_TOOL_HINTS: &[&str] = &["read", "file", "cat", "open", "download", "blob"];

pub fn is_raw_tool(tool_name: &str) -> bool {
    let lower = tool_name.to_lowercase();
    if RAW_TOOL_HINTS.iter().any(|h| lower.contains(h)) {
        return true;
    }
    std::env::var("SCHLIFFE_MCP_RAW_TOOLS")
        .map(|v| v.split(',').any(|t| t.trim() == tool_name))
        .unwrap_or(false)
}

/// `store` saves a raw text and returns its hash — called only when content
/// was actually cut (truncation or array cap), so the original stays
/// recoverable via `schliffe show <hash>` (business rule 4).
pub fn compress_tools_call_result(
    msg: &Value,
    tool_name: &str,
    store: impl Fn(&str) -> String,
) -> Value {
    if let Some(deduplicated) = deduplicate_repeated_result(msg, tool_name, &store, |raw| {
        let session = std::env::var("CLAUDE_CODE_SESSION_ID").ok();
        matches!(
            crate::core::store::check_and_record_dedup(raw, 60, session.as_deref()),
            crate::core::store::Dedup::SeenRecently
        )
    }) {
        return deduplicated;
    }
    let Some(content) = msg.pointer("/result/content").and_then(Value::as_array) else {
        return msg.clone();
    };
    let Some(new_content) = compress_content_blocks(content, tool_name, store) else {
        return msg.clone();
    };

    let mut out = msg.clone();
    out["result"]["content"] = Value::Array(new_content);

    // Business rule 6 on the whole message, not just the text block.
    let orig_len = serde_json::to_string(msg)
        .map(|s| s.len())
        .unwrap_or(usize::MAX);
    let new_len = serde_json::to_string(&out)
        .map(|s| s.len())
        .unwrap_or(usize::MAX);
    if new_len < orig_len { out } else { msg.clone() }
}

/// Repeated large results are a common MCP pattern: refreshing a dashboard or
/// listing the same project tree can return the same payload several times in
/// one session. Keep the first copy in the private store and replace later
/// copies with a recoverable reference. Small results are excluded because a
/// reference would cost more than the content and because this must never
/// inflate output (business rule 6).
fn deduplicate_repeated_result(
    msg: &Value,
    tool_name: &str,
    store: &impl Fn(&str) -> String,
    seen_recently: impl Fn(&str) -> bool,
) -> Option<Value> {
    if is_raw_tool(tool_name)
        || msg.pointer("/result/isError").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let raw = serde_json::to_string(msg).ok()?;
    if raw.len() < MIN_DEDUP_BYTES || !seen_recently(&raw) {
        return None;
    }
    let hash = store(&raw);
    let mut out = msg.clone();
    out["result"]["content"] = serde_json::json!([{
        "type": "text",
        "text": format!("(same MCP result as before — full output: schliffe show {hash})"),
    }]);
    let compact_len = serde_json::to_string(&out).ok()?.len();
    (compact_len < raw.len()).then_some(out)
}

/// The content-block part of `compress_tools_call_result`, shared with the
/// Claude Code hook (`bornes/hook`), which receives MCP results as a bare
/// array of content blocks instead of a JSON-RPC message. `None` when
/// nothing changed (or the tool reads files and must stay untouched).
pub fn compress_content_blocks(
    content: &[Value],
    tool_name: &str,
    store: impl Fn(&str) -> String,
) -> Option<Vec<Value>> {
    if is_raw_tool(tool_name) {
        return None;
    }
    let mut new_content = Vec::with_capacity(content.len());
    let mut changed = false;
    let mut hints = Vec::new();
    for block in content {
        if let Some((compacted, lossy)) = try_compact_text_block(block) {
            if lossy && let Some(text) = block.get("text").and_then(Value::as_str) {
                hints.push(store(text));
            }
            new_content.push(compacted);
            changed = true;
        } else {
            new_content.push(block.clone());
        }
    }
    if !changed {
        return None;
    }
    for hash in hints {
        new_content.push(serde_json::json!({
            "type": "text",
            "text": format!("(schliffe trimmed this result — full output: schliffe show {hash})"),
        }));
    }
    Some(new_content)
}

/// Only compresses `{"type": "text", "text": "<json>"}` blocks whose text is
/// actually JSON — MCP tool results typically serialize the real payload as
/// a string inside the content block (specs §6.2). Free-form text (not
/// JSON) is left untouched — out of scope for this technique. Returns the
/// new block and whether anything was cut (not just re-serialized).
fn try_compact_text_block(block: &Value) -> Option<(Value, bool)> {
    if block.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    let text = block.get("text").and_then(Value::as_str)?;
    let parsed: Value = serde_json::from_str(text).ok()?;
    let mut lossy = false;
    let compact = compact_json(&parsed, &mut lossy);
    let compact_text = serde_json::to_string(&compact).ok()?;
    if compact_text.len() < text.len() {
        let mut b = block.clone();
        b["text"] = Value::String(compact_text);
        Some((b, lossy))
    } else {
        None
    }
}

fn compact_json(value: &Value, lossy: &mut bool) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if v.is_null() {
                    continue;
                }
                out.insert(k.clone(), compact_json(v, lossy));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            let mut out: Vec<Value> = items
                .iter()
                .take(MAX_ARRAY_ITEMS)
                .map(|v| compact_json(v, lossy))
                .collect();
            if items.len() > MAX_ARRAY_ITEMS {
                *lossy = true;
                out.push(serde_json::json!({
                    "_schliffe_omitted_items": items.len() - MAX_ARRAY_ITEMS
                }));
            }
            Value::Array(out)
        }
        Value::String(s) if s.chars().count() > MAX_STRING_CHARS => {
            *lossy = true;
            let truncated: String = s.chars().take(MAX_STRING_CHARS).collect();
            Value::String(format!(
                "{truncated}…(+{} chars omitted)",
                s.chars().count() - MAX_STRING_CHARS
            ))
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wrap(text_payload: &Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [{"type": "text", "text": text_payload.to_string()}],
                "isError": false
            }
        })
    }

    #[test]
    fn strips_null_fields() {
        let msg = wrap(&json!({"id": 1, "author": null, "title": "ok"}));
        let out = compress_tools_call_result(&msg, "search", |_| "h".into());
        let text = out["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert!(parsed.get("author").is_none());
        assert_eq!(parsed["title"], "ok");
    }

    #[test]
    fn caps_large_array_with_marker() {
        let items: Vec<Value> = (0..30).map(|i| json!({"id": i})).collect();
        let msg = wrap(&json!({"items": items}));
        let out = compress_tools_call_result(&msg, "search", |_| "h".into());
        let text = out["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        let arr = parsed["items"].as_array().unwrap();
        assert_eq!(arr.len(), MAX_ARRAY_ITEMS + 1); // 10 items + marker
        assert_eq!(arr[MAX_ARRAY_ITEMS]["_schliffe_omitted_items"], 20);
    }

    #[test]
    fn truncates_long_string() {
        let long = "x".repeat(500);
        let msg = wrap(&json!({"excerpt": long}));
        let out = compress_tools_call_result(&msg, "search", |_| "h".into());
        let text = out["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.len() < 500);
        assert!(text.contains("chars omitted"));
    }

    #[test]
    fn non_json_text_passthrough() {
        let msg = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"content": [{"type": "text", "text": "plain text, not json"}]}
        });
        let out = compress_tools_call_result(&msg, "search", |_| "h".into());
        assert_eq!(out, msg);
    }

    #[test]
    fn small_result_falls_back_to_original() {
        let msg = wrap(&json!({"ok": true}));
        let out = compress_tools_call_result(&msg, "search", |_| "h".into());
        assert_eq!(out, msg); // nothing to trim, rule 6 avoids "improving" it into something worse
    }

    #[test]
    fn file_reading_tools_are_never_touched() {
        let msg = wrap(&json!({"optional": null, "items": (0..30).collect::<Vec<_>>()}));
        for tool in ["read_text_file", "read_file", "get_file_contents", "cat"] {
            assert_eq!(compress_tools_call_result(&msg, tool, |_| "h".into()), msg);
        }
    }

    #[test]
    fn cut_content_gets_a_recovery_hint() {
        let items: Vec<Value> = (0..30).map(|i| json!({"id": i})).collect();
        let msg = wrap(&json!({"items": items}));
        let out = compress_tools_call_result(&msg, "search", |raw| {
            assert!(raw.contains("\"id\":29"));
            "abc123".into()
        });
        let hint = out["result"]["content"][1]["text"].as_str().unwrap();
        assert!(hint.contains("schliffe show abc123"));
    }

    #[test]
    fn null_stripping_alone_adds_no_hint_and_keeps_key_order() {
        let msg = wrap(&json!({"zeta": 1, "alpha": null, "beta": "x", "gamma": null}));
        let out = compress_tools_call_result(&msg, "search", |_| panic!("nothing was cut"));
        assert_eq!(out["result"]["content"].as_array().unwrap().len(), 1);
        let text = out["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, r#"{"zeta":1,"beta":"x"}"#);
    }

    #[test]
    fn repeated_large_result_becomes_a_recoverable_reference() {
        let items: Vec<Value> = (0..400).map(|_| json!("same payload")).collect();
        let msg = wrap(&json!({"items": items}));
        let out = deduplicate_repeated_result(
            &msg,
            "search",
            &|raw| {
                assert!(raw.len() > MIN_DEDUP_BYTES);
                "deadbeef01234567".into()
            },
            |_| true,
        )
        .expect("a repeated large result should be reduced");
        assert!(
            out["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("schliffe show deadbeef01234567")
        );
        assert!(
            serde_json::to_string(&out).unwrap().len() < serde_json::to_string(&msg).unwrap().len()
        );
    }

    #[test]
    fn repeated_file_result_is_never_replaced() {
        let items: Vec<Value> = (0..400).map(|_| json!("file content")).collect();
        let msg = wrap(&json!({"items": items}));
        assert!(
            deduplicate_repeated_result(&msg, "read_text_file", &|_| "hash".into(), |_| true)
                .is_none()
        );
    }
}
