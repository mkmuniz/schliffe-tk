# Pasted logs — what is recognized, kept and removed

When a large log is pasted into a Claude Code prompt, Schliffe offers a compact version (see the README). This page lists exactly what happens to each kind of log.

## The one rule

**Remove only what is provably noise; keep anything in doubt.** Concretely:

- **Kept lines are never edited.** Every line in the compact version is byte-for-byte a line of the original (only ANSI color codes are stripped, and paths under the project folder become relative). No line is shortened.
- **Every removal is visible.** Each removed stretch becomes `[+N lines omitted: ...]` at the exact place it was, so order and position stay clear.
- **The model is told.** The compact block starts with a header saying what kind of lines were omitted and how to get the full original (`schliffe show <hash>`).
- **You decide.** The compact version is only offered: send the same prompt again and the original goes as is.
- **When unsure, nothing changes.** A block under 20 lines or 1.5 KB, one that isn't recognized with confidence, or one that wouldn't shrink to 60% or less is left untouched.

## Recognized logs

| Kind | Recognized by | Always kept | Removed (counted in place) |
|---|---|---|---|
| **Node.js / TypeScript stack** | `    at fn (file:line:col)` frames | Error message; every frame outside `node_modules`/Node core; the throw site; the library call your code made | `node_modules` and Node core (`node:internal`, `node:events`...) frames in between |
| **Python traceback** | `Traceback (most recent call last):` | Exception line(s); every frame outside `site-packages`/`dist-packages`/the stdlib (with its code line); where it was raised; the library call your code made; chained tracebacks | Library/stdlib frames in between |
| **Java / Kotlin / Scala stack** | `at pkg.Class.method(File.java:12)` | Exception message; every `Caused by:` / `Suppressed:` section; `... N more`; your frames; throw site; the library call your code made | `java.`, `jakarta.`, `jdk.`, `kotlin.`, `org.springframework.`, `org.apache.`, `org.hibernate.`, `io.netty.`... frames in between |
| **.NET stack** | `at Ns.Class.Method(args) in file.cs:line N` | Exception message; inner exceptions and `--- End of ... ---` markers; your frames; throw site; the library call your code made (e.g. `Enumerable.First`) | `System.`, `Microsoft.`, `Npgsql.`, `Newtonsoft.`... frames in between |
| **Go panic / fatal error** | `goroutine N [...]:` + `panic:` or `fatal error:` | The panic/fatal message; **every goroutine header and state** (they matter in a deadlock); your frames in each goroutine; the stdlib call your code made | `runtime.`, `sync.`, `net/`, `os.`... frames in between |
| **Rust panic backtrace** | `panicked at` + `stack backtrace:` | Panic message and location; every frame from your crates; the std call your code made (e.g. `Option::expect`); `note:` lines | `std`/`core`/`alloc`/`tokio` frames, including the `std[hash]::` form of recent Rust |
| **Application log** | Mostly timestamped lines **and** log levels (`INFO`, `WARN`, `ERROR`...) or HTTP request lines | Every problem line — errors, warnings, exceptions, **HTTP 4xx/5xx**, timeouts, refused/denied, OOM, deadlock, `exit code N`... — with 2 lines of context each; anything attached to it (e.g. a stack trace under an ERROR line, itself compacted as above); the first and last line (time span) | Routine lines far from any problem |
| **Anything else** | — | Everything | Only ANSI color codes; consecutive identical lines collapse to one line with `(×N)` |

## Deliberately left untouched

- **An application log with no problem line.** Without an error to anchor on, there's no way to know what you're asking about (latency, a specific request...), so nothing is dropped.
- **Timestamped data that isn't a log** — a CSV export, query results — has no log levels and isn't treated as a log.
- **Short pastes** (under 20 lines / 1.5 KB): the saving isn't worth any risk.

## Known limits

- Library frames are recognized by name/path prefix. A project whose own code lives under a prefix that looks like a framework (e.g. its own `org.apache.*` package) would have its frames counted as library frames — still recoverable with `schliffe show`.
- Deep recursion keeps up to 30 frames of your code per stack section.
