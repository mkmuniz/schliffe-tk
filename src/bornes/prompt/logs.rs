/// Recognizes what kind of log a pasted block is and compacts it without
/// losing the error: exception messages, the first frames in the user's own
/// code, and every error/warning line always stay; what goes is framework
/// and library frames, repeated lines, and runs of routine log lines — each
/// cut replaced by a counted marker.
///
/// Pure text processing (no model), deterministic, fail-open: an
/// unrecognized block only gets the generic pass (ANSI codes, duplicate
/// lines, overlong lines).
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

/// App frames kept per stack section; the rest are counted.
const APP_FRAMES: usize = 3;
/// Context lines kept around each error line in a timestamped log.
const CONTEXT: usize = 1;
const MAX_LINE: usize = 400;

pub fn detect(text: &str) -> LogKind {
    let lines: Vec<&str> = text.lines().collect();
    let count = |pred: &dyn Fn(&str) -> bool| lines.iter().filter(|l| pred(l)).count();
    if text.contains("Traceback (most recent call last):") {
        return LogKind::PythonTraceback;
    }
    if text.contains("goroutine ") && text.contains("panic:") {
        return LogKind::GoPanic;
    }
    if text.contains("panicked at")
        && (text.contains("stack backtrace:") || text.contains("thread '"))
    {
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
    if lines.len() >= 5 && count(&|l| timestamp().is_match(l)) * 2 >= lines.len() {
        return LogKind::TimestampedLog;
    }
    LogKind::Generic
}

/// Compacts `text` as the given kind. Always returns something no larger
/// than the input (falls back to the input otherwise).
pub fn compact(text: &str, kind: LogKind) -> String {
    let cleaned = generic_pass(text);
    let out = match kind {
        LogKind::JsStack => stack(&cleaned, js_frame(), is_js_library),
        LogKind::JavaStack => stack(&cleaned, java_frame(), is_java_library),
        LogKind::DotnetStack => stack(&cleaned, dotnet_frame(), is_dotnet_library),
        LogKind::PythonTraceback => python(&cleaned),
        LogKind::GoPanic => go_panic(&cleaned),
        LogKind::RustPanic => rust_panic(&cleaned),
        LogKind::TimestampedLog => timestamped(&cleaned),
        LogKind::Generic => cleaned.clone(),
    };
    if out.len() < text.len() {
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
fn timestamp() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"^\s*\[?(\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}([.,]\d+)?(Z|[+-]\d{2}:?\d{2})?|\d{2}:\d{2}:\d{2}([.,]\d+)?)\]?",
    )
}
fn severity_error() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(
        &R,
        r"(?i)\b(error|err|fatal|critical|crit|exception|panic|failed|failure|warn|warning|traceback|caused by)\b",
    )
}
fn ansi() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, "\x1b\\[[0-9;]*[a-zA-Z]")
}

/// ANSI codes out, consecutive duplicates collapsed (`×N`), overlong lines cut.
fn generic_pass(text: &str) -> String {
    let stripped = ansi().replace_all(text, "");
    let mut out: Vec<String> = Vec::new();
    let mut last: Option<String> = None;
    let mut repeats = 0;
    let flush = |out: &mut Vec<String>, repeats: usize| {
        if repeats > 0
            && let Some(l) = out.last_mut()
        {
            l.push_str(&format!("  (×{})", repeats + 1));
        }
    };
    for line in stripped.lines() {
        let line = line.trim_end();
        let line = if line.chars().count() > MAX_LINE {
            let cut: String = line.chars().take(MAX_LINE).collect();
            format!("{cut}…(+{} chars)", line.chars().count() - MAX_LINE)
        } else {
            line.to_string()
        };
        if last.as_deref() == Some(line.as_str()) && !line.is_empty() {
            repeats += 1;
            continue;
        }
        flush(&mut out, repeats);
        repeats = 0;
        out.push(line.clone());
        last = Some(line);
    }
    flush(&mut out, repeats);
    // Runs of blank lines → one.
    let mut compact: Vec<String> = Vec::new();
    for l in out {
        if l.is_empty() && compact.last().is_some_and(|p: &String| p.is_empty()) {
            continue;
        }
        compact.push(l);
    }
    compact.join("\n").trim().to_string()
}

