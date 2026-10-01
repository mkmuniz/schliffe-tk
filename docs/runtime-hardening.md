# Runtime hardening and efficiency

Implemented in the 2026-09-28 working-tree change: bounded command capture and MCP framing/lifecycle fixes. Filters, token-reduction rules, and ordinary recovery hints are unchanged.

## Behavior

- Command stdout and captured stderr each buffer up to 64 MiB. Above that threshold, the entire affected stream is forwarded byte-for-byte, including its buffered prefix. The command runs once, its exit code is preserved, and neither an extra newline nor a recovery hint is appended to that stream. Oversized streams bypass compression, caching, and deduplication.
- MCP messages are limited to 64 MiB each in both directions, without a message queue between readers and processing. There is no cumulative session byte limit. Oversized or invalid-UTF-8 frames terminate the connection rather than forwarding partial JSON.
- Pending MCP requests receive an error when the server disconnects. A child that closes stdout but refuses to exit gets a one-second shutdown grace period before termination. This is not an execution timeout: valid active requests can take as long as needed, including after client stdin closes.
- At most 1,024 client requests may be pending. Additional requests and duplicate pending IDs receive errors without being forwarded.

The tradeoff is explicit: unusually large command output saves no tokens on the streamed channel; an MCP message exceeding the per-message limit is rejected. Ordinary compression stays unchanged.

## Local comparison

macOS Apple Silicon, optimized builds, same Cargo.lock. The baseline executable was built from the clean checkout before edits. Each command fixture used 40 measured runs after five warmups, alternating before/after execution order, isolated homes, statistics disabled, and unique deduplication session IDs. Timing includes process startup, the fake command reading its fixture, filtering, and storage.

| Case | Before | After | Output comparison |
|---|---:|---:|---|
| `git log`, repository git-log fixture | 37.854 ms median | 37.887 ms median | Byte-identical: 909 stdout bytes |
| `npm install`, repository stdout/stderr fixtures | 38.934 ms median | 38.837 ms median | Byte-identical: 167 stdout bytes, empty stderr |
| MCP: 100 responses of 1 KiB, warmed session | 1.012 ms median | 1.007 ms median | Byte-identical responses |
| Synthetic 128 MiB command stdout | 419.2 MiB peak RSS | 82.7 MiB peak RSS | New build forwards all bytes raw |

The MCP comparison used 40 batches after five warmup batches in persistent sessions, alternating execution order; it excludes process startup and shutdown.

Peak RSS was measured with macOS `/usr/bin/time -l`, discarding command stdout in the benchmark harness. The synthetic case exercises the overflow path; it is not a claim about typical project workloads. The fixture timings show no meaningful difference in these samples, not a guarantee of zero overhead for every command.

## Regression coverage

`cargo test --locked` passes 122 unit tests and 27 integration tests. New cases cover boundary sizes, arbitrary bytes, empty/CRLF/Unicode frames, long sessions exceeding 64 MiB, oversized input in both MCP directions, pending-request saturation, server stdout closing before process exit, slow valid responses, and oversized stdout/stderr with exact bytes and nonzero exit status preserved.

`cargo clippy --locked --all-targets -- -D warnings`, `cargo fmt --check`, and the release build pass. Protocol and oversized-output regressions are also included in the scheduled security workflow. Validation was local on macOS; Linux and Windows execution remains for CI.
