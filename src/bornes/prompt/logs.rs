/// Recognizes what kind of log a pasted block is and compacts it **without
/// changing what it says**. The rule is to remove only what is provably
/// noise, and keep anything in doubt:
///
/// - never edited: every kept line is byte-for-byte the original (apart from
///   ANSI color codes); no line is ever shortened;
/// - always kept: exception/error messages, "Caused by"/chained sections,
///   every frame in the user's own code, the frame where it was thrown, and
///   the library frame the user's code called (it names the failing API);
/// - removed: library/framework/runtime frames in between, and — in app
///   logs — routine lines far from any problem; each removed stretch
///   becomes a counted marker at the exact place it was, so order and
///   position stay visible.
///
/// When a log can't be recognized with confidence it is left as it is.
/// Pure text processing (no model), deterministic.
use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LogKind {
    JsStack,
    PythonTraceback,
    JavaStack,
    DotnetStack,
    GoPanic,
    RustPanic,
    TimestampedLog,
    Generic,
}

impl LogKind {
    pub fn label(self) -> &'static str {
        match self {
            LogKind::JsStack => "Node.js/TypeScript stack trace",
            LogKind::PythonTraceback => "Python traceback",
            LogKind::JavaStack => "Java/Kotlin stack trace",
            LogKind::DotnetStack => ".NET stack trace",
            LogKind::GoPanic => "Go panic",
            LogKind::RustPanic => "Rust panic",
            LogKind::TimestampedLog => "application log",
            LogKind::Generic => "log",
        }
    }
}

/// Guard against pathological recursion only (hundreds of identical app
/// frames); a normal stack keeps all of its app frames.
const MAX_APP_FRAMES: usize = 30;
/// Lines kept on each side of a problem line in an app log.
const CONTEXT: usize = 2;

pub fn detect(text: &str) -> LogKind {
    let lines: Vec<&str> = text.lines().collect();
    let count = |pred: &dyn Fn(&str) -> bool| lines.iter().filter(|l| pred(l)).count();
    // Checked first: an app log often has a stack trace inside it. It must
    // look like a log (timestamps AND log levels or HTTP request lines) —
    // a CSV of timestamped data rows is not a log and must not be thinned.
    if lines.len() >= 5
        && count(&|l| timestamp().is_match(l)) * 2 >= lines.len()
        && count(&|l| log_level().is_match(l) || http_request().is_match(l)) * 10 >= lines.len() * 3
    {
        return LogKind::TimestampedLog;
    }
    if text.contains("Traceback (most recent call last):") {
        return LogKind::PythonTraceback;
    }
    if text.contains("goroutine ") && (text.contains("panic:") || text.contains("fatal error:")) {
        return LogKind::GoPanic;
    }
    if text.contains("panicked at") && text.contains("stack backtrace:") {
        return LogKind::RustPanic;
    }
    if count(&|l| java_frame().is_match(l)) >= 2 {
        return LogKind::JavaStack;
    }
    if count(&|l| dotnet_frame().is_match(l) && !java_frame().is_match(l)) >= 2 {
        return LogKind::DotnetStack;
    }
    if count(&|l| js_frame().is_match(l)) >= 2 {
        return LogKind::JsStack;
    }
    LogKind::Generic
}

/// Compacts `text` as the given kind. Never returns something larger than
/// the input (falls back to the input).
pub fn compact(text: &str, kind: LogKind) -> String {
    let cleaned = generic_pass(text);
    let out = match kind {
        LogKind::JsStack => stack(&cleaned, js_frame(), is_js_library),
        LogKind::JavaStack => stack(&cleaned, java_frame(), is_java_library),
        LogKind::DotnetStack => stack(&cleaned, dotnet_frame(), is_dotnet_library),
        LogKind::PythonTraceback => python(&cleaned),
        LogKind::GoPanic => go_panic(&cleaned),
        LogKind::RustPanic => rust_panic(&cleaned),
        LogKind::TimestampedLog => embedded_stacks(&timestamped(&cleaned)),
        LogKind::Generic => cleaned.clone(),
    };
    // Nothing substantive removed (only trailing whitespace): hand back the
    // original, byte for byte.
    if out.len() < text.len() && out.trim_end() != text.trim_end() {
        out
    } else {
        text.to_string()
    }
}