// ---- stack traces (JS, Java, .NET) --------------------------------------

fn is_js_library(frame: &str) -> bool {
    frame.contains("node_modules")
        || frame.contains("node:internal")
        || frame.contains("(internal/")
}
fn is_java_library(frame: &str) -> bool {
    let f = frame.trim_start().trim_start_matches("at ");
    [
        "java.",
        "javax.",
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

/// Keeps every non-frame line (messages, "Caused by:", "--- End of inner
/// exception ---"), and per run of frames: the first APP_FRAMES frames in
/// user code plus the very first frame (where it was thrown). Library
/// frames and extra app frames are counted into one marker per run.
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
        let run = &lines[start..i];
        let mut kept = 0;
        let mut hidden_lib = 0;
        let mut hidden_app = 0;
        for (n, f) in run.iter().enumerate() {
            let lib = is_library(f);
            if n == 0 || (!lib && kept < APP_FRAMES) {
                out.push(f.to_string());
                if !lib {
                    kept += 1;
                }
            } else if lib {
                hidden_lib += 1;
            } else {
                hidden_app += 1;
            }
        }
        let mut parts = Vec::new();
        if hidden_app > 0 {
            parts.push(format!("{hidden_app} app"));
        }
        if hidden_lib > 0 {
            parts.push(format!("{hidden_lib} library/framework"));
        }
        if !parts.is_empty() {
            out.push(format!(
                "    [+{} lines omitted: {} frames]",
                hidden_app + hidden_lib,
                parts.join(" + ")
            ));
        }
    }
    out.join("\n")
}

// ---- Python -------------------------------------------------------------

/// Per traceback: header, the last 3 frames (each "File ..." + code line),
/// and the exception line(s). Chained tracebacks ("During handling of the
/// above exception...") each get the same treatment.
fn python(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with("Traceback (most recent call last):") {
            out.push(lines[i].to_string());
            i += 1;
            // frames: "  File ..." followed by indented code lines
            let mut frames: Vec<Vec<&str>> = Vec::new();
            while i < lines.len() && lines[i].starts_with("  ") {
                if lines[i].trim_start().starts_with("File \"") {
                    frames.push(vec![lines[i]]);
                } else if let Some(f) = frames.last_mut() {
                    f.push(lines[i]);
                }
                i += 1;
            }
            let hidden = frames.len().saturating_sub(APP_FRAMES);
            if hidden > 0 {
                let hidden_lines: usize = frames[..hidden].iter().map(Vec::len).sum();
                out.push(format!(
                    "  [+{hidden_lines} lines omitted: {hidden} earlier frames]"
                ));
            }
            for f in &frames[hidden..] {
                out.extend(f.iter().map(|l| l.to_string()));
            }
        } else {
            out.push(lines[i].to_string());
            i += 1;
        }
    }
    out.join("\n")
}

// ---- Go / Rust panics ---------------------------------------------------

/// The panic message and the first goroutine's first frames; other
/// goroutines are counted.
fn go_panic(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut goroutines = 0;
    let mut in_first = false;
    let mut frame_lines = 0;
    let mut hidden = 0;
    for line in text.lines() {
        if line.starts_with("goroutine ") {
            goroutines += 1;
            in_first = goroutines == 1;
            if in_first {
                out.push(line.to_string());
            } else {
                hidden += 1;
            }
            frame_lines = 0;
            continue;
        }
        if goroutines == 0 {
            out.push(line.to_string());
        } else if in_first && frame_lines < APP_FRAMES * 2 {
            out.push(line.to_string());
            frame_lines += 1;
        } else if !line.trim().is_empty() {
            hidden += 1;
        }
    }
    if hidden > 0 {
        out.push(format!(
            "[+{hidden} lines omitted: remaining frames of {goroutines} goroutine(s)]"
        ));
    }
    out.join("\n")
}

