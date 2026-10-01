pub mod compress;
mod oauth;
mod schema;

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompressionLevel {
    Low,
    #[default]
    Medium,
    High,
    Max,
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub lazy_schemas: bool,
    pub compression: CompressionLevel,
    pub include_tools: Vec<String>,
    pub exclude_tools: Vec<String>,
    pub oauth: bool,
}

/// Remote MCP entry point. The stdio proxy remains unchanged; this path uses
/// MCP Streamable HTTP POST requests and reuses its schema/result transforms.
/// Authentication accepts explicit headers and, when `--oauth` is enabled,
/// performs the MCP OAuth flow after a protected resource returns 401.
pub fn run_http_with_options(
    url: &str,
    headers: Vec<(String, String)>,
    options: Options,
) -> ExitCode {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        eprintln!("schliffe: MCP HTTP URL must use http:// or https://");
        return ExitCode::FAILURE;
    }
    let client = match reqwest::blocking::Client::builder().build() {
        Ok(client) => client,
        Err(e) => {
            eprintln!("schliffe: failed to create MCP HTTP client: {e}");
            return ExitCode::FAILURE;
        }
    };
    let state = Arc::new(ProxyState::default());
    let mut session_id = None::<String>;
    let mut oauth_token = None::<String>;
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();

    loop {
        let line = match read_frame(&mut reader, crate::core::secure::MAX_INPUT_BYTES) {
            Ok(Some(line)) if !line.trim().is_empty() => line,
            Ok(Some(_)) => continue,
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("schliffe: MCP HTTP request read failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            eprintln!("schliffe: MCP HTTP request was not valid JSON");
            return ExitCode::FAILURE;
        };
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned();

        if options.lazy_schemas
            && method == Some("tools/call")
            && msg.pointer("/params/name").and_then(Value::as_str) == Some("get_tool_schema")
        {
            respond_get_tool_schema(&msg, id, &state);
            continue;
        }

        if let Some(id) = &id {
            let kind = match method {
                Some("tools/list")
                    if options.lazy_schemas
                        || !options.include_tools.is_empty()
                        || !options.exclude_tools.is_empty() =>
                {
                    Some(PendingKind::ToolsList)
                }
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
                    write_value_to_client(&json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32000, "message": "Duplicate request ID or too many pending requests (schliffe proxy)"}
                    }));
                    continue;
                }
                pending.insert(key, kind);
            }
        }

        let mut response = match send_http_request(
            &client,
            url,
            &line,
            session_id.as_deref(),
            &headers,
            oauth_token.as_deref(),
        ) {
            Ok(response) => response,
            Err(e) => {
                eprintln!("schliffe: MCP HTTP request failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        if response.status() == reqwest::StatusCode::UNAUTHORIZED && options.oauth {
            let challenge = response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            match oauth::authorize(&client, url, challenge.as_deref()) {
                Ok(tokens) => {
                    oauth_token = Some(tokens.access_token);
                    response = match send_http_request(
                        &client,
                        url,
                        &line,
                        session_id.as_deref(),
                        &headers,
                        oauth_token.as_deref(),
                    ) {
                        Ok(response) => response,
                        Err(e) => {
                            eprintln!("schliffe: authenticated MCP request failed: {e}");
                            return ExitCode::FAILURE;
                        }
                    };
                }
                Err(e) => {
                    eprintln!("schliffe: MCP OAuth authorization failed: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        if let Some(id) = response.headers().get("Mcp-Session-Id")
            && let Ok(id) = id.to_str()
        {
            session_id = Some(id.to_string());
        }
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = match read_http_body(response) {
            Ok(body) => body,
            Err(e) => {
                eprintln!("schliffe: failed to read MCP HTTP response: {e}");
                return ExitCode::FAILURE;
            }
        };
        if status.as_u16() == 202 || body.trim().is_empty() {
            continue;
        }
        let messages = if content_type.starts_with("text/event-stream") {
            body.lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        } else {
            vec![body]
        };
        for message in messages {
            handle_server_message(&message, &state, &options);
        }
    }
}

fn send_http_request(
    client: &reqwest::blocking::Client,
    url: &str,
    body: &str,
    session_id: Option<&str>,
    headers: &[(String, String)],
    oauth_token: Option<&str>,
) -> Result<reqwest::blocking::Response, reqwest::Error> {
    let mut request = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(body.to_owned());
    if let Some(id) = session_id {
        request = request.header("Mcp-Session-Id", id);
    }
    for (name, value) in headers {
        request = request.header(name, expand_header_value(value));
    }
    if let Some(token) = oauth_token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    request.send()
}

fn read_http_body(response: reqwest::blocking::Response) -> io::Result<String> {
    let mut bytes = Vec::new();
    response
        .take(crate::core::secure::MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > crate::core::secure::MAX_INPUT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MCP HTTP response exceeds 64 MiB limit",
        ));
    }
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn expand_header_value(value: &str) -> String {
    let Some(name) = value.strip_prefix("${").and_then(|v| v.strip_suffix('}')) else {
        return value.to_string();
    };
    std::env::var(name).unwrap_or_default()
}

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
pub fn run_with_options(server_cmd: &str, server_args: &[String], options: Options) -> ExitCode {
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
    let client_options = options.clone();
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        loop {
            match read_frame(&mut reader, crate::core::secure::MAX_INPUT_BYTES) {
                Ok(Some(line)) if !line.trim().is_empty() => {
                    handle_client_message(&line, &client_state, &child_stdin, &client_options);
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
            Ok(Some(line)) if !line.trim().is_empty() => {
                handle_server_message(&line, &state, &options)
            }
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
    options: &Options,
) {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        write_raw_line(child_stdin, line); // fail-open (rule 3): didn't parse, forward as-is
        return;
    };

    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id").cloned();

    if method == Some("tools/call") {
        let tool_name = msg.pointer("/params/name").and_then(Value::as_str);
        if options.lazy_schemas && tool_name == Some("get_tool_schema") {
            respond_get_tool_schema(&msg, id, state);
            return; // local short-circuit — specs §6.1, step 2
        }
    }

    if let Some(id) = &id {
        // Only requests (with a method) — a message with an id but no
        // method is the client answering a server-initiated request.
        let kind = match method {
            Some("tools/list")
                if options.lazy_schemas
                    || !options.include_tools.is_empty()
                    || !options.exclude_tools.is_empty() =>
            {
                Some(PendingKind::ToolsList)
            }
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
fn handle_server_message(line: &str, state: &Arc<ProxyState>, options: &Options) {
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
            schema::transform_tools_list_with_options(&msg, &state.schemas, options),
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
