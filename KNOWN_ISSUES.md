# Schliffe — Known issues and improvements

A consolidation of everything marked "left for later" across M0-M8 (previously scattered across `specs.md` §13 and each milestone's notes in `MILESTONES.md`). Nothing here blocks current use — these are known gaps, not hidden bugs.

## Platform / installation

- **`install.ps1` not validated with full activation.** Only tested in safe mode (an isolated copy, without touching the real PATH) — this development machine has no native `git`/`cargo`/`npm` on Windows (they only exist via WSL), so there was no way to validate intercepting a real native binary. Needs to run on a Windows machine with a native toolchain installed.
- **macOS: native build only, no cross-compile from Linux.** Builds and passes all tests natively on Apple Silicon (2026-09-24), and CI now has a `macos-latest` job. Cross-compiling *from* Linux is still blocked by the missing SDK (MILESTONES.md M8) — not needed while CI builds natively.
- **Shims only activate inside AI agents.** Since 2026-09-24 the shim filters only when an agent marker is set (`CLAUDECODE`, `AI_AGENT`, or `SCHLIFFE_FORCE=1`; `SCHLIFFE_DISABLE=1` turns it off). Other agents that set none of these pass through unfiltered until they're added or the user sets `SCHLIFFE_FORCE=1` in that tool's environment.
- **"The model's command" = an agent marker AND a shell parent.** Programs that call a shimmed tool directly (the agent's internals, MCP servers, test runners, node scripts) are never filtered. Scripts the model runs through a shell (`bash deploy.sh` → `git diff`) still are — the shim can't tell them apart from the model's own commands; use `SCHLIFFE_DISABLE=1` inside such a script if it parses git output. Claude Code hooks run through a shell too.
- **Redirects inside an agent are still filtered.** Claude Code captures command output into a regular file, so `git diff > x.patch` run *by the agent* is indistinguishable from normal capture and gets filtered (an invalid patch). Pipes into other programs (`git log | grep`) are filtered too. Workaround: `SCHLIFFE_DISABLE=1 git diff > x.patch`.
- **Python venvs / version managers can bypass shims.** Activating a venv (or similar) prepends its own `bin/` ahead of `~/.schliffe/shims`, so e.g. `pytest` from the venv isn't intercepted.
- **`install.sh`/`install.ps1` always build from source** — there's no prebuilt-binary download yet. A release pipeline (CI + published binaries) is a phase-2 idea (specs §3, original plan).
- **Critical finding already fixed, but worth remembering for any new platform**: shim activation depends entirely on HOW the tool you want to intercept actually invokes a shell (login vs. interactive, etc. — see `specs.md` §5.1 and `MILESTONES.md`, "Critical post-M8 fix" section). Before declaring something "ready" on a new platform, you need to confirm the invocation pattern there experimentally, not assume it generalizes from WSL/Linux.

## Command coverage (Layer B / `bornes/comandos`)

- **Layer B covers 19 filters, still not the whole long tail.** Package installs (`npm`/`pnpm`/`yarn`/`pip`), JS builds and linters run through the package manager (`pnpm build`, `npm run lint`...: Next.js, Vite, ESLint), `git pull`/`merge`/`branch`, `docker` images/pull/build and `compose build|pull`, `dotnet` build/test, cargo's stderr progress, `go test` + download chatter, `terraform plan`. `go`/`terraform` are unit-tested only. Still no rule for: `rg`, `make`, `jq`, `poetry`, `uv`, `nx`, `turbo`, `pre-commit`, among others. Checked and deliberately left without a rule (real output, 2026-09-24): `tsc`, `vitest` and `jest` are already compact when stdout isn't a TTY.
- **Only commands with a filter are captured; everything else streams live** (since 2026-09-24). A filtered command is still buffered until it exits, so filters must never match long-running forms: `pnpm build` is matched, `pnpm dev`/`start`/`test` and `docker compose up` are not. A `build` script that actually runs in watch mode (e.g. `vite build --watch`) would be buffered — use `SCHLIFFE_DISABLE=1` for it.
- **Filters that target stderr reorder the output**: when a filter asks for stderr (e.g. `docker build`, `npm install`), stderr is captured and printed before stdout instead of interleaved as the tool wrote it.
- **Catalog actions (specs §5.3) not implemented yet**: `group_by`, `json_extract`/`json_schema`/`ndjson_stream`, `regex_extract`, `state_machine`, `aggregate`, `format_template`, `compact_path`. None of the 4 v1 filters have needed them yet.
- **No `read`/`smart` parser** (file reading) in Layer A — blocks the "summarize a long docstring/comment" use case that `bornes/prosa` could cover (specs §13, already resolved as "out of v1" for lack of somewhere to plug it in).

## Cache, progressive disclosure, and dedup (`core/store`)

- **Cache (specs §8.2) only covers `git show <explicit sha>`.** Working-tree cache (`git status`/`git diff` with no fixed commit, would need to check `.git/index` mtime) and file-read cache were left out — neither has a Layer A parser to lean on yet.
- **Dedup is session-scoped only when the agent exposes a session id.** Claude Code sets `CLAUDE_CODE_SESSION_ID` (used since 2026-09-24; `SCHLIFFE_SESSION_ID` works for other agents). Without one it falls back to the time window (`SCHLIFFE_DEDUP_WINDOW_SECS`, default 1,800s). Not yet verified: whether Claude Code subagents get their own session id — if they inherit the parent's, a subagent could get a "same as previous output" reference for something only the parent saw (still recoverable via `schliffe show`).
- **Cleanup policy (14 days, ~2% probabilistic sweep) never tested at real scale** — only with the small volume generated during this development session.

## `bornes/mcp`

- **Validated against one real MCP server only** (`@modelcontextprotocol/server-filesystem`, 2026-09-24). Other servers may shape results differently. File-reading tools are detected by name (`read`/`file`/`cat`/`open`/`download`/`blob`), which is a heuristic: a tool returning file content under another name would still get its JSON compacted — add it to `SCHLIFFE_MCP_RAW_TOOLS`.
- **Schema lazy-loading overlaps with Claude Code's own tool deferral.** Use `--keep-schemas` there (see README); lazy-loading is still the default for other clients.
- **Remote MCP proxy support is currently HTTP plus explicit headers only.** Use `schliffe mcp --url https://... --header 'Authorization=Bearer ${TOKEN}'` for Streamable HTTP servers. OAuth browser login, token storage, and renewal are not implemented yet; remote results can still go through the Claude Code `PostToolUse` hook, which only works where Claude Code honors `updatedToolOutput`: macOS, Linux, WSL — **not native Windows**.
- **No field pruning by semantic relevance** (pagination, HATEOAS links, redundant timestamps) — only the 3 purely mechanical techniques (null-strip, string truncation, array cap). Pruning by relevance would require knowing the specific API, which would go against business rule 5.
- **Requests the MCP server itself initiates** (e.g. `sampling/createMessage`) pass straight through with no interception or compression — not the token-waste axis that motivated this borne, but also not addressed.

## `bornes/prosa`

- **The sentence splitter is rule-based**: abbreviations come from a fixed EN+PT list, and a sentence that legitimately ends right before a lowercase word ("...done. then we...") won't be split. Fine for commit bodies and short prose.
- **TF-IDF scores by statistical word rarity, not intuitive "importance"** — validated live that a summary sometimes picks a sentence a human wouldn't have picked first (`schliffe compress`, MILESTONES.md M7). Expected algorithm behavior, not a bug, but worth keeping in mind when interpreting a summary.
- **The user's `/compress` doesn't use `bornes/prosa`** — a deliberate decision (specs §7.2/§7.3, they're different tasks), not a gap to close.

## Quality / process

- **`bytes/4` as the token estimate**, never a real tokenizer — the same approximation RTK/snip use, followed for comparison consistency (specs §5.4.1), not precision. A real tokenizer is a phase-2 idea (specs §11).
- **End-to-end tests are Unix-only** (`tests/e2e.rs`, symlinks + sh scripts) — Windows' copy-based shims aren't exercised by `cargo test`. On macOS they take ~12s, almost all of it the OS scanning each freshly created fake-tool script on first exec; the shim's own overhead in real use is ~20ms.
- **A known and accepted architectural ceiling, not a bug**: static per-command rules are measurably worse than pruning conditioned on the agent's task/intent (arXiv 2604.04979/2604.19572, specs §11) — would require a trained model or intent context passed to the filter, against the project's deterministic philosophy. Recorded, not pursued.
- **Dilution effect** (specs §11): a token reduction in one command's output doesn't equal a reduction in the session's total cost (prompt, history, system prompt also count) — be careful reporting whole-session savings based only on per-command savings.

## Still-open decisions

- None remaining that block current use. The project was renamed from **Elagix** to **Schliffe** on 2026-09-25 (German: *the cuts*, the facets of a cut gem — each filter is one cut). Earlier history in this repo mentions it under the new name.


## `bornes/hook` (Claude Code hook)

- **Not native Windows** — see above.
- **Read's image result shape is undocumented.** The hook finds base64 image data generically; if Claude Code changes the shape, the replacement is discarded by Claude Code's schema check and the original image is used (safe, just no savings). `SCHLIFFE_HOOK_DUMP=<dir>` in the hook command's environment saves each hook input for diagnosis.
- **Only PNG and JPEG are resized** (WebP/GIF left as-is, so the declared media type never changes).
- **Adds ~0.5–1s after reading a very large image** (decode + resize + encode); text reads and non-image tools cost a few milliseconds.
- **Image savings are small at the default 1280px** (measured live 2026-09-25: −11% on a 2400×1500 image; ≈ −7% on a Retina screenshot), because the API already downscales images to ~1.15 megapixels. 1024px gives ≈ −40% but small UI text gets harder to read — tune with `SCHLIFFE_IMAGE_MAX_EDGE`.

## `bornes/prompt` (pasted logs, context notices)

- **Claude Code can't let a hook edit a prompt**, so the compact version goes through the clipboard (`pbcopy`, `wl-copy`, `xclip`, `xsel`, `clip.exe`, in that order; otherwise saved to `~/.schliffe/prompts/last-compact.txt`). It overwrites whatever was on the clipboard; the original prompt is saved to `~/.schliffe/prompts/last-original.txt`.
- **Log detection is rule-based.** What each kind keeps and drops is listed in `docs/pasted-logs.md`. Anything not recognized with confidence is left untouched (apart from ANSI codes and consecutive duplicate lines); blocks under 20 lines / 1.5 KB, or that wouldn't shrink to 60% or less, are never touched.
- **Library vs. app frames are guessed by prefix** (`node_modules`, `java.`, `org.springframework.`, `System.`, `Microsoft.`…). A project whose own namespace starts like a framework would see its frames counted as library frames — still recoverable via `schliffe show`.
- **Context size comes from the session transcript** (the latest reply's usage); the notice appears on the prompt after the threshold was crossed.
- **Not native Windows** (same as the other hook).

## Security

Threat model, fixed vulnerabilities and residual risks live in `SECURITY.md`. The gaps that remain open:

- **`curl … | bash` trusts GitHub.** `SHA256SUMS` proves the download wasn't altered in transit, not that the release itself is honest. Signed releases (sigstore/cosign) aren't implemented; `--from-source` from a clone you've read is the stricter path.
- **A script the model runs through a shell is still filtered** (`bash deploy.sh` → `git diff`): indistinguishable from a command the model ran. `SCHLIFFE_DISABLE=1` inside such a script.
- **Library-vs-app frame detection is a heuristic**, so a project namespaced like a framework can have its own frames collapsed (recoverable, never deleted).
- **No protection against code already running as your user** — Schliffe defends against *other* accounts on the machine.
- **`deny.toml`'s banned-crate list is a denylist**, so it names the capabilities Schliffe must never gain rather than proving the absence of all others; `cargo audit`/`cargo deny` cover the rest.
