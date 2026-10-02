# Schliffe — Milestones

Each milestone references the `specs.md` section that already settles the technique — this file is just delivery sequencing, not a new decision.

- [x] **M0 — Scaffold**: Rust project at `~/projects/schliffe` (repo root, outside `bench/`), clean `cargo build`. *(done, 2026-07-26)*
- [x] **M1 — Interception mechanism**: PATH shim working — TTY detection, real-binary resolution, passthrough vs. capture. (specs §5.1) *(done, 2026-07-26)*
- [x] **M2 — Layer A complete**: `git status`, `git log`, `git diff`/`git show`, `pytest`, `cargo test` — the 5 commands validated in the audit (specs §10). All tested live against the real RTK (see "Validated" section below). (specs §5.4) *(done, 2026-07-26)*
- [x] **M3 — Layer B (declarative pipeline)**: generic rule engine for the long tail, TOML format (decided in specs §13). (specs §5.2/5.3) *(done, 2026-07-26)*
- [x] **M4 — Reversibility, cache, and dedup**: content-addressed store, progressive disclosure, cache with per-type invalidation, deduplication across calls. Disk cleanup policy decided before turning it on. (specs §8) *(done, 2026-07-26 — v1 scope: cache only for explicit `git show <sha>`, see detail below)*
- [x] **M5 — `bornes/mcp`**: JSON-RPC proxy with schema lazy-loading. (specs §6.1) *(done, 2026-07-26)*
- [x] **M6 — `bornes/mcp` result**: MCP tool call result compression, reusing the JSON techniques from §5.5. (specs §6.2) *(done, 2026-07-26, implemented together with M5 — both share the same proxy)*
- [x] **M7 — `bornes/prosa`**: extractive TF-IDF, integrated into the commit body (M2). (specs §7) *(done, 2026-07-26 — scope revised during implementation: does NOT integrate with `/compress`, see detail below)*
- [x] **M8 — Packaging**: cross-platform installer, binaries for Windows/Linux/Mac. *(done, 2026-07-26 — Windows/Linux complete and genuinely activated; Mac cross-compile explicitly deferred, see detail below)*

## Validated live this session (2026-07-26)

Tested against the real `bastion-agent` repository, via the shim installed at `~/.schliffe/shims/` (`git`/`pytest` symlinks pointing at the release binary):

| Command | Raw | Schliffe | Reduction | Note |
|---|---|---|---|---|
| `git status` (clean branch) | 174B | 28B | 84% | Identical to RTK |
| `git log -5` | 9,313B | 204B | 97.8% | Same technique as RTK (keeps 1st commit, cuts the rest) |
| `git show HEAD` ("cargo fmt" diff, 6 files) | 32,001B | 5,740B | 82.1% | **Better than RTK** (71.43% on the same diff, section 9) |
| `pytest` (import error) | 3,245B | 106B | 96.7% | **Better than RTK**: preserves `ModuleNotFoundError: ...` — RTK only says "No tests collected", no reason given |
| `cargo test --lib control_plane` | — | — | — | Clean summary extracted ("60 passed; 0 failed... 426 filtered out"), no verbose compile log |

Exit codes preserved in every case (`pytest` correctly exits with 2, not 0).

### Layer B (M3, validated live 2026-07-26)

Generic engine in `src/camada_b/` — `FilterFile` (TOML) + `Step` (a tagged `action` enum) + `engine::apply()`. v1 actions: `strip_ansi`, `replace`, `match_output`, `keep_lines_matching`/`strip_lines_matching`, `dedup`, `truncate_lines`, `max_lines`, `on_empty` (the rest of the §5.3 catalog — `group_by`, `json_extract`/`json_schema`, `state_machine`, `aggregate`, `format_template`, `compact_path` — are left for whenever a v1 command actually needs them). `match_output`/`on_empty` only fire with `exit_code == 0`, the same business rule 2/3 as Layer A. 4 example filters embedded via `include_str!` (`filters-toml/*.toml`): `docker-images`, `git-branch`, `terraform-plan`, `npm-install`. Extensible without recompiling via `$SCHLIFFE_FILTERS_DIR` (default `~/.schliffe/filters/*.toml`).