// ---- shared -----------------------------------------------------------

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex is valid"))
}

fn js_frame() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^\s+at .*(\(.+:\d+:\d+\)|\S+:\d+:\d+)\s*$")
}
fn java_frame() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // `at com.acme.Svc.run(Svc.java:88)`, `(Native Method)`, `(Unknown Source)`
    re(
        &R,
        r"^\s+at [\w$.<>/]+\((\w[\w$]*\.(java|kt|scala|groovy):\d+|Native Method|Unknown Source)\)\s*$",
    )
}
fn dotnet_frame() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // `at Ns.Class.Method(Int32 id) in /src/File.cs:line 57` — the name has
    // no spaces and touches the parenthesis (unlike JS's `at fn (file:1:2)`).
    re(&R, r"^\s+at [\w.`<>\[\]+,]+\([^)]*\)( in .+:line \d+)?\s*$")
}
fn rust_std_frame() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // `std::`, or `std[5d97c59e5e5fafcc]::` / `<<std[..]` in recent Rust.
    re(
        &R,
        r"- <*(?:&(?:mut )?dyn |dyn |fn\(\) as |impl )?(std|core|alloc|tokio|futures\w*|panic_unwind|backtrace\w*|__rust\w*|rust_begin_unwind|__rustc|_main|start)\b",
    )
}
fn timestamp() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"^\s*\[?(\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}([.,]\d+)?(Z|[+-]\d{2}:?\d{2})?|\d{2}:\d{2}:\d{2}([.,]\d+)?)\]?",
    )
}
fn log_level() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"(?i)(\b|\[)(trace|debug|info|notice|warn|warning|error|err|fatal|crit|critical)(\b|\])",
    )
}
fn http_request() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r#"\b(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+/\S*"?\s+\S*\s*\d{3}\b"#,
    )
}
/// A line that signals a problem. Deliberately broad: a false positive only
/// keeps an extra line; a false negative would hide one.
fn problem_line() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r#"(?i)\b(error|errors|err|fatal|critical|crit|exception|panic|failed|failure|fail|warn|warning|traceback|caused by|timeout|timed out|refused|denied|unauthorized|forbidden|killed|oom|out of memory|segfault|segmentation|abort|aborted|unavailable|deadlock|overflow|unhandled|rejected|invalid|cannot|can't|could not|not found)\b|\b(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+/\S*"?\s+\S*\s*[45]\d\d\b|exit (code|status) [1-9]"#,
    )
}
fn ansi() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, "\x1b\\[[0-9;]*[a-zA-Z]")
}

/// ANSI codes out, consecutive identical lines collapsed with their count
/// (`(×N)`), runs of blank lines reduced to one. Nothing else changes; no
/// line is shortened.
fn generic_pass(text: &str) -> String {
    let stripped = ansi().replace_all(text, "");
    let mut out: Vec<String> = Vec::new();
    let mut repeats = 0;
    for line in stripped.lines() {
        let line = line.trim_end();
        if !line.is_empty() && out.last().is_some_and(|l| l == line) {
            repeats += 1;
            continue;
        }
        if repeats > 0
            && let Some(l) = out.last_mut()
        {
            l.push_str(&format!("  (×{})", repeats + 1));
        }
        repeats = 0;
        if line.is_empty() && out.last().is_some_and(|l| l.is_empty()) {
            continue;
        }
        out.push(line.to_string());
    }
    if repeats > 0
        && let Some(l) = out.last_mut()
    {
        l.push_str(&format!("  (×{})", repeats + 1));
    }
    out.join("\n").trim().to_string()
}

