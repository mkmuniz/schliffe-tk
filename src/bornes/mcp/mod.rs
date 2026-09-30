pub mod compress;
mod schema;

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// `bornes/mcp` (specs.md §6) — JSON-RPC protocol proxy over stdio. Unlike
/// `bornes/comandos`'s shim (a short-lived, one-shot process), this process
/// stays alive for the entire MCP session: it sits between the client
/// (Claude Code, this process's real stdin/stdout) and the real MCP server
/// (a child process spawned here).
///
/// The client thread and main server-reader thread each enforce a per-message
/// limit, then forward directly: no queue or total session byte budget.
/// Shared child ownership lets either direction terminate a malformed stream.
/// Shared state (`ProxyState`) tracks which method each pending request
/// `id` represents — a JSON-RPC response doesn't repeat the method, only
/// the `id` (specs §6.1: the only way to decide what to transform in a
/// `tools/list` response is knowing the matching request was a `tools/list`).
enum PendingKind {
    ToolsList,
    /// Carries the tool name: file-reading tools are never compressed.
    ToolsCall(String),
    /// Any other request — tracked only so it can get an error response if
    /// the server dies before answering.
    Other,
}

#[derive(Default)]
struct ProxyState {
    pending: Mutex<HashMap<String, PendingKind>>,
    schemas: Mutex<HashMap<String, Value>>,
}

/// `lazy_schemas = false` (`schliffe mcp --keep-schemas`) leaves `tools/list`
/// untouched and only compresses call results — for clients that already
/// defer tool schemas themselves (current Claude Code loads MCP schemas on
/// demand via its own tool search, which also relies on the full
/// descriptions this proxy would shorten).
pub fn run(server_cmd: &str, server_args: &[String], lazy_schemas: bool) -> ExitCode {
    let mut child = match Command::new(server_cmd)
        .args(server_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("schliffe: failed to start MCP server '{server_cmd}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let child_stdin = Arc::new(Mutex::new(
        child.stdin.take().expect("stdin piped on spawn"),
    ));
    let child_stdout = child.stdout.take().expect("stdout piped on spawn");
    let state = Arc::new(ProxyState::default());

    let child = Arc::new(Mutex::new(child));
    let protocol_error = Arc::new(AtomicBool::new(false));
    let client_child = child.clone();
    let client_error = protocol_error.clone();
    let client_state = state.clone();
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        loop {
            match read_frame(&mut reader, crate::core::secure::MAX_INPUT_BYTES) {
                Ok(Some(line)) if !line.trim().is_empty() => {
                    handle_client_message(&line, &client_state, &child_stdin, lazy_schemas);
                }
                Ok(Some(_)) => {}
                // Drop the child's stdin on client EOF. Do not time out a
                // legitimate tool that is still working on its response.
                Ok(None) => break,
                Err(e) => {
                    eprintln!("schliffe: MCP client protocol read failed: {e}");
                    client_error.store(true, Ordering::Relaxed);
                    // Wake the server reader even if the peer never replies.
                    let _ = client_child.lock().unwrap().kill();
                    break;
                }
            }
        }
    });
    let mut reader = BufReader::new(child_stdout);
    loop {
        match read_frame(&mut reader, crate::core::secure::MAX_INPUT_BYTES) {
            Ok(Some(line)) if !line.trim().is_empty() => handle_server_message(&line, &state),
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                eprintln!("schliffe: MCP server protocol read failed: {e}");
                protocol_error.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
    drop(reader);
    // Send errors BEFORE waiting: stdout EOF is not proof that the child
    // exited. A server can close stdout and stay alive indefinitely.
    let orphaned: Vec<String> = state
        .pending
        .lock()
        .unwrap()
        .drain()
        .map(|(id, _)| id)
        .collect();
    for id in orphaned {
        let id: Value = serde_json::from_str(&id).unwrap_or(Value::Null);
        write_value_to_client(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32000, "message": "MCP server disconnected or protocol limit exceeded (schliffe proxy)" },
        }));
    }
    if protocol_error.load(Ordering::Relaxed) {
        let _ = child.lock().unwrap().kill();
    }
    // A grace period applies only AFTER disconnection, never to an active
    // request. Reap a child that refuses to exit after closing its output.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let status = child.lock().unwrap().try_wait();
        match status {
            Ok(Some(status)) => {
                return if protocol_error.load(Ordering::Relaxed) {
                    ExitCode::FAILURE
                } else {
                    ExitCode::from(status.code().unwrap_or(1) as u8)
                };
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                thread::sleep(std::time::Duration::from_millis(1));
            }
            _ => {
                let mut child = child.lock().unwrap();
                let _ = child.kill();
                let _ = child.wait();
                return ExitCode::FAILURE;
            }
        }
    }
}

/// Limit each newline-delimited message, not the lifetime of the session.
/// Never parse or forward a truncated JSON message. The delimiter does not
/// count against the payload budget; a final unterminated frame is accepted.
fn read_frame(reader: &mut impl BufRead, limit: usize) -> io::Result<Option<String>> {
    let mut frame = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            break;
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let len = newline.unwrap_or(available.len());
        if len > limit.saturating_sub(frame.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP message exceeds 64 MiB limit",
            ));
        }
        frame.extend_from_slice(&available[..len]);
        reader.consume(len + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }
    String::from_utf8(frame)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

