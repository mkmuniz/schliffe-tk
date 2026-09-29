pub mod compress;
mod schema;

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

/// `bornes/mcp` (specs.md §6) — JSON-RPC protocol proxy over stdio. Unlike
/// `bornes/comandos`'s shim (a short-lived, one-shot process), this process
/// stays alive for the entire MCP session: it sits between the client
/// (Claude Code, this process's real stdin/stdout) and the real MCP server
/// (a child process spawned here).
///
/// Two threads: one reads the client's stdin and forwards it (intercepting
/// along the way) to the server's stdin; the main one reads the server's
/// stdout and forwards it (intercepting along the way) to the real stdout.
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

    let state_up = state.clone();
    let child_stdin_up = child_stdin.clone();
    let upstream = thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            handle_client_message(&line, &state_up, &child_stdin_up, lazy_schemas);
        }
        // `child_stdin_up` gets dropped here, at the end of the closure.
    });

    // Real bug found testing live (2026-07-26) with the fake MCP server:
    // `child_stdin` (this scope) and `child_stdin_up` (the thread above) are
    // two copies of the same `Arc` — the child process's stdin pipe only
    // truly closes once the LAST copy is dropped. Without this explicit
    // `drop`, this copy would survive until `run()` returns, which would
    // only happen AFTER `child.wait()` — but the real server (reading stdin
    // until EOF) never gets that EOF while the pipe stays open, so it never
    // exits on its own, and `child.wait()` hangs forever. Needs to be
    // dropped here, before the read loop below — from this point on, only
    // the thread's copy matters, and it drops when the CLIENT's stdin closes.
    drop(child_stdin);

    // Bounded (2026-09-28 hardening): the server on the other end is a
    // third-party process. `lines()` would grow a single line without
    // limit, so a hostile or broken server could exhaust memory just by
    // never sending a newline. Past the cap the proxy stops forwarding —
    // pending requests then get the "server exited" error below.
    let reader = BufReader::new(child_stdout.take(crate::core::secure::MAX_INPUT_BYTES as u64));
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        handle_server_message(&line, &state);
    }

    // The server's stdout closed: either a normal shutdown (the client
    // closed stdin first) or the server died. Any request still pending
    // would otherwise wait forever — answer each with a JSON-RPC error
    // (validated 2026-09-24 with a server that exits mid-call).
    let status = child.wait();
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
            "error": { "code": -32000, "message": "MCP server exited before responding (schliffe proxy)" },
        }));
    }

    // Only wait for the client->server thread on a normal shutdown: if the
    // server died while the client is still connected, that thread is
    // blocked reading the client's stdin and returning from here ends it.
    if upstream.is_finished() {
        let _ = upstream.join();
    }
    match status {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(_) => ExitCode::FAILURE,
    }
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
            state.pending.lock().unwrap().insert(id_key(id), kind);
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