| Command | Raw | Schliffe | Reduction | Note |
|---|---|---|---|---|
| `git branch -a` (bastion-agent) | 353B | 316B | 10.5% | Only removed `remotes/origin/HEAD -> origin/main`; the list didn't exceed the 25-line cap |
| `docker images` (21 real images) | 1,782B | 1,234B | 30.8% | `max_lines(15)` cut 7 images; no `<none>:<none>` present for `strip_lines_matching` to act on |
| `terraform plan` (local fixture, 1 resource) | 1,492B | 1,290B | 13.5% | Deliberately modest: no real state refresh or "no change" resources in this small fixture — confirms the architectural ceiling already documented (specs §10.3, failure type #3): a specific-noise rule only helps when that noise actually shows up |

6 new unit tests in `src/camada_b/engine.rs` (project total: 20 tests, all passing).

**Four real bugs found and fixed while testing live** (documented as code comments, with the reason for each fix kept alongside):
1. **Shim recursion** (`src/shim.rs`): the initial version tried to discover its own folder from `argv[0]`, assuming the shell always passes the full path — bash sometimes passes just the bare name ("git"), causing the shim to find itself again and filter its own output twice. Fixed: the shims folder is now known ahead of time (`~/.schliffe/shims` or `$SCHLIFFE_SHIMS_DIR`), not discovered.
2. **Filter turned off in the highest-value case** (`src/main.rs`): the rule "no success shortcut if the process failed" had, by mistake, become "only filter if `exit_code == 0`" — that turned off pytest's filter in exactly the case we most wanted to show off (test collection failure, which exits with a non-zero code). Fixed: each filter is responsible for never fabricating success; filtering itself always runs.
3. **`git diff` with no commit header** (`src/filters/git_diff.rs`): `git diff` (working tree) starts directly with "diff --git", without the commit block `git show` has before it — the original detection only looked for `"\ndiff --git"` (with a newline before it), failing on this real case and falling back to fail-open with no filtering at all.
4. **A cut in the middle of a diff made the output look like broken code** (`src/filters/git_diff.rs`): once the per-file changed-line cap was hit, the initial version kept showing `@@` headers and context from later hunks, only hiding the `+`/`-` lines — the result looked like broken syntax (an incomplete function call, an unclosed brace). Fixed to stop for good on the first excess, with a clear count of what was omitted.

### M4 — store, cache, progressive disclosure, dedup (validated live 2026-07-26)

Three pending decisions resolved before implementing (specs §13): the disk store (`~/.schliffe/store/`, file-per-hash layout, no in-RAM index), cleanup by 14-day expiration with a lazy sweep (~2% chance per write, plus manual `schliffe store clear`/`schliffe store gc`), and v1 cache scope restricted to `git show <explicit sha>` (the only case provably immutable without a heuristic — `HEAD`/branch are excluded).

New module `src/store/` — CAS (`put`/`get`, sha256 truncated to 64 bits) + keyed cache (`put_keyed`/`get_keyed`, a versioned key `git-show:v1:<sha>` so it never serves stale output if the filter changes) + sliding-window dedup (`check_and_record_dedup`, a limitation documented in specs §8.3: approximates "session" by time, not by a real id) + `force_gc`/`clear_all`. New meta-commands: `schliffe show <hash>`, `schliffe store clear`, `schliffe store gc` — handled in `main.rs` before any shim resolution (there's no "real schliffe" on the PATH).

| Mechanism | Live test | Result |
|---|---|---|
| Cache (`git show <sha>`, bastion-agent) | 1st call runs for real, 2nd call hits the cache | 0.023s → 0.001s (~23×), byte-identical output (confirmed with `diff`) |
| Progressive disclosure (`git show`, the same 32,001B diff from M2's validation) | Filtered output (5,786B) gained the hint `(full output: schliffe show c8a886791a32083d)` | `schliffe show c8a886791a32083d` recovered the exact original 32,001B |
| Dedup (repeated `git branch -a`, 60s window) | 1st call: normal 316B. Identical 2nd call: collapsed | 316B → 75B (`(same as previous output — schliffe show ... to view it again)`) |

Business rule 6 (never inflate) and rule 2/3 (no success shortcut on an unsuccessful process) verified in code: cache only writes with `exit_code == 0`; dedup only substitutes if the reference message is shorter than the original output. 5 new unit tests in `src/store/mod.rs` (project total: 25 tests, all passing).

**Scope explicitly left for later** (not a gap, a phasing decision): working-tree cache (`git status`/`git diff` with no fixed commit, needs to check `.git/index` mtime) and file-read cache (there's no `read`/`smart` parser yet) — specs §8.2 already lists both as the next candidates once there's a concrete reason to prioritize them.

### M5/M6 — `bornes/mcp`: JSON-RPC proxy (validated live 2026-07-26)

Unlike `bornes/comandos`'s shim (a short, one-shot process), this is a **long-lived** process: `schliffe mcp -- <real server command> [args...]` spawns the real MCP server as a child and stays in the middle of the whole conversation, over stdio (newline-delimited JSON-RPC, no LSP-style framing). New module `src/mcp_proxy/` — `mod.rs` (spawn + two threads: one forwards client→server, intercepting `tools/call get_tool_schema` and pending `tools/list`/`tools/call`; the main one reads server→client and applies the right transformation per request `id`), `schema.rs` (lazy-loading, specs §6.1), and `compress.rs` (result compression, specs §6.2).

**Schema lazy-loading**: `tools/list` returns minimal wrappers (name + first sentence of the description + a generic `inputSchema` of `{"type":"object"}`) and injects a synthetic `get_tool_schema` tool. The original full schema is cached in memory (per process, lasts the MCP session); when the model calls `get_tool_schema("X")`, the proxy answers **locally**, never forwarding that call to the real server (which doesn't even know about this tool).

**Result compression**: only the 3 purely mechanical techniques from specs §5.5 (none of them tries to guess a field's semantic relevance): recursively removes `null`, truncates a long string keeping a prefix + a count, caps a large array at 10 items + an `_schliffe_omitted_items` marker. Field pruning by relevance (pagination, HATEOAS) is left out of v1 — it would require knowing the specific API.

Tested live with our own test MCP server (`fake_mcp_server.py`, a `search_docs` tool with a whole-paragraph description and a result with nulls/a 30-item array/long strings — a controlled fixture, not a third-party server):

| Message | Raw | Schliffe | Reduction |
|---|---|---|---|
| `tools/list` (2 verbose tools) | 1,047B | 566B | 45.9% |
| `get_tool_schema("search_docs")` | — | 962B | recovers the **complete and exact** schema — round trip validated |
| `tools/call("search_docs")` | 16,658B | 4,530B | 72.8% |

**Real bug found and fixed testing live**: a pipe deadlock on shutdown — the `Arc<Mutex<ChildStdin>>` had an extra copy stuck in `run()`'s scope besides the forwarding thread's copy; the child process's stdin only truly closes once the LAST copy of the `Arc` is dropped, so the real server (reading stdin until EOF) never got that EOF, never exited on its own, and `child.wait()` hung forever. Fixed with an explicit `drop()` of the main scope's copy right after spawning the thread.

8 new unit tests (`schema.rs` + `compress.rs`, project total: 32 tests, all passing). Business rule 6 verified at three points: `transform_tools_list` and `compress_tools_call_result` compare the whole JSON-RPC message's size before/after and fall back to the original if the transformation doesn't pay off (the `tiny_single_tool_falls_back_to_original` test proves this: a single tiny tool doesn't amortize the fixed cost of injecting `get_tool_schema`).

**Scope left for later**: OAuth and remote HTTP streaming (specs §13 already lists this as out of v1); field pruning by semantic relevance; requests the server ITSELF initiates (e.g. `sampling/createMessage`) pass straight through with no interception, since that isn't the token-waste axis that motivated this borne.

### M7 — `bornes/prosa` (validated live 2026-07-26)

New module `src/prosa/` — `summarize(text, max_sentences)`: classic TF-IDF (tf normalized per sentence, idf smoothed with `ln(N/df)+1`, EN+PT stopwords since this project's own commits mix both languages), extracts the highest-scoring sentences while preserving their ORIGINAL order (not score order — a summary out of chronological order would confuse more than it would help). Fail-open: text already within the sentence cap comes back unmodified.

Integrated into the two places that used to drop a commit's body entirely:
- `filters/git_log.rs`: the first commit's body now shows up as `summary: <sentence>` (when it actually shrank) or `body: <sentence>` (when it was already a single sentence, shown in full instead of labeled as if it had been compressed — business rule 5).
- `filters/git_diff.rs` (`git show`): a real finding during implementation — the filter used to drop even the commit's **hash and subject** along with the body, not just the body. Nobody had noticed (the filtered `git show` output never said which commit that diff belonged to). Fixed: now keeps `commit <hash> — <subject>` + a body summary before the hunks.

**Decision revised during implementation** (specs §7.2/§7.3, §13): `/compress` **does not** turn into a call to `bornes/prosa`, contrary to what the earlier spec assumed. Re-examining the real `~/.claude/commands/compress.md` while integrating made it clear these are different tasks — `/compress` needs to cut redundancy WITHIN each sentence while preserving numbers/names/constraints (semantic judgment), while extractive TF-IDF can only drop WHOLE sentences (risking the loss of a constraint that landed in a low-scoring sentence — going against business rule 5). `bornes/prosa` gained an equivalent standalone utility, `schliffe compress` (reads stdin, summarizes, prints — an automatic sentence cap of ~1/3 of the original, or an explicit `--sentences N`), useful for prose that can tolerate that kind of loss, but it isn't the engine behind the user's slash command.

Tested live against the real `bastion-agent` repository (the same 2-sentence "cargo fmt" commit used in M2/M4's validation):

| Command | Before M7 | After M7 |
|---|---|---|
| `git log -5` | commit body dropped entirely, silently | `summary: Never ran cargo fmt this session...` (the highest-signal sentence per TF-IDF, not the first one by default) |
| `git show HEAD` | commit hash/subject/body all dropped (a loss undocumented until now) | `commit cb93a3bd... — chore: cargo fmt (fix CI fmt-check failure)` + `summary: ...` before the hunks |
| `schliffe compress` (utility, 3 test sentences) | N/A | picked the 3rd sentence, not the 1st — TF-IDF scores by word rarity, not intuitive "importance"; expected algorithm behavior, documented as a known limitation, not a bug |

6 new unit tests in `src/prosa/mod.rs` + 2 updated in `git_log.rs`/`git_diff.rs` to reflect the new behavior (project total: 38 tests, all passing).

### M8 — Packaging (validated live 2026-07-26 — **Schliffe genuinely activated this session**)

v1 scope: build from source (`cargo build --release`), no prebuilt-binary download — there's no release/CDN pipeline yet, and it wouldn't make sense to pretend there is. Two installers at the repo root:

- **`install.sh`** (Linux/Mac/WSL): builds with `cargo`, creates symlinks at `~/.schliffe/shims/{git,cargo,pytest,docker,npm,terraform}` (the list covers everything that already has a filter, Layer A + Layer B), makes sure `~/.schliffe/shims` is ahead in `$PATH` via `~/.bashrc`/`~/.zshrc` (idempotent — doesn't duplicate the line on a re-run).
- **`install.ps1`** (native Windows): deliberately doesn't symlink (would need developer mode/admin) — copies the `.exe` to each command name inside `~\.schliffe\shims\`, since Schliffe decides what to filter by the file's NAME (`argv[0]`), not whether it's a link or a copy. Looks for an already-built `schliffe.exe` (3 candidate paths) before trying to build it on the spot; adjusts the user's PATH via `[Environment]::SetEnvironmentVariable(...,"User")`, also idempotent.

**Windows — cross-compiled and tested actually RUNNING on native PowerShell** (not just `file`/static inspection): installed `mingw-w64` + the `x86_64-pc-windows-gnu` target in the build environment (WSL); `cargo build --release --target x86_64-pc-windows-gnu` compiled cleanly (no project dependency uses C/FFI, only pure Rust crates — `regex`/`serde`/`serde_json`/`toml`/`sha2` — which is why mingw was enough, no need for `cargo-zigbuild` or anything more elaborate). Copied the `.exe` to the Windows side and ran it natively via PowerShell: `schliffe compress`, `schliffe show`, `schliffe store gc` ran and exited with the right code.

**Real bug found and fixed in this test**: `"text" | schliffe.exe compress` arrived with a UTF-8 BOM (`U+FEFF`) at the front of the text — a known artifact of how native PowerShell encodes a string literal when sending it to a process's stdin, not an Schliffe logic bug. Fixed with `strip_prefix('\u{feff}')` in `run_compress` (specs §7, `main.rs`) — stripping a BOM never loses substantive content, so it didn't violate business rule 5.

**`install.ps1` only tested in safe mode (isolated copy), not with full activation**: this development machine has no native `git`/`cargo`/`npm` on the Windows PATH — all development happens via WSL (confirmed with `Get-Command`, none of them resolved). Running the full `install.ps1` here would have had no practical effect (there's no real native tool to intercept). The script is correct and tested as far as this machine allows; full activation validation (a real PATH + intercepting an actual native `git.exe`/`npm.exe`) remains pending until run on a Windows machine with a native toolchain installed.

**Linux/WSL — genuinely activated, not just tested**: `install.sh` run with no sandbox, `~/.bashrc` actually modified. A methodology finding (not a product bug): validating this via `wsl -e bash -lc "..."` gives a false negative — it's a NON-interactive login shell, and the "only run the rest if interactive" guard at the top of Ubuntu's default `.bashrc` blocks reading the part we appended. Tested correctly via `bash -ic` (interactive, what a real terminal tab uses):

```
$ git status | cat
clean — nothing to commit
```

Confirmed: `which git`/`which cargo`/`which pytest` resolve to `~/.schliffe/shims/`, and `git --version | cat` (a command with no filter defined) passes straight through unmodified — only subcommands with a real filter are touched.

**macOS — cross-compile deferred, with concrete evidence of why** (specs §9 already flagged this as Rust's known friction point): `rustup target add aarch64-apple-darwin` works, but linking fails —

```
warning: invoking "xcrun" "--sdk" "macosx" "--show-sdk-path" ... failed: No such file or directory
error: linking with `cc` failed
cc: error: unrecognized command-line option '-arch'
cc: error: unrecognized command-line option '-mmacosx-version-min=11.0.0'
```

Needs a real SDK/Xcode (or a tool like `cargo-zigbuild`/`osxcross`, neither configured) — not something to solve for free in a plain Linux environment. Deferred until there's access to a real Mac or a macOS CI runner (e.g. GitHub Actions `macos-latest`), neither configured yet (there was no remote repository/CI for this project at the time — only local, no commits yet).

### Critical post-M8 fix: yesterday's activation didn't apply to how Claude Code actually runs commands (2026-07-26)

The user asked, rightly, "but the goal is to save tokens with the AIs, right? why 'any new WSL terminal'?" — that exposed that the first activation (a line only in `~/.bashrc`) **had zero effect** on the real way I (Claude Code, running as a VSCode extension on Windows) invoke commands: always via `wsl -e bash -lc "..."` (a **login, non-interactive** shell). Confirmed live: `which git` showed the real binary, `$-` showed `hBc` (no `i`).

Root cause: Ubuntu's default `~/.bashrc` has a guard at the top (`if not interactive, exit`) that discards any line appended at the end of the file when the shell isn't interactive — exactly the `-lc` case. bash uses different files depending on the login/interactive combination, and no single file covers the two combinations that matter:
- login (interactive or not, includes `-lc`, the real case) → `~/.profile`
- non-login but interactive (e.g. `bash -ic`) → `~/.bashrc`

Two intermediate attempts discarded with evidence, not just theory:
1. **`/etc/environment`** (PAM level, should apply to everything) — edited with sudo, but confirmed that `wsl -e bash -lc` (`-e`/direct-exec mode) **doesn't even consult this file**: the observed `$PATH` had no trace of the value in it, even after a full `wsl --shutdown` to force a re-read. Reverted.
2. Simply moving the line to `.profile` alone — fixed the real case (`-lc`) but **broke** the `bash -ic` case (non-login+interactive, which never reads `.profile` — that's bash's own rule).

**Final fix, validated across the three combinations that genuinely exist** (`wsl -e bash -lc`, `wsl -e bash -ic`, and confirmed that only the theoretical `bash -c` with neither `-l` nor `-i` is left uncovered, which no dotfile handles by bash's own design — not the observed real invocation pattern, so not pursued):
- `~/.profile`: a plain line at the end (the convention, no guard).
- `~/.bashrc`: a line at the **top of the file**, before the interactivity guard — not at the end.

`install.sh` rewritten to do both from the very first install (with the same logic mirrored for zsh via `~/.zprofile`/`~/.zshrc`, not tested live this session for lack of an available zsh shell, but the same bash rule applies).

```
$ wsl -e bash -lc 'git status | cat'
clean — nothing to commit
$ wsl -e bash -ic 'git status | cat'
clean — nothing to commit
```

**The bigger lesson, beyond the bug itself**: "activating the shim" isn't a binary checkbox — it depends entirely on HOW the tool we want to intercept (here, Claude Code itself) actually invokes a shell. Worth checking this explicitly on any new platform before declaring M8 ready there too.

### Folder restructuring: `core/` + `bornes/{comandos,mcp,prosa}/` (2026-07-26)

`src/` finally reflects the architecture diagram `specs.md` §3 had described since the start of the project (self-contained modules in a `bornes/` folder) — up to this point the real code was flatter (`src/filters/`, `src/camada_b/`, `src/mcp_proxy/`, `src/prosa/`, `src/store/`, `src/shim.rs` all sitting loose directly under `src/`).

```
src/
  main.rs              — thin entry point: only decides meta-command (core::meta) vs shim (bornes::comandos)
  core/
    store.rs            — content-addressed store (specs §8), cross-cutting: today only bornes/comandos uses it, but it's infra for all 3
    meta.rs              — routes `schliffe show/store/compress/mcp`
  bornes/
    comandos/            — the "RTK-like" one: $PATH shim + Layer A + Layer B
      shim.rs
      filters/           — Layer A (git_status, git_log, git_diff, pytest, cargo_test)
      camada_b/          — declarative pipeline engine (engine.rs) + types (mod.rs)
      filters-toml/       — Layer B rule data (moved from filters-toml/ at the root)
      mod.rs              — orchestration (used to be the big body of the old main.rs)
    mcp/                  — MCP compressor (used to be mcp_proxy/)
    prosa/                — TF-IDF summarization
```

Only 5 files needed a real content change (everything else was moved untouched): `main.rs` (rewritten, now thin), `camada_b/mod.rs` (`include_str!` from `../../filters-toml/` to `../filters-toml/`, since the data moved along with it), `camada_b/engine.rs` (the `Step` path in the test), `filters/git_log.rs` and `filters/git_diff.rs` (the `bornes::prosa::summarize` path). `cargo build --release` was clean on the first try, 38 tests passing with no assertion changes, and validated live again against `bastion-agent` — the same behavior as before the restructuring (`git status`/`git log` filtered the same way, cache/disclosure/dedup intact).

## Next steps
- **All 8 planned milestones (M0-M8) are done, activation has been validated against Claude Code's real invocation pattern, and the folder structure reflects the architecture documented from the start.**
- See `KNOWN_ISSUES.md` (new, 2026-07-26) for the full, consolidated list of technical debt and known improvements — previously scattered across loose notes in this file and in `specs.md` §13.
- Real, non-blocking pending items: (1) validate `install.ps1` on a Windows machine with a genuine native toolchain; (2) real build/test on macOS (needs a physical Mac or CI); (3) the project's own repository now has commits, published under `mkmuniz/schliffe-tk`, after the M8 rename to Schliffe; (4) watch real usage for a few days and see if any filter needs adjusting based on real production data, not just fixtures; (5) `install.sh`'s zsh path hasn't been tested live (only bash, this machine's real shell).
- Test the shim under real use (add `~/.schliffe/shims` to the persistent `$PATH`, not just per-call) and watch it for a few days before moving forward
- `bornes/mcp` also needs a live test against a real MCP server (not just the test fake) before M5/M6 can be considered ready for real use — only the mechanism has been validated, not compatibility with production servers

## Roadmap — M9 onwards (planned 2026-09-27)

Order chosen by impact: ship what's already built, then attack the biggest cost (long conversations), then adoption, then the remaining sources of tokens.

### M9 — Release v0.4.0 ✅ (done 2026-09-27)

- **Goal:** publish what's merged since v0.3.0 — the `cargo test` false-success fix, the prompt hook (pasted logs + long-conversation notice), shell-only agent detection, the RTK-gap filters.
- **Requirements:**
  - `Cargo.toml` → `0.4.0`; `CHANGELOG.md` `[Unreleased]` → `[0.4.0] — <date>`.
  - Merged to `main` through a PR (branch protection: PR + 3 required checks).
  - Tag `v0.4.0` on `main`; the release workflow publishes the 4 binaries + `SHA256SUMS`.
- **Done when:** the release page lists the 4 archives, CI and Release workflows are green, and `schliffe --version` on the dev machine prints `0.4.0` after `install.sh`.
- **Depends on:** the open PR being merged (human action: GitHub merge).

### M10 — `schliffe report`: where the tokens go ✅ (done 2026-09-27)

Validated against an independent calculation on the dev machine's real transcripts: identical session/reply counts and cost split (15 sessions, 3,319 replies, 71% re-reads / 18% new content / 11% output), in ~0.6 s. Findings while validating: one reply spans several transcript lines (deduplicated by message id); thinking is billed but not re-read (shown separately); the transcript folder name can't be decoded into the project folder reliably (the session's most frequent `cwd` is used instead); the manual analysis had over-counted Figma by measuring embedded screenshots by base64 length.


- **Goal:** turn the manual analysis done during development into a command, so the user can see which sessions and which kinds of content drive the bill — and change habits where it matters.
- **Requirements:**
  - Reads Claude Code transcripts (`~/.claude/projects/*/*.jsonl`) read-only; never sends anything anywhere.
  - Per period (24h / 7d / all): total cost-equivalent tokens, split into cache re-reads, new input, output (weighted by relative price).
  - Per session: responses, peak context size, share of the total — longest sessions first.
  - Per content source: conversation, file reads (Read), MCP (by server/tool), shell commands, images — images counted by billed pixels, not base64 length.
  - Schliffe's own savings shown as a share of the total (so the "~1%" is visible, not hidden).
  - Fast on large transcripts (streamed, no full-file loads); fail-open on unknown line formats.
- **Development requirements:** unit tests on a synthetic transcript; an e2e test running the binary on a fixture directory; README section; no new dependencies beyond what's there.
- **Done when:** on the dev machine it reproduces the manual numbers (top sessions, ~75% cache re-reads, Figma share) within rounding.

### M11 — Installer downloads the prebuilt binary ✅ (done 2026-09-27)

Validated on the dev machine (macOS arm64, isolated `$HOME`): one-command piped install with no Rust in `PATH` in ~2.3 s; a tampered archive (wrong SHA-256) refused with nothing installed; `--from-source`, `--prebuilt` and "clone without Rust" paths. `install.ps1` can't run here — the new `Installers` workflow runs it on a real Windows runner (first real Windows run of the installer), plus `install.sh` on Linux/macOS runners.


- **Goal:** install in seconds without Rust.
- **Requirements:**
  - `install.sh` detects OS/arch, downloads the matching archive from the latest GitHub release, verifies it against `SHA256SUMS`, installs to `~/.schliffe/bin`.
  - Falls back to building from source when there's no matching binary, no network, or the checksum fails (never installs an unverified binary).
  - `--from-source` flag to force the build; same migration/shims/hook steps as today.
  - `install.ps1`: same download for Windows (hook still not installed there).
- **Development requirements:** shellcheck-clean script; tested live on macOS arm64 (download path) and with `--from-source`; README install section updated.
- **Done when:** a fresh macOS user without Rust installs with one command in under 30 seconds.
- **Depends on:** M9 (a release with binaries).

### M12 — Figma `get_design_context` trimming ✅ (done 2026-10-02)

- **Goal:** cut noise from the largest single MCP payload seen in real use (~974k tokens in a week, ~23k per call) without changing the design information.
- **Measured on a real FoundationOne frame** (97,459 chars / ~24k tokens):

| Category | Size | % of payload |
|---|---|---|
| className attributes (Tailwind) | 50,974 B | 52% — **kept** |
| Arbitrary class values (var refs) | 23,342 B | 24% — var() resolved to fallback |
| data-node-id | 8,441 B | 8% — **kept** |
| data-name (layer names) | 5,792 B | 5% — **removed** |
| Asset URLs | 2,666 B | 2% — **kept** |
| Instruction blocks | 2,243 B | 2% — kept |

- **Two lossless-for-implementation cuts** applied in the PostToolUse hook, only for `mcp__figma__get_design_context`:
  1. **Strip `data-name` attributes** (−5%): Figma layer names ("image", "p.MuiTypography-root") carry no implementation information.
  2. **Resolve `var()` to fallback** (−7%): `font-[family-name:var(--font-family/font-2,'Manrope:Bold')]` → `font-[family-name:'Manrope:Bold']`. The agent converts to the target design system; only the concrete value matters.
- **Result: −13% per call** (97,459 → ~84,000 chars). Above the 10% threshold.
- **Invariants verified on the real fixture:** every `data-node-id`, every `className`, every asset URL preserved; className count unchanged.
- **Implementation:** `src/bornes/hook/figma.rs` — two regexes (`LazyLock`), applied before the generic MCP compactor. 14 tests (9 unit + 3 real-fixture + 2 integration).

### M13 — New filters guided by clean `stats`

- **Goal:** cover the commands that actually produce large output for the model, now that `stats` only counts the model's commands.
- **Requirements:** after a few days of normal use, rank "passed through with no filter" by output size (not only count); write a filter only where output is large and mostly noise; each filter with a real captured fixture and the usual rules (no inflation, fail-open, errors kept).
- **Done when:** the top 3 large unfiltered commands (if any) have filters, or it's documented that none are worth one.
- **Depends on:** a few days of real usage data.

### M14 — Security hardening ✅ (done 2026-09-28)

A full pass over the attack surface, driven by the fact that Schliffe runs in front of every command an AI agent executes and stores what they print. Each finding below was **verified exploitable on a real machine first**, then fixed, then locked in by a regression test the new `Security` workflow runs on every PR and daily.

Found and fixed: arbitrary file read through `schliffe show` (path traversal, reachable via prompt injection because Schliffe's own recovery hints are text the model reads); code execution through a relative `$PATH` entry (a planted `./git` in an untrusted checkout ran); secrets written world-readable (the store holds raw command output); a symlink at a store destination being written through; a panic on a multi-byte argument; unbounded reads from hook stdin and from a hostile MCP server; and an image decompression bomb that was only refused by a dependency's default.

Tooling added: `cargo audit` (0 advisories over 60 dependencies), `cargo deny` with a policy that bans unneeded network clients, scripting and dynamic-loading crates, while explicitly allowing the Rustls-based HTTP client required by remote MCP; licence and registry allow-lists; `gitleaks`; and a check that every `unsafe` carries a `SAFETY:` comment (two sites, both reviewed). `SECURITY.md` documents the threat model, each fix, and the residual risks.

### M15 — OAuth for remote MCP

- **Goal:** let a user connect to protected Streamable HTTP MCP servers without manually copying bearer tokens into command arguments.
- **Requirements:** follow MCP OAuth discovery through Protected Resource Metadata and Authorization Server Metadata; use Authorization Code + PKCE S256; prefer Client ID Metadata Documents and retain Dynamic Client Registration only for compatible legacy servers; validate issuer, redirect URI, state and PKCE before accepting a callback; retry a request once after a successful authorization; support refresh-token renewal without printing tokens.
- **Security requirements:** store tokens only under `~/.schliffe` with owner-only permissions and atomic writes; bind cached credentials to the exact server and issuer; never put tokens in URLs, logs, stats or recovery content; cap metadata and token responses; accept only HTTPS for remote OAuth endpoints except loopback development; fail closed on an invalid callback and fail open to the original HTTP error when authorization cannot complete.
- **Tests:** local OAuth fixture covering discovery, PKCE mismatch, issuer mismatch, expired access token, refresh, token redaction and a successful retry. No real account or credential belongs in the repository.
- **Done when:** a protected test MCP server can be connected through `schliffe mcp --oauth --url ...`, the token survives a restart securely, and `cargo deny`, `cargo audit`, `cargo clippy` and `cargo test` pass.

### M16 — Complete Streamable HTTP lifecycle

- **Goal:** support server-initiated messages and long-lived event streams without bypassing Schliffe's filtering or security limits.
- **Requirements:** preserve request/response correlation, validate the MCP method headers when present, bound each event and the number of pending requests, forward client answers unchanged, and apply transformations only to known `tools/list` and `tools/call` responses.
- **Done when:** a local HTTP fixture exercises notifications, server requests, SSE events, reconnect behavior and shutdown without hangs or semantic changes.