fn write_raw_line<W: Write>(dest: &Arc<Mutex<W>>, line: &str) {
    let mut w = dest.lock().unwrap();
    let _ = writeln!(w, "{line}");
    let _ = w.flush();
}

fn write_value_to_client(value: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

/// Client -> server. Intercepts `tools/call get_tool_schema` locally (it
/// never reaches the real server — it doesn't know about this synthetic
/// tool) and records pending `tools/list`/`tools/call` requests so it knows
/// how to handle the matching response.
fn handle_client_message(
    line: &str,
    state: &Arc<ProxyState>,
    child_stdin: &Arc<Mutex<std::process::ChildStdin>>,
    lazy_schemas: bool,
) {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        write_raw_line(child_stdin, line); // fail-open (rule 3): didn't parse, forward as-is
        return;
    };

    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id").cloned();

    if method == Some("tools/call") {
        let tool_name = msg.pointer("/params/name").and_then(Value::as_str);
        if lazy_schemas && tool_name == Some("get_tool_schema") {
            respond_get_tool_schema(&msg, id, state);
            return; // local short-circuit — specs §6.1, step 2
        }
    }

    if let Some(id) = &id {
        // Only requests (with a method) — a message with an id but no
        // method is the client answering a server-initiated request.
        let kind = match method {
            Some("tools/list") if lazy_schemas => Some(PendingKind::ToolsList),
            Some("tools/call") => Some(PendingKind::ToolsCall(
                msg.pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            )),
            Some(_) => Some(PendingKind::Other),
            None => None,
        };
        if let Some(kind) = kind {
            let mut pending = state.pending.lock().unwrap();
            let key = id_key(id);
            if pending.contains_key(&key) || pending.len() >= 1024 {
                drop(pending);
                write_value_to_client(&json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32000, "message": "Duplicate request ID or too many pending requests (schliffe proxy)" }
                }));
                return;
            }
            pending.insert(key, kind);
        }
    }

    write_raw_line(child_stdin, line);
}

fn respond_get_tool_schema(msg: &Value, id: Option<Value>, state: &Arc<ProxyState>) {
    let requested = msg
        .pointer("/params/arguments/tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let schemas = state.schemas.lock().unwrap();
    let found = schemas.get(requested).cloned();
    drop(schemas);

    let (text, is_error) = match found {
        Some(full) => (
            serde_json::to_string(&full).unwrap_or_else(|_| "{}".to_string()),
            false,
        ),
        None => (
            format!("tool '{requested}' not found — run tools/list first"),
            true,
        ),
    };

    write_value_to_client(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{"type": "text", "text": text}], "isError": is_error },
    }));
}

/// Server -> client. Only transforms responses (`result`/`error` present)
/// whose `id` matches a request we recorded as `tools/list`/`tools/call` —
/// anything else (a notification, a request from the server itself, a
/// response for a method with no special handling) passes straight through.
fn handle_server_message(line: &str, state: &Arc<ProxyState>) {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        println!("{line}");
        let _ = std::io::stdout().flush();
        return;
    };

    let id = msg.get("id").cloned();
    let is_response = msg.get("result").is_some() || msg.get("error").is_some();

    let kind = match (&id, is_response) {
        (Some(id), true) => state.pending.lock().unwrap().remove(&id_key(id)),
        _ => None,
    };

    let (out, key) = match kind {
        Some(PendingKind::ToolsList) => (
            schema::transform_tools_list(&msg, &state.schemas),
            "mcp tools/list".to_string(),
        ),
        Some(PendingKind::ToolsCall(tool)) => (
            compress::compress_tools_call_result(&msg, &tool, crate::core::store::put),
            format!("mcp {tool}"),
        ),
        Some(PendingKind::Other) | None => {
            write_value_to_client(&msg);
            return;
        }
    };
    let after = serde_json::to_string(&out)
        .map(|s| s.len())
        .unwrap_or(line.len());
    crate::core::stats::record(&key, line.len(), after);
    write_value_to_client(&out);
}

#[cfg(test)]
mod framing_tests {
    use super::*;

    #[test]
    fn frame_budget_resets_between_messages() {
        let mut reader = io::Cursor::new(b"1234\n5678\nlast");
        for expected in ["1234", "5678", "last"] {
            assert_eq!(
                read_frame(&mut reader, 4).unwrap().as_deref(),
                Some(expected)
            );
        }
        assert!(read_frame(&mut reader, 4).unwrap().is_none());
    }

    #[test]
    fn refuses_oversized_and_invalid_utf8_frames() {
        for bytes in [b"12345\n".as_slice(), b"12345", b"\xff\n"] {
            let mut reader = BufReader::with_capacity(2, bytes);
            assert_eq!(
                read_frame(&mut reader, 4).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn accepts_empty_crlf_and_split_unicode_frames() {
        let mut reader = BufReader::with_capacity(1, "\n€\r\n".as_bytes());
        assert_eq!(read_frame(&mut reader, 4).unwrap().as_deref(), Some(""));
        assert_eq!(read_frame(&mut reader, 4).unwrap().as_deref(), Some("€\r"));
        assert!(read_frame(&mut reader, 4).unwrap().is_none());
    }
}