/// The panic line(s) and, in a backtrace, only frames from the user's
/// crate (not `std::`, `core::`, `tokio::`, `rustc` internals).
fn rust_panic(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(bt) = lines.iter().position(|l| l.trim() == "stack backtrace:") else {
        return text.to_string();
    };
    let mut out: Vec<String> = lines[..=bt].iter().map(|l| l.to_string()).collect();
    let lib = |l: &str| {
        let t = l.trim_start();
        [
            "std::",
            "core::",
            "alloc::",
            "tokio::",
            "futures",
            "rust_begin_unwind",
            "__rust",
            "<core::",
            "<alloc::",
            "<std::",
            "at /rustc/",
        ]
        .iter()
        .any(|p| t.contains(p))
    };
    let mut hidden = 0;
    let mut kept = 0;
    let mut i = bt + 1;
    while i < lines.len() {
        let l = lines[i];
        // "  12: crate::func" optionally followed by "      at src/x.rs:3:5"
        let has_loc = lines
            .get(i + 1)
            .is_some_and(|n| n.trim_start().starts_with("at "));
        let size = if has_loc { 2 } else { 1 };
        if l.trim_start()
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
            && !lib(l)
            && kept < APP_FRAMES
        {
            out.extend(lines[i..i + size].iter().map(|l| l.to_string()));
            kept += 1;
        } else if l.trim_start().starts_with("note:") {
            out.push(l.to_string());
        } else {
            hidden += size;
        }
        i += size;
    }
    if hidden > 0 {
        out.push(format!("[+{hidden} lines omitted: std/runtime frames]"));
    }
    out.join("\n")
}

// ---- timestamped logs ---------------------------------------------------

/// Keeps every error/warning line with CONTEXT lines around it, plus the
/// first and last line (so the time span stays visible); each run of
/// routine lines in between becomes one counted marker.
fn timestamped(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    let mut keep = vec![false; n];
    if n > 0 {
        keep[0] = true;
        keep[n - 1] = true;
    }
    for (i, l) in lines.iter().enumerate() {
        // A line that isn't a new log entry (no timestamp) belongs to the
        // previous one — e.g. a stack trace under an ERROR line.
        let is_error = severity_error().is_match(l);
        if is_error {
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
    }
    let mut out: Vec<String> = Vec::new();
    let mut hidden = 0;
    for (i, l) in lines.iter().enumerate() {
        if keep[i] {
            if hidden > 0 {
                out.push(format!("[+{hidden} lines omitted: routine log lines]"));
                hidden = 0;
            }
            out.push(l.to_string());
        } else {
            hidden += 1;
        }
    }
    if hidden > 0 {
        out.push(format!("[+{hidden} lines omitted: routine log lines]"));
    }
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
            out.contains("[+31 lines omitted: 31 library/framework frames]"),
            "{out}"
        );
        assert!(out.len() * 4 < t.len());
    }

    #[test]
    fn python_keeps_last_frames_and_exception() {
        let mut t = String::from("Traceback (most recent call last):\n");
        for i in 0..8 {
            t.push_str(&format!(
                "  File \"/app/lib/m{i}.py\", line {i}, in f{i}\n    call_{i}()\n"
            ));
        }
        t.push_str("KeyError: 'user_id'\n");
        assert_eq!(detect(&t), LogKind::PythonTraceback);
        let out = compact(&t, LogKind::PythonTraceback);
        assert!(
            out.contains("[+10 lines omitted: 5 earlier frames]"),
            "{out}"
        );
        assert!(out.contains("m7.py") && !out.contains("m4.py"));
        assert!(out.ends_with("KeyError: 'user_id'"));
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
        assert!(out.contains("lines omitted: routine log lines]"));
        assert!(out.len() * 3 < t.len());
    }

    #[test]
    fn generic_collapses_repeats_and_ansi() {
        let t = "\x1b[31mwarn: retrying\x1b[0m\nwarn: retrying\nwarn: retrying\ndone\n";
        assert_eq!(detect(t), LogKind::Generic);
        assert_eq!(compact(t, LogKind::Generic), "warn: retrying  (×3)\ndone");
    }

    #[test]
    fn plain_prose_is_left_alone() {
        let t = "Could you check why the build fails? It started after the upgrade.";
        assert_eq!(compact(t, detect(t)), t);
    }
}
