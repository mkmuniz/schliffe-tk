# Changelog

All notable changes to this project are recorded here. Format loosely inspired by [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Security

Full write-up, including how each was exploited and its regression test: [`SECURITY.md`](SECURITY.md).

- **Arbitrary file read via `schliffe show`** (path traversal). `schliffe show ../../../../etc/passwd` read any file. Reachable through prompt injection, since Schliffe's own recovery hints sit in text the model reads. The argument must now be exactly 16 lowercase hex characters.
- **Code execution via a relative `$PATH` entry.** With `.` in `$PATH`, an executable named `git` inside a cloned repository was run by the shim. Non-absolute entries are now skipped (`SCHLIFFE_ALLOW_RELATIVE_PATH=1` restores the old behavior).
- **Secrets written world-readable.** The store keeps raw command output and the prompt hook keeps the prompt verbatim, all created `0644`. Everything Schliffe writes is now `0600` (directories `0700`), and `install.sh` repairs existing installs.
- **Symlink at a store destination** was written through; writes are now `O_EXCL` temp file + `rename`.
- **Crash** on a multi-byte argument to `schliffe show`.
- **Unbounded input** from hook stdin and the MCP proxy, now capped at 64 MB.
- **Image decompression bomb** now refused by an explicit decode ceiling instead of a dependency's default.
- New `Security` workflow (PRs + daily): `cargo audit`, `cargo deny` (`deny.toml`: banned crates, licence and registry allow-lists), `gitleaks`, a regression test per vulnerability, and a `SAFETY:` comment required on every `unsafe`.

### Added
- **One-command install**: `curl -fsSL https://raw.githubusercontent.com/mkmuniz/schliffe-tk/main/install.sh | bash` downloads the latest release's prebuilt binary, verifies it against `SHA256SUMS` (never installs an unverified archive), and needs no Rust. Inside a clone with Rust it still builds that checkout; `--prebuilt` / `--from-source` force either. `install.ps1` gets the same (`-Prebuilt` / `-FromSource`, `irm … | iex`). A new `Installers` workflow runs both installers on Linux, macOS and Windows runners, including a tampered-archive check.
- `schliffe report [--days N]`: where the tokens of Claude Code sessions go, from its transcripts (read-only) — cost split into re-reads / new content / replies / thinking, the most expensive sessions with their peak context, what fills the conversations by source (conversation, shell, MCP per server, file reads, images by billed pixels), Schliffe's share, and up to three levers tied to the numbers.

## [0.4.0] — 2026-09-27

### Fixed
- **Agent detection counted every process under the agent.** Claude Code's own internal `git` calls (and anything polling `docker context`) inherit `CLAUDECODE`, so ~8k non-model calls a day were logged in `stats`, and a program parsing `git log` could have received compacted text. Filtering now also requires the command's parent to be a shell — how the model's commands are always run.
- **`cargo test` could report success on a failed run.** The filter only read the last `test result:` line; with several suites (unit + integration + doc-tests), a failure in an early suite followed by a passing last one read as `test result: ok` (only the exit code was right). It now reads every suite.

### Added
- **Pasted logs** (Claude Code `UserPromptSubmit` hook): recognizes stack traces (Node/TS, Python, Java/Kotlin, .NET, Go, Rust) and timestamped app logs pasted into a prompt, and copies a compact version of the whole prompt to the clipboard — errors and your own frames kept, the original recoverable. Sending the same prompt again sends the original. `SCHLIFFE_PROMPT_LOGS=block|tip|off`.
- Pasted-log compaction follows one rule — remove only provable noise, keep anything in doubt: kept lines are never edited or shortened, every user frame and the library call it made stay, app logs keep HTTP 4xx/5xx and other problem lines, logs with no problem line and timestamped data (CSV) are left untouched, and the compact block tells the model what was omitted. Details: `docs/pasted-logs.md`.
- **Long-conversation notice**: once per threshold (200k/400k/600k/800k tokens of context), a one-line suggestion to start a new conversation or `/compact`. `SCHLIFFE_CONTEXT_WARN_AT`.
- `cargo test`: one totals line when everything passes (`cargo test: 89 passed; 0 failed (2 suites, 17.71s)`); on failure, each suite's result plus every failing test with where it panicked and the message.
- `git pull`/`git merge`: a clean fast-forward or merge becomes one line (range + summary); conflicts and errors are kept verbatim.
- `git branch -a`/`-r`: remote branches grouped per remote, prefix stripped, mirrors of local branches counted instead of listed, long lists capped (recoverable).
- `docker images`/`docker ps`: column padding removed (cells joined with ` | `), long tables capped (recoverable).
- Benchmark on 25 real commands (README: −73% of all bytes, median −74% per command).

## [0.3.0] — 2026-09-25

### Changed
- **Renamed from Elagix to Schliffe** (German: *the cuts* — the facets of a cut gem). Command `schliffe`, folder `~/.schliffe`, environment variables `SCHLIFFE_*`. `install.sh` migrates an existing Elagix install: moves the stats history, store and custom filters, removes the old shims and PATH lines (with backups) and re-registers the Claude Code hook. The binary still answers to `elagix` for sessions started before the switch.

### Added
- Claude Code `PostToolUse` hook (`schliffe hook install|uninstall`, registered automatically by `install.sh`): compresses results from remote MCP servers (HTTP/OAuth, e.g. Figma) and shrinks images (MCP screenshots, Read on PNG/JPEG) to a 1280px long edge. macOS, Linux and WSL only — not native Windows.

### Fixed
- `install.sh` deleted existing shims for tools it couldn't see from a trimmed environment (e.g. an agent shell without nvm); it now also checks the user's full shell PATH and never removes a shim.

## [0.2.0] — 2026-09-25

First MVP meant for day-to-day use: validated on macOS (Apple Silicon, zsh) with Claude Code in VS Code.

### Added
- `schliffe stats`: savings report (24h / 7 days / all time, top savers, commands that ran with no filter). Size-only log, never arguments or output.
- `schliffe --version`.
- Layer B: filters can target stderr (`stream = "stderr" | "both"`), `match_any` for rules reached through several invocations, `|` alternatives in `match_args_prefix`, and new actions `compact_path`, `collapse_lines_matching`, `squeeze_spaces`.
- New filters, validated against real output: `npm`/`pnpm`/`yarn`/`pip` install, JS builds via the package manager (Next.js, Vite), linters via the package manager (ESLint), `git pull`/`merge`, `docker pull`/`build`/`compose build|pull`, `dotnet`, cargo's stderr progress, `go`.
- MCP proxy: `--keep-schemas` (recommended for Claude Code), recovery hint for trimmed results, JSON-RPC error when the server dies mid-call.
- End-to-end test suite against the compiled binary (`tests/e2e.rs`), including determinism.
- CI job for macOS; release workflow with prebuilt binaries.

### Changed
- Filtering only happens for AI agents (`CLAUDECODE` / `AI_AGENT` / `SCHLIFFE_FORCE`); humans, editors, git hooks and scripts get untouched output. `SCHLIFFE_DISABLE=1` opts out.
- Commands with no filter stream live instead of being captured (dev servers work normally).
- `git log`: one line per commit instead of only the first commit.
- Dedup is scoped to the agent's real session id (`CLAUDE_CODE_SESSION_ID`).
- Sentence splitter handles abbreviations, initials and bullet lists.
- `install.sh`: zsh PATH line at the end of `.zshrc`, shims only for installed tools, binary copied to `~/.schliffe/bin`, `schliffe` itself on PATH.

### Fixed
- The shim could resolve itself as the real binary and recurse until fork failed.
- MCP: JSON *file contents* read through a file tool were being compacted (corruption risk) — file-reading tools are never touched now.
- MCP: pending requests hung forever when the server died.
- Windows: `npm`/`pnpm`/`yarn` (`.cmd`) were never found by their shims.
- Missing real binary now exits 127 like "command not found".

## [0.1.0] — 2026-07-26

First release. Milestones M0-M8 complete (full history in [`MILESTONES.md`](MILESTONES.md)).

### Added
- `bornes/comandos`: `$PATH` shim, Layer A (dedicated parsers for `git status`/`log`/`diff`/`show`, `pytest`, `cargo test`), and Layer B (declarative TOML rule engine, with example filters for `docker images`, `git branch`, `terraform plan`, `npm install`).
- `bornes/mcp`: JSON-RPC proxy over stdio with tool schema lazy-loading and call-result compression.
- `bornes/prosa`: TF-IDF extractive summarization, integrated into commit message bodies (`git log`/`git show`) and available as a standalone utility (`schliffe compress`).
- `core/store`: content-addressed store — `git show <sha>` cache, progressive disclosure (`schliffe show <hash>`), time-window deduplication, automatic expiration-based cleanup.
- `install.sh` (Linux/macOS/WSL) and `install.ps1` (Windows) installers.
- Cross-compilation validated for Windows (`x86_64-pc-windows-gnu`).

### Known to be incomplete
See [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) — notably: Layer B command coverage is still small, no macOS build, `install.ps1` has no fully validated activation, `bornes/mcp` has never been wired up to a real server.