/// Emits `items` in order, keeping those marked in `keep` and replacing
/// each stretch of dropped ones with one marker in its place.
fn emit_with_markers(
    items: &[Vec<&str>],
    keep: &[bool],
    indent: &str,
    what: &str,
    out: &mut Vec<String>,
) {
    let mut hidden_items = 0;
    let mut hidden_lines = 0;
    let flush = |out: &mut Vec<String>, items: usize, lines: usize| {
        if items > 0 {
            out.push(format!("{indent}[+{lines} lines omitted: {items} {what}]"));
        }
    };
    for (item, &k) in items.iter().zip(keep) {
        if k {
            flush(out, hidden_items, hidden_lines);
            hidden_items = 0;
            hidden_lines = 0;
            out.extend(item.iter().map(|l| l.to_string()));
        } else {
            hidden_items += 1;
            hidden_lines += item.len();
        }
    }
    flush(out, hidden_items, hidden_lines);
}

/// Which frames of a run to keep, given which are library frames.
/// `innermost_first`: JS/Java/.NET/Rust/Go list the throwing frame first;
/// Python lists it last.
fn keep_frames(lib: &[bool], innermost_first: bool) -> Vec<bool> {
    let n = lib.len();
    let mut keep = vec![false; n];
    let mut app_kept = 0;
    for i in 0..n {
        let throw_site = if innermost_first { i == 0 } else { i + 1 == n };
        // The library frame the user's code called (it names the failing
        // API, e.g. `Enumerable.First`): adjacent to an app frame, on the
        // callee side.
        let called_by_app = lib[i]
            && if innermost_first {
                i + 1 < n && !lib[i + 1]
            } else {
                i > 0 && !lib[i - 1]
            };
        if !lib[i] {
            if app_kept < MAX_APP_FRAMES {
                keep[i] = true;
                app_kept += 1;
            }
        } else if throw_site || called_by_app {
            keep[i] = true;
        }
    }
    keep
}

// ---- stack traces (JS, Java, .NET) --------------------------------------

fn is_js_library(frame: &str) -> bool {
    // node_modules, and Node's own core (`node:events`, `node:internal/...`).
    frame.contains("node_modules")
        || frame.contains("(node:")
        || frame.contains(" node:")
        || frame.contains("(internal/")
}
fn is_java_library(frame: &str) -> bool {
    let f = frame.trim_start().trim_start_matches("at ");
    [
        "java.",
        "javax.",
        "jakarta.",
        "jdk.",
        "sun.",
        "com.sun.",
        "kotlin.",
        "kotlinx.",
        "scala.",
        "org.springframework.",
        "org.apache.",
        "org.hibernate.",
        "io.netty.",
        "reactor.",
        "org.junit.",
        "com.fasterxml.",
    ]
    .iter()
    .any(|p| f.starts_with(p))
}
fn is_dotnet_library(frame: &str) -> bool {
    let f = frame.trim_start().trim_start_matches("at ");
    [
        "System.",
        "Microsoft.",
        "Npgsql.",
        "Newtonsoft.",
        "Xunit.",
        "NUnit.",
    ]
    .iter()
    .any(|p| f.starts_with(p))
        || f.starts_with("---") // "--- End of stack trace from previous location ---"
}

/// Every non-frame line (messages, "Caused by:", "... 42 more") stays;
/// runs of frames go through `keep_frames`.
fn stack(text: &str, frame: &Regex, is_library: fn(&str) -> bool) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    // Only frame lines (and .NET's "--- End of stack trace ---" separators)
    // form a run. `is_library` classifies frames inside a run; applied to
    // any line it would swallow messages like "System.InvalidOperation...".
    let in_run = |l: &str| frame.is_match(l) || l.trim_start().starts_with("--- ");
    while i < lines.len() {
        if !in_run(lines[i]) {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && in_run(lines[i]) {
            i += 1;
        }
        let run: Vec<Vec<&str>> = lines[start..i].iter().map(|l| vec![*l]).collect();
        let lib: Vec<bool> = lines[start..i].iter().map(|l| is_library(l)).collect();
        let keep = keep_frames(&lib, true);
        let indent: String = lines[start]
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect();
        emit_with_markers(&run, &keep, &indent, "library/framework frames", &mut out);
    }
    out.join("\n")
}

