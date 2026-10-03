//! End-to-end tests for the MCP Streamable HTTP proxy (`schliffe mcp --url`),
//! exercising the full lifecycle (M16): SSE event framing, server-initiated
//! messages on the standalone GET stream, the `MCP-Protocol-Version` header,
//! reconnect with `Last-Event-ID`, and clean shutdown on client EOF.
//!
//! The fixture is a minimal HTTP/1.1 server over `TcpListener` (no extra
//! dependencies). Each test configures a handler that inspects the request and
//! writes the response (plain JSON or an SSE stream) directly to the socket.
#![cfg(unix)]

use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_schliffe");

struct Request {
    method: String,
    headers: Vec<(String, String)>,
    body: String,
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Reads one HTTP/1.1 request (request line, headers, and a Content-Length
/// body if present). Byte-at-a-time: slow but trivially correct for tests.
fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) => {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
                if head.len() > 64 * 1024 {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.split("\r\n");
    let method = lines.next()?.split_whitespace().next()?.to_string();
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_lowercase();
            let v = v.trim().to_string();
            if k == "content-length" {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body).ok()?;
    }
    Some(Request {
        method,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Starts the fixture server; returns the MCP URL. The handler is called once
/// per connection with the parsed request and the raw stream to write to.
fn start_server<H>(handler: H) -> String
where
    H: Fn(Request, TcpStream) + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = Arc::new(handler);
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let handler = handler.clone();
            let mut stream = conn;
            thread::spawn(move || {
                if let Some(req) = read_request(&mut stream) {
                    handler(req, stream);
                }
            });
        }
    });
    format!("http://{addr}/mcp")
}

fn write_json(mut stream: TcpStream, body: &str) {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn write_sse_head(stream: &mut TcpStream) {
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    let _ = stream.flush();
}

fn write_sse_event(stream: &mut TcpStream, id: Option<&str>, data: &str) {
    let mut frame = String::new();
    if let Some(id) = id {
        frame.push_str(&format!("id: {id}\n"));
    }
    frame.push_str(&format!("data: {data}\n\n"));
    let _ = stream.write_all(frame.as_bytes());
    let _ = stream.flush();
}

fn write_405(mut stream: TcpStream) {
    let _ = stream.write_all(
        b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let _ = stream.flush();
}

fn initialize_result() -> String {
    json!({
        "jsonrpc": "2.0", "id": 1,
        "result": {"protocolVersion": "2025-06-18", "capabilities": {}}
    })
    .to_string()
}

/// A live `schliffe mcp --url` child, with its stdout collected line-by-line.
struct Proxy {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
    tmp: PathBuf,
}

impl Proxy {
    fn start(url: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let tmp = std::env::temp_dir().join(format!(
            "schliffe-mcp-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(tmp.join("store")).unwrap();
        let mut child = Command::new(BIN)
            .args(["mcp", "--url", url])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("HOME", &tmp)
            .env("SCHLIFFE_STORE_DIR", tmp.join("store"))
            .spawn()
            .unwrap();

        let stdout = child.stdout.take().unwrap();
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                sink.lock().unwrap().push(line);
            }
        });
        Proxy { child, lines, tmp }
    }

    fn send(&mut self, msg: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    fn close_stdin(&mut self) {
        drop(self.child.stdin.take());
    }

    /// Polls until `pred` holds over the collected stdout lines, or times out.
    fn wait_for<F: Fn(&[String]) -> bool>(&self, pred: F, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if pred(&self.lines.lock().unwrap()) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn wait_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.tmp);
    }
}

fn poll<T: Clone, F: Fn() -> Option<T>>(f: F, timeout: Duration) -> Option<T> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(v) = f() {
            return Some(v);
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

const TIMEOUT: Duration = Duration::from_secs(5);

/// A POST whose SSE response carries a server notification followed by the
/// JSON-RPC result: both reach the client, and the notification is not dropped.
#[test]
fn post_sse_response_forwards_notification_and_result() {
    let url = start_server(|req, stream| {
        if req.method == "GET" {
            write_405(stream); // no standalone stream in this test
            return;
        }
        let msg: Value = serde_json::from_str(&req.body).unwrap();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        if method == "initialize" {
            write_json(stream, &initialize_result());
            return;
        }
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let mut stream = stream;
        write_sse_head(&mut stream);
        write_sse_event(
            &mut stream,
            None,
            &json!({"jsonrpc":"2.0","method":"notifications/message","params":{"n":1}}).to_string(),
        );
        write_sse_event(
            &mut stream,
            None,
            &json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}}).to_string(),
        );
    });

    let mut proxy = Proxy::start(&url);
    proxy.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}).to_string());
    assert!(proxy.wait_for(|l| l.iter().any(|x| x.contains("2025-06-18")), TIMEOUT));
    proxy.send(&json!({"jsonrpc":"2.0","id":2,"method":"ping"}).to_string());
    assert!(proxy.wait_for(
        |l| l.iter().any(|x| x.contains("notifications/message")),
        TIMEOUT
    ));
    assert!(proxy.wait_for(|l| l.iter().any(|x| x.contains("\"ok\":true")), TIMEOUT));
}

