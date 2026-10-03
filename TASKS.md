# Schliffe — Task backlog

Every item here comes from a gap already recorded in `KNOWN_ISSUES.md` (which explains the *why* behind each one) — this file is just the actionable version, to pull from when developing. No implied priority/order yet.

## Platform / installation

- [ ] Validate `install.ps1` with full activation on a Windows machine with a genuine native toolchain (`git`/`cargo`/`npm` on the Windows PATH, not just WSL).
- [x] Build and test Schliffe on real macOS — native build + 41 tests on Apple Silicon (2026-09-24); `macos-latest` job added to CI.
- [x] Test `install.sh`'s `zsh` path live — validated on macOS (2026-09-24) with `zsh -lic`/`-lc`/`-ic`; the `.zshrc` line now goes at the END (after nvm), not the top.
- [x] Release pipeline — `.github/workflows/release.yml` builds Linux x86_64, macOS arm64/x86_64 and Windows x86_64 binaries on a `v*` tag and attaches them (+ SHA256SUMS) to a GitHub Release. Not run yet (needs a tag push).
- [x] `install.sh`/`install.ps1` download the prebuilt binary from the latest release, verified against `SHA256SUMS` (M11, 2026-09-27).
- [ ] Repeat the "how does Claude Code actually invoke a shell" check (login/interactive, etc.) on any new platform, before declaring activation ready there — don't assume it generalizes from WSL/Linux.
- [x] Add the `schliffe` binary itself to the PATH — `install.sh` now links `~/.schliffe/shims/schliffe`.

## Command coverage (Layer B / `bornes/comandos`)

- [x] M13 stats-guided audit (2026-10-02): fixed `git status` filter to accept flags (was bare-only, 164 passthrough). Top unfiltered commands all produce < 2.2 KB — no new filters needed.

- [x] First batch of long-tail filters (2026-09-24): npm/pnpm/yarn/pip install, docker pull/build, dotnet, cargo stderr, go — plus stderr support in Layer B (`stream = "stderr"|"both"`).
- [ ] Write TOML filters for the remaining long-tail commands (RTK audit, specs §10): `go-build`, `tsc`, `rg`, `make`, `jq`, `poetry`, `uv`, `mise`, `jj`, `nx`, `turbo`, `pre-commit`, `grep`, `fd`, `tree`, `wc`, `df`, `stat`, `shellcheck`, `yamllint`, `oxlint`, `ruff-format`, `cargo-clippy`, `ls-la`, `golangci-lint`, among others.
- [x] ~~Make Layer B's pipeline order configurable~~ — it already is: steps run in the order the TOML lists them (the "fixed order" note was wrong).
- [x] Catalog actions `compact_path`, `collapse_lines_matching` (aggregate-lite, with recovery hint) and `squeeze_spaces`; `match_any` for rules reached through several invocations (2026-09-24).
- [ ] Remaining catalog actions: `group_by`, `json_extract`/`json_schema`/`ndjson_stream`, `regex_extract`, `state_machine`, `format_template`.
- [ ] Build a Layer A parser for file reading (`read`/`smart`) — unblocks the "summarize a long docstring/comment" use case via `bornes/prosa`.

## Cache, progressive disclosure, and dedup (`core/store`)

- [ ] Extend the cache to working-tree-dependent commands (`git status`/`git diff` with no fixed commit — needs to check `.git/index` mtime).
- [ ] Evaluate a file-read cache (key: path + mtime + size, or a content hash).
- [x] Scope dedup to a real session id — uses `CLAUDE_CODE_SESSION_ID` (or `SCHLIFFE_SESSION_ID`), time window kept as the upper bound (2026-09-24).
- [ ] Validate the cleanup policy (14 days, ~2% sweep per write) at real usage volume, not just the volume generated during development.

## `bornes/mcp`

- [x] Test against a real production MCP server — `@modelcontextprotocol/server-filesystem` (2026-09-24): `tools/list` −77%, `get_tool_schema` round trip OK. Found and fixed: JSON *file contents* were being compressed (corruption risk) — file-reading tools are now never touched.
- [x] Document wrapping an MCP server (README, "Using the MCP proxy"). Automatic rewriting of client configs left out on purpose — editing `~/.claude.json` behind the user's back is riskier than one `claude mcp add` line.
- [x] Remote MCP servers (HTTP/OAuth, e.g. Figma) — covered by the Claude Code `PostToolUse` hook (`bornes/hook`, 2026-09-25) instead of a proxy: Claude Code keeps doing OAuth, Schliffe rewrites the result. macOS/Linux/WSL only.
- [x] Images (MCP screenshots, Read on PNG/JPEG) shrunk to a 1280px long edge by the same hook.
- [x] Figma `get_design_context` trimming (M12, 2026-10-02): strip `data-name` attributes (−5%) and resolve CSS `var()` to fallbacks (−7%) = −13% per call. Verified lossless on a real FoundationOne frame (97k chars). All `data-node-id`, `className` and asset URLs preserved.
- [x] Complete Streamable HTTP lifecycle (M16, 2026-10-02): proper SSE event framing, standalone GET stream for server-initiated messages with auto-reconnect (`Last-Event-ID`), `MCP-Protocol-Version` header, clean shutdown on client EOF. E2e fixture in `tests/mcp_http.rs`.
- [ ] Confirm the exact shape of Claude Code's Read result for images on a live session (undocumented; the hook detects base64 image data generically — check `schliffe stats` shows `image (Read)` after reading a large screenshot).
- [ ] Field pruning by semantic relevance (pagination, HATEOAS links, redundant timestamps) — today only the 3 mechanical techniques (null-strip, truncation, array cap).
- [ ] Handle/compress requests initiated by the MCP server itself (e.g. `sampling/createMessage`) — pass straight through today.
- [x] Server dying mid-call — pending requests now get a JSON-RPC error (validated live, 0.02s) instead of hanging forever.

## `bornes/prosa`

- [x] Sentence splitter handles abbreviations (EN+PT), initials, lowercase continuations, and treats bullets/blank lines as boundaries (2026-09-24).
- [ ] Evaluate an importance heuristic beyond pure TF-IDF (or formally accept the current limitation — it picks by word rarity, not intuitive "importance").
- [ ] Consider a real tokenizer instead of the `bytes/4` estimate.

## Quality / process

- [x] Security audit with specialized tooling and fixes for every finding (M14, 2026-09-28) — see `SECURITY.md`.
- [ ] Signed releases (sigstore/cosign) so the installer can verify provenance, not just integrity.

- [x] Benchmark against RTK on real commands and close the gaps that don't cost information: `git pull` one-liner, `cargo test` totals (fixing a false-success bug), `git branch -a` grouping, `docker images`/`ps` column compaction (2026-09-26).

- [x] `schliffe stats` — savings report (24h/7d/all time, top savers, unfiltered commands), fed by a size-only log from the shim and the MCP proxy (2026-09-25).

- [x] End-to-end tests against the compiled binary — `tests/e2e.rs` (12 tests: agent gating, exit codes, 127, self-recursion, `schliffe show`, session dedup, stderr filters, rule 6, `compress`).
- [x] Determinism test — `tests/e2e.rs::output_is_deterministic` (same input, fresh store, byte-identical output).
- [x] Set up CI (cross-platform build + test) — Linux, Windows cross-compile, macOS.