// ---- Python -------------------------------------------------------------

fn is_python_library(file_line: &str) -> bool {
    [
        "site-packages",
        "dist-packages",
        "/lib/python",
        "\\lib\\",
        "<frozen ",
        "/usr/lib/",
    ]
    .iter()
    .any(|p| file_line.contains(p))
}

/// Per traceback: all frames of the user's code stay, plus the frame where
/// it was raised and the library call the user's code made; the exception
/// lines and chained tracebacks ("During handling of the above
/// exception...") are untouched.
fn python(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].starts_with("Traceback (most recent call last):") {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        out.push(lines[i].to_string());
        i += 1;
        // frames: "  File ..." followed by indented code / caret lines
        let mut frames: Vec<Vec<&str>> = Vec::new();
        while i < lines.len() && lines[i].starts_with("  ") {
            if lines[i].trim_start().starts_with("File \"") || frames.is_empty() {
                frames.push(vec![lines[i]]);
            } else if let Some(f) = frames.last_mut() {
                f.push(lines[i]);
            }
            i += 1;
        }
        let lib: Vec<bool> = frames.iter().map(|f| is_python_library(f[0])).collect();
        let keep = keep_frames(&lib, false);
        emit_with_markers(&frames, &keep, "  ", "library frames", &mut out);
    }
    out.join("\n")
}

// ---- Go / Rust panics ---------------------------------------------------

fn is_go_library(func_line: &str, path_line: &str) -> bool {
    let f = func_line.trim_start().trim_start_matches("created by ");
    [
        "runtime.",
        "sync.",
        "internal/",
        "reflect.",
        "syscall.",
        "net.",
        "net/",
        "os.",
        "io.",
        "bufio.",
        "encoding/",
        "testing.",
        "context.",
    ]
    .iter()
    .any(|p| f.starts_with(p))
        || ["/go/src/", "/libexec/src/", "/pkg/mod/golang.org/"]
            .iter()
            .any(|p| path_line.contains(p))
}

/// Every goroutine header (its state matters — e.g. in a deadlock) and the
/// panic/fatal message stay; inside each goroutine, the user's frames stay
/// and runtime/stdlib frames are collapsed. A frame is a function line plus
/// its `\tfile.go:12 +0x1d` line.
fn go_panic(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].starts_with("goroutine ") {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        out.push(lines[i].to_string());
        i += 1;
        let mut frames: Vec<Vec<&str>> = Vec::new();
        while i < lines.len() && !lines[i].trim().is_empty() && !lines[i].starts_with("goroutine ")
        {
            let has_path = lines.get(i + 1).is_some_and(|n| n.starts_with('\t'));
            let size = if has_path { 2 } else { 1 };
            frames.push(lines[i..(i + size).min(lines.len())].to_vec());
            i += size;
        }
        let lib: Vec<bool> = frames
            .iter()
            .map(|f| is_go_library(f[0], f.get(1).copied().unwrap_or("")))
            .collect();
        let keep = keep_frames(&lib, true);
        emit_with_markers(&frames, &keep, "", "runtime/stdlib frames", &mut out);
    }
    out.join("\n")
}