/// After initialize, the proxy opens the standalone GET stream (carrying the
/// negotiated `MCP-Protocol-Version`) and forwards a server-initiated request.
#[test]
fn get_stream_delivers_server_request_with_protocol_header() {
    let seen_version: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured = seen_version.clone();
    let url = start_server(move |req, mut stream| {
        if req.method == "GET" {
            *captured.lock().unwrap() = header(&req, "mcp-protocol-version").map(String::from);
            write_sse_head(&mut stream);
            write_sse_event(
                &mut stream,
                None,
                &json!({"jsonrpc":"2.0","id":"s1","method":"roots/list"}).to_string(),
            );
            thread::sleep(Duration::from_millis(300));
            return;
        }
        write_json(stream, &initialize_result());
    });

    let mut proxy = Proxy::start(&url);
    proxy.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}).to_string());
    assert!(proxy.wait_for(|l| l.iter().any(|x| x.contains("roots/list")), TIMEOUT));
    let version = poll(|| seen_version.lock().unwrap().clone(), TIMEOUT);
    assert_eq!(version.as_deref(), Some("2025-06-18"));
}

/// When the GET stream drops, the proxy reconnects and resumes with the
/// `Last-Event-ID` of the last event it saw.
#[test]
fn get_stream_reconnects_with_last_event_id() {
    let get_count = Arc::new(Mutex::new(0u32));
    let resume_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let counter = get_count.clone();
    let resumed = resume_id.clone();
    let url = start_server(move |req, mut stream| {
        if req.method != "GET" {
            write_json(stream, &initialize_result());
            return;
        }
        let n = {
            let mut c = counter.lock().unwrap();
            *c += 1;
            *c
        };
        if n == 1 {
            write_sse_head(&mut stream);
            write_sse_event(
                &mut stream,
                Some("evt-1"),
                &json!({"jsonrpc":"2.0","method":"notifications/message"}).to_string(),
            );
            // drop stream -> close, forcing a reconnect
        } else {
            *resumed.lock().unwrap() = header(&req, "last-event-id").map(String::from);
            write_sse_head(&mut stream);
            thread::sleep(Duration::from_millis(500));
        }
    });

    let mut proxy = Proxy::start(&url);
    proxy.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}).to_string());
    let resumed = poll(|| resume_id.lock().unwrap().clone(), TIMEOUT);
    assert_eq!(resumed.as_deref(), Some("evt-1"));
}

/// Client EOF must shut the process down promptly even while a GET stream is
/// held open by the server — the listener thread is detached, never joined.
#[test]
fn client_eof_shuts_down_despite_open_get_stream() {
    let get_opened = Arc::new(Mutex::new(false));
    let opened = get_opened.clone();
    let url = start_server(move |req, mut stream| {
        if req.method == "GET" {
            *opened.lock().unwrap() = true;
            write_sse_head(&mut stream);
            thread::sleep(Duration::from_secs(60)); // hold open
            return;
        }
        write_json(stream, &initialize_result());
    });

    let mut proxy = Proxy::start(&url);
    proxy.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"}).to_string());
    assert!(
        poll(|| get_opened.lock().unwrap().then_some(()), TIMEOUT).is_some(),
        "GET stream never opened"
    );
    proxy.close_stdin();
    assert!(
        proxy.wait_exit(TIMEOUT),
        "proxy did not exit on client EOF while a GET stream was open"
    );
}
