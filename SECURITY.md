# Security

## Reporting a vulnerability

Report privately through [GitHub Security Advisories](https://github.com/mkmuniz/schliffe-tk/security/advisories/new). Please don't open a public issue for something exploitable. Include what you did, what happened, and the version (`schliffe --version`).

## Why this project needs a threat model

Schliffe is not a library that processes data at arm's length. It installs itself **in front of every command an AI agent runs**, on a developer's machine, with that developer's files, shell and credentials. It then **stores what those commands printed**. Two consequences shape everything below:

- **Its inputs are hostile by default.** Command output, MCP results, pasted logs and images all come from places an attacker may control — an untrusted repository the agent was asked to work on, a third-party MCP server, a log pasted from production.
- **Its output is read by an AI that acts on it.** Text Schliffe emits becomes instructions-adjacent context. Anything Schliffe can be talked into doing, an attacker can try to reach through prompt injection.

## What Schliffe does not do

These are design constraints, enforced by tests and by `deny.toml`:

- **Network access is narrow and explicit.** The binary can connect to a user-selected MCP HTTP(S) endpoint when `schliffe mcp --url` is used. The remote path uses `reqwest` with Rustls, accepts only HTTP(S) URLs, applies bounded response reads, and never downloads or executes code. The installer is the only other component that performs network downloads.
- **No telemetry.** Nothing is sent to Schliffe or a third-party analytics service. MCP traffic is sent only to the endpoint explicitly configured by the user. `schliffe stats` and `schliffe report` read local files and print to your terminal.
- **No code execution from configuration.** Layer B filters are declarative TOML with a fixed action catalogue; there is no scripting engine, and embedding one is banned.
- **No dynamic loading.** No plugin `.so`/`.dylib` is loaded.
- **No privilege escalation.** The installer never uses `sudo` and writes only under `$HOME`.

## Fixed vulnerabilities

Each was **verified exploitable** on a real machine before the fix, and each has a regression test that the `Security` workflow runs on every PR and daily.

| Issue | Impact | Fix | Test |
|---|---|---|---|
| **Path traversal in `schliffe show`** | `schliffe show ../../../../etc/passwd` read arbitrary files. Reachable through prompt injection: Schliffe's own recovery hints (`schliffe show <hash>`) sit in text the model reads, so a crafted log or commit message could have produced a hint the agent would run. | The argument must be exactly 16 lowercase hex characters (`core::secure::is_store_hash`) before it reaches a path. | `only_real_store_hashes_are_accepted`, `show_refuses_anything_that_is_not_a_store_hash` |
| **Code execution via a relative `$PATH` entry** | With `.` (or an empty entry) in `$PATH`, an executable named `git` shipped inside a cloned repository was executed by the shim. Confirmed: a planted `./git` ran. | Non-absolute `$PATH` entries are skipped. Escape hatch: `SCHLIFFE_ALLOW_RELATIVE_PATH=1`. | `relative_path_entries_are_never_used` |
| **Secrets written world-readable** | The store keeps **raw command output** — a `git diff` touching a `.env`, a stack trace with a connection string — and the prompt hook keeps the user's prompt verbatim. All were created `0644`: readable by any other local account. | Every file Schliffe writes is `0600` and every directory `0700`, applied to existing installs too. | `written_files_are_owner_only_and_dirs_too`, `everything_written_is_owner_only` |
| **Crash on a multi-byte argument** | `schliffe show "€x"` panicked while slicing the first two bytes of a UTF-8 string. | Validation happens before any slicing. | `only_real_store_hashes_are_accepted` |
| **Symlink at a store destination** | A symlink planted at a store path would have been written *through*, letting a local attacker redirect command output into a file of their choosing. | Writes go to a fresh `O_EXCL` temp file and are `rename`d into place, which replaces a symlink instead of following it. | `a_symlink_at_the_destination_is_replaced_not_followed` |
| **Unbounded input** | A hostile MCP peer or runaway tool could allocate without limit (no newline, no EOF). | Hook stdin is capped at 64 MiB. Command capture is capped at 64 MiB per stream, then forwards all bytes raw. MCP enforces 64 MiB per message in both directions without a message queue; oversized frames terminate the connection without forwarding partial JSON. | `oversized_input_is_refused`, `oversized_captured_streams_pass_through_exactly_and_keep_exit_code`, `mcp_oversized_client_frame_stops_the_server`, `mcp_oversized_server_frame_is_not_forwarded_or_allowed_to_hang` |
| **MCP lifetime cap and shutdown hang** | The old 64 MiB cap applied to the entire server stream, cutting off valid long sessions; waiting for child exit before responding could hang. | Reset the budget on each frame; report pending errors before reaping the child, with a one-second grace period after server stdout closes. Active requests have no execution timeout. | `mcp_session_can_exceed_64_mib_with_small_frames`, `mcp_stdout_eof_does_not_wait_forever_for_child_exit`, `mcp_client_eof_allows_a_slow_valid_response` |
| **Image decompression bomb** | A ~1 MB PNG declaring 20000×20000 expands to 1.2 GB. The `image` crate happens to refuse it today, but a security property must not rest on a dependency's default. | An explicit 256 MB decode ceiling. | `decompression_bomb_is_refused` |

## Standing defences

**Fail-open, always.** Every layer is written so that an error, an unrecognized shape or a refused input results in the *original, unmodified* output reaching the agent. Oversized command streams are forwarded raw rather than stored or compressed. MCP framing errors are a protocol exception: an oversized or invalid-UTF-8 message terminates the connection with diagnostics and errors for tracked pending requests; truncated JSON is never forwarded. At most 1,024 client requests may be pending; additional requests and duplicate pending IDs receive an error. Valid MCP sessions have no total byte limit.

**Never fabricate a result.** Business rules 2 and 5 (`specs.md` §4) forbid turning a failure into an apparent success — the case that makes a compressor actively dangerous to an agent.

**Supply chain.** 60 dependencies, all from crates.io, pinned by `Cargo.lock`, built with `--locked`. On every PR and daily:

- `cargo audit` against the RustSec advisory database;
- `cargo deny` for advisories, yanked crates, wildcard requirements, banned crates (see above), licence allow-list and registry allow-list;
- `gitleaks` over the full history;
- every `unsafe` must carry a `SAFETY:` comment — there are two sites, both reviewed (the macOS parent-process lookup, and `env::set_var` inside tests).

**Releases.** Binaries are built by GitHub Actions from a tagged commit, and `SHA256SUMS` is published with them. The installer refuses an archive whose checksum doesn't match and never installs an unverified binary. Verified by the `Installers` workflow, including a deliberately tampered archive.

## Residual risks

Stated plainly, because pretending otherwise would be the real problem:

- **`curl … | bash` trusts GitHub.** The checksum is published alongside the archive, so it proves integrity of the download, not that the release itself is honest. If you need more, clone the repository, read it, and use `--from-source`. Signed releases (sigstore) are not implemented.
- **A script the agent runs through a shell is still filtered.** `bash deploy.sh` → `git diff` looks exactly like a command the model ran. If such a script parses git output, set `SCHLIFFE_DISABLE=1` inside it.
- **Library-vs-app frame detection is a heuristic** (`docs/pasted-logs.md`). A project whose own namespace mimics a framework's could have its frames collapsed — recoverable, never deleted.
- **Local attacker with your user account.** Schliffe protects against *other* accounts on the machine; nothing protects you from code already running as you.
- **The Claude Code hooks depend on Claude Code.** They edit `~/.claude/settings.json` (with a backup) and are refused on native Windows, where output replacement doesn't work.

## Reproducing the audit

```bash
cargo audit                 # RustSec advisories
cargo deny check            # bans, licences, sources, advisories
cargo test                  # includes every regression test above
```