/// The panic line(s) and every frame from the user's crates; std/core/
/// runtime frames are collapsed in place.
fn rust_panic(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(bt) = lines.iter().position(|l| l.trim() == "stack backtrace:") else {
        return text.to_string();
    };
    let mut out: Vec<String> = lines[..=bt].iter().map(|l| l.to_string()).collect();
    let mut frames: Vec<Vec<&str>> = Vec::new();
    let mut tail: Vec<&str> = Vec::new();
    let mut i = bt + 1;
    while i < lines.len() {
        let l = lines[i];
        // "  12: crate::func" optionally followed by "      at src/x.rs:3:5"
        if l.trim_start()
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            let has_loc = lines
                .get(i + 1)
                .is_some_and(|n| n.trim_start().starts_with("at "));
            let size = if has_loc { 2 } else { 1 };
            frames.push(lines[i..i + size].to_vec());
            i += size;
        } else {
            tail.push(l); // "note: ..." and anything after the backtrace
            i += 1;
        }
    }
    let lib: Vec<bool> = frames
        .iter()
        .map(|f| rust_std_frame().is_match(f[0]))
        .collect();
    // The throw site in a Rust backtrace is panic machinery (always std);
    // only the user's frames and the std call they made are informative.
    let mut keep = keep_frames(&lib, true);
    if let Some(first) = keep.first_mut() {
        *first = !lib[0];
    }
    emit_with_markers(&frames, &keep, "", "std/runtime frames", &mut out);
    out.extend(tail.iter().map(|l| l.to_string()));
    out.join("\n")
}

// ---- timestamped logs ---------------------------------------------------

/// A stack trace inside an app log (printed under an ERROR line) gets the
/// same frame treatment as a pasted one.
fn embedded_stacks(text: &str) -> String {
    let frames = |re: &Regex| text.lines().filter(|l| re.is_match(l)).count();
    if frames(java_frame()) >= 2 {
        stack(text, java_frame(), is_java_library)
    } else if frames(dotnet_frame()) >= 2 {
        stack(text, dotnet_frame(), is_dotnet_library)
    } else if frames(js_frame()) >= 2 {
        stack(text, js_frame(), is_js_library)
    } else {
        text.to_string()
    }
}

/// Keeps every problem line (errors, warnings, HTTP 4xx/5xx, timeouts,
/// refused connections...) with CONTEXT lines around it, everything
/// attached to it (continuation lines without a timestamp, e.g. a stack
/// trace), and the first and last line (so the time span stays visible).
/// Routine lines in between become one counted marker each.
///
/// A log with no problem line at all is returned unchanged: without an
/// error to anchor on, there's no way to tell which lines the user cares
/// about (latency, a specific request...).
fn timestamped(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    if !lines.iter().any(|l| problem_line().is_match(l)) {
        return text.to_string();
    }
    let mut keep = vec![false; n];
    if n > 0 {
        keep[0] = true;
        keep[n - 1] = true;
    }
    for (i, l) in lines.iter().enumerate() {
        if !problem_line().is_match(l) {
            continue;
        }
        for k in keep
            .iter_mut()
            .take((i + CONTEXT + 1).min(n))
            .skip(i.saturating_sub(CONTEXT))
        {
            *k = true;
        }
        let mut j = i + 1;
        while j < n && !timestamp().is_match(lines[j]) {
            keep[j] = true;
            j += 1;
        }
    }
    let items: Vec<Vec<&str>> = lines.iter().map(|l| vec![*l]).collect();
    let mut out = Vec::new();
    emit_with_markers(&items, &keep, "", "routine log lines", &mut out);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn js_trace() -> String {
        let mut s =
            String::from("TypeError: Cannot read properties of undefined (reading 'map')\n");
        s.push_str("    at ProductList (/app/src/components/ProductList.tsx:42:18)\n");
        s.push_str("    at renderWithHooks (/app/node_modules/react-dom/cjs/react-dom.development.js:16305:18)\n");
        for i in 0..30 {
            s.push_str(&format!("    at beginWork (/app/node_modules/react-dom/cjs/react-dom.development.js:{i}:10)\n"));
        }
        s.push_str("    at Page (/app/src/app/page.tsx:12:5)\n");
        s
    }

    #[test]
    fn js_keeps_message_and_app_frames() {
        let t = js_trace();
        assert_eq!(detect(&t), LogKind::JsStack);
        let out = compact(&t, LogKind::JsStack);
        assert!(out.starts_with("TypeError: Cannot read properties of undefined"));
        assert!(out.contains("ProductList.tsx:42:18"));
        assert!(out.contains("page.tsx:12:5"));
        assert!(
            out.contains("[+30 lines omitted: 30 library/framework frames]"),
            "{out}"
        );
        assert!(out.len() * 4 < t.len());
    }

    #[test]
    fn python_keeps_every_user_frame() {
        // The user's code calls into `requests`, which fails deep inside:
        // the last frames are all library frames — the user's must survive.
        let mut t = String::from("Traceback (most recent call last):\n");
        t.push_str("  File \"/app/main.py\", line 10, in <module>\n    sync_orders()\n");
        t.push_str("  File \"/app/orders.py\", line 42, in sync_orders\n    resp = requests.get(url, timeout=5)\n");
        for i in 0..8 {
            t.push_str(&format!("  File \"/usr/lib/python3.12/site-packages/requests/m{i}.py\", line {i}, in f{i}\n    call_{i}()\n"));
        }
        t.push_str("requests.exceptions.ConnectTimeout: HTTPSConnectionPool(host='api.acme.io', port=443)\n");
        assert_eq!(detect(&t), LogKind::PythonTraceback);
        let out = compact(&t, LogKind::PythonTraceback);
        assert!(
            out.contains("/app/main.py\", line 10") && out.contains("/app/orders.py\", line 42"),
            "{out}"
        );
        assert!(out.contains("requests.get(url, timeout=5)"));
        assert!(out.contains("requests/m0.py")); // the library call the user made
        assert!(out.contains("requests/m7.py")); // where it was raised
        assert!(
            out.contains("[+12 lines omitted: 6 library frames]"),
            "{out}"
        );
        assert!(out.ends_with("HTTPSConnectionPool(host='api.acme.io', port=443)"));
    }

    #[test]
    fn java_keeps_caused_by_and_app_frames() {
        let mut t = String::from(
            "org.springframework.web.util.NestedServletException: Request processing failed\n",
        );
        t.push_str("\tat org.springframework.web.servlet.FrameworkServlet.processRequest(FrameworkServlet.java:1014)\n");
        for i in 0..20 {
            t.push_str(&format!(
                "\tat org.apache.catalina.core.X.invoke(X.java:{i})\n"
            ));
        }
        t.push_str("Caused by: java.lang.NullPointerException: user is null\n");
        t.push_str("\tat com.acme.orders.OrderService.place(OrderService.java:88)\n");
        t.push_str("\tat com.acme.orders.OrderController.create(OrderController.java:41)\n");
        for i in 0..10 {
            t.push_str(&format!(
                "\tat java.base/jdk.internal.reflect.M.invoke(M.java:{i})\n"
            ));
        }
        assert_eq!(detect(&t), LogKind::JavaStack);
        let out = compact(&t, LogKind::JavaStack);
        assert!(out.contains("Caused by: java.lang.NullPointerException: user is null"));
        assert!(out.contains("OrderService.java:88") && out.contains("OrderController.java:41"));
        assert!(!out.contains("X.java:5"));
    }

    #[test]
    fn dotnet_keeps_exception_and_user_frames() {
        let mut t =
            String::from("System.InvalidOperationException: Sequence contains no elements\n");
        t.push_str("   at System.Linq.ThrowHelper.ThrowNoElementsException()\n");
        t.push_str("   at CustomerScore.API.Services.ScoreService.Get(Int32 id) in /src/CustomerScore.API/Services/ScoreService.cs:line 57\n");
        for i in 0..15 {
            t.push_str(&format!(
                "   at Microsoft.AspNetCore.Mvc.Infrastructure.X.Y{i}()\n"
            ));
        }
        assert_eq!(detect(&t), LogKind::DotnetStack);
        let out = compact(&t, LogKind::DotnetStack);
        assert!(out.contains("ScoreService.cs:line 57"));
        assert!(out.starts_with("System.InvalidOperationException: Sequence contains no elements"));
        // Where it was thrown stays, even though it's a framework frame.
        assert!(out.contains("ThrowNoElementsException"), "{out}");
        assert!(!out.contains("Y7()"));
    }

    #[test]
    fn timestamped_keeps_errors_with_context() {
        let mut t = String::new();
        for i in 0..40 {
            t.push_str(&format!(
                "2026-09-26T10:00:{:02}.000Z INFO request handled id={i}\n",
                i
            ));
        }
        t.push_str("2026-09-26T10:00:41.000Z ERROR db timeout after 30000ms\n");
        t.push_str("    at Pool.acquire (/app/db.js:10:5)\n");
        for i in 42..60 {
            t.push_str(&format!(
                "2026-09-26T10:00:{i}.000Z INFO request handled id={i}\n"
            ));
        }
        assert_eq!(detect(&t), LogKind::TimestampedLog);
        let out = compact(&t, LogKind::TimestampedLog);
        assert!(out.contains("ERROR db timeout after 30000ms"));
        assert!(out.contains("Pool.acquire"));
        assert!(out.contains("id=0") && out.contains("id=59")); // time span kept
        assert!(out.contains("routine log lines]"));
        assert!(out.len() * 3 < t.len());
    }

    #[test]
    fn generic_collapses_repeats_and_ansi() {
        let t = "\x1b[31mwarn: retrying\x1b[0m\nwarn: retrying\nwarn: retrying\ndone\n";
        assert_eq!(detect(t), LogKind::Generic);
        assert_eq!(compact(t, LogKind::Generic), "warn: retrying  (×3)\ndone");
    }

    /// Real logs captured on the dev machine (2026-09-26), paths anonymized.
    #[test]
    fn real_node_express_error() {
        let t = include_str!("fixtures/node-full.log");
        assert_eq!(detect(t), LogKind::JsStack);
        let out = compact(t, LogKind::JsStack);
        assert!(out.starts_with("TypeError: Cannot read properties of undefined (reading 'user')"));
        assert!(out.contains("at loadUser (/home/dev/app/node/app.js:3:45)"));
        assert!(!out.contains("node:events"), "{out}"); // Node core is library
        assert!(out.lines().count() <= 6, "{out}");
    }

    #[test]
    fn real_rust_panic_keeps_the_users_frame() {
        let t = include_str!("fixtures/rust.log");
        assert_eq!(detect(t), LogKind::RustPanic);
        let out = compact(t, LogKind::RustPanic);
        assert!(out.contains("panicked at src/main.rs:1:103"));
        assert!(out.contains("user not found"));
        assert!(out.contains("find_user"), "{out}");
        assert!(!out.contains("backtrace_rs"), "{out}");
        assert!(!out.contains("BacktraceLock"), "{out}"); // `<<std[..]` frames too
        assert!(!out.contains("lang_start"), "{out}"); // `<fn() as core[..]`, `<&dyn core[..]`
        assert!(out.contains("Option<"), "{out}"); // the std call the user made (`expect`)
    }

    #[test]
    fn real_server_log_with_a_stack_inside() {
        let t = include_str!("fixtures/app.log");
        assert_eq!(detect(t), LogKind::TimestampedLog);
        let out = compact(t, LogKind::TimestampedLog);
        assert!(out.contains("ERROR db query failed"));
        assert!(out.contains("WARN pool nearly exhausted"));
        assert!(out.contains("TypeError: Cannot read properties of null"));
        assert!(out.contains("logs.js:4:12")); // the user's frame
        assert!(!out.contains("cjs/loader:1266"), "{out}"); // Node internals collapsed
        assert!(out.len() * 5 < t.len(), "{} vs {}", out.len(), t.len());
    }

    // ---- semantic-safety guarantees ---------------------------------------

    #[test]
    fn long_lines_are_never_shortened() {
        let detail = format!("ERROR insert failed: {} at position 5231", "x".repeat(900));
        let mut t = String::new();
        for i in 0..40 {
            t.push_str(&format!("2026-09-26T10:00:{:02}.000Z INFO ok id={i}\n", i));
        }
        t.push_str(&format!("2026-09-26T10:00:41.000Z {detail}\n"));
        let out = compact(&t, detect(&t));
        assert!(
            out.contains(&detail),
            "the tail of a long line must survive"
        );
    }

    #[test]
    fn every_user_frame_is_kept_however_many() {
        let mut t = String::from("Error: boom\n");
        for i in 0..12 {
            t.push_str(&format!("    at step{i} (/app/src/pipeline.ts:{i}:1)\n"));
            t.push_str(&format!(
                "    at next (/app/node_modules/lib/index.js:{i}:1)\n"
            ));
        }
        let out = compact(&t, LogKind::JsStack);
        for i in 0..12 {
            assert!(out.contains(&format!("pipeline.ts:{i}:1")), "{out}");
        }
    }

    #[test]
    fn http_errors_without_the_word_error_are_kept() {
        let mut t = String::new();
        for i in 0..60 {
            let status = if i == 30 || i == 45 { 502 } else { 200 };
            t.push_str(&format!(
                "2026-09-26T10:00:{:02}.000Z INFO GET /api/pay 1.1 {status} 12ms\n",
                i
            ));
        }
        let out = compact(&t, detect(&t));
        assert_eq!(out.matches(" 502 ").count(), 2, "{out}");
    }

    #[test]
    fn a_log_with_no_problem_is_left_alone() {
        // Without an error to anchor on we can't know what matters (e.g.
        // the user asks about latency): nothing is dropped.
        let mut t = String::new();
        for i in 0..60 {
            t.push_str(&format!(
                "2026-09-26T10:00:{:02}.000Z INFO GET /api/cart 1.1 200 {}ms\n",
                i,
                10 + i
            ));
        }
        assert_eq!(compact(&t, detect(&t)), t);
    }

    #[test]
    fn timestamped_data_is_not_a_log() {
        let mut csv = String::from("created_at,order_id,total\n");
        for i in 0..50 {
            csv.push_str(&format!("2026-09-26 10:00:{:02},{i},99.90\n", i));
        }
        assert_eq!(detect(&csv), LogKind::Generic);
        assert_eq!(compact(&csv, detect(&csv)), csv);
    }

    #[test]
    fn go_deadlock_keeps_every_goroutine_state() {
        let mut t = String::from("fatal error: all goroutines are asleep - deadlock!\n\n");
        for g in 1..=3 {
            t.push_str(&format!("goroutine {g} [chan receive]:\n"));
            t.push_str(
                "runtime.gopark(0x0?, 0x0?, 0x0?)\n\t/usr/local/go/src/runtime/proc.go:402 +0xce\n",
            );
            t.push_str(&format!(
                "main.worker{g}(...)\n\t/app/main.go:{g}0 +0x1d\n\n"
            ));
        }
        assert_eq!(detect(&t), LogKind::GoPanic);
        let out = compact(&t, LogKind::GoPanic);
        for g in 1..=3 {
            assert!(
                out.contains(&format!("goroutine {g} [chan receive]:")),
                "{out}"
            );
            assert!(out.contains(&format!("/app/main.go:{g}0")), "{out}");
        }
    }

    #[test]
    fn kept_lines_are_byte_identical() {
        // Everything in the output is either an original line or a marker.
        for t in [
            include_str!("fixtures/node-full.log"),
            include_str!("fixtures/rust.log"),
            include_str!("fixtures/app.log"),
        ] {
            let out = compact(t, detect(t));
            for line in out.lines() {
                let marker =
                    line.trim_start().starts_with("[+") && line.contains(" lines omitted: ");
                assert!(
                    marker || t.lines().any(|l| l.trim_end() == line),
                    "changed line: {line}"
                );
            }
        }
    }

    #[test]
    fn plain_prose_is_left_alone() {
        let t = "Could you check why the build fails? It started after the upgrade.";
        assert_eq!(compact(t, detect(t)), t);
    }
}
