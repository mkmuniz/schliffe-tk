mod camada_b;
mod filters;
mod shim;

use crate::core::{stats, store};
use std::process::ExitCode;

/// Minimum size for dedup to kick in (specs §8.3) — below this, the
/// reference line would cost more than it saves.
const DEDUP_MIN_BYTES: usize = 200;

/// `bornes/comandos` (specs.md §5) — `$PATH` shim: intercepts the output of
/// a shell command (`git`, `docker`, `cargo`, `pytest`...) invoked as
/// `invoked_name` (the process's `argv[0]`, resolved by `main.rs`).
pub fn run(invoked_name: &str, rest_args: &[String]) -> ExitCode {
    let Some(real_bin) = shim::resolve_real_binary(invoked_name) else {
        // Same message and exit code (127) a shell gives for a missing
        // command, so scripts probing for the tool behave as without Schliffe.
        eprintln!("{invoked_name}: command not found (schliffe shim: no real binary in PATH)");
        return ExitCode::from(127);
    };

    // Total passthrough for interactive human use — never filters when it's a
    // TTY (specs.md §5.1), nor when no AI agent is calling (see
    // `shim::agent_active`). On Unix this replaces the current process (a real exec).
    if shim::stdout_is_tty() || !shim::agent_active() {
        if let Err(e) = shim::exec_passthrough(&real_bin, rest_args) {
            eprintln!("schliffe: failed to run {invoked_name}: {e}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS; // unreachable on Unix (exec replaces the process)
    }

    // Cache (specs.md §8.2, v1 scope decided in §13): only the one case
    // provably immutable — `git show <explicit sha>`. `HEAD`/branch are
    // excluded because they can point to a different commit tomorrow. The
    // key carries a format version ("v1") so it never serves stale output if
    // the `git_diff` filter changes in the future.
    let git_show_cache_key = if invoked_name == "git"
        && rest_args.len() == 2
        && rest_args[0] == "show"
        && looks_like_git_sha(&rest_args[1])
    {
        Some(format!("git-show:v1:{}", rest_args[1]))
    } else {
        None
    };
    // (getting here already implies the non-interactive path — the TTY
    // branch above always returns/replaces the process before this line.)
    if let Some(key) = &git_show_cache_key
        && let Some(cached) = store::get_keyed(key)
    {
        print!("{cached}");
        if !cached.ends_with('\n') {
            println!();
        }
        return ExitCode::SUCCESS;
    }

    // Non-interactive path (pipe) — this is where filtering kicks in.
    let (run_args, subcommand): (Vec<String>, Option<&str>) = match invoked_name {
        "git" if is_git_status_filterable(rest_args) => (
            vec!["status".into(), "--porcelain=v1".into(), "--branch".into()],
            Some("git-status"),
        ),
        "git" if rest_args.first().map(String::as_str) == Some("log") => {
            (rest_args.to_vec(), Some("git-log"))
        }
        "git"
            if matches!(
                rest_args.first().map(String::as_str),
                Some("diff") | Some("show")
            ) =>
        {
            (rest_args.to_vec(), Some("git-diff"))
        }
        "git"
            if matches!(
                rest_args.first().map(String::as_str),
                Some("pull") | Some("merge")
            ) =>
        {
            (rest_args.to_vec(), Some("git-pull"))
        }
        "git"
            if rest_args.first().map(String::as_str) == Some("branch")
                && rest_args[1..]
                    .iter()
                    .any(|a| matches!(a.as_str(), "-a" | "-r" | "--all" | "--remotes")) =>
        {
            (rest_args.to_vec(), Some("git-branch"))
        }
        "docker"
            if matches!(
                rest_args
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .as_slice(),
                ["images", ..] | ["ps", ..] | ["image", "ls", ..] | ["container", "ls", ..]
            ) =>
        {
            (rest_args.to_vec(), Some("table"))
        }
        "pytest" => (rest_args.to_vec(), Some("pytest")),
        "cargo" if rest_args.first().map(String::as_str) == Some("test") => {
            (rest_args.to_vec(), Some("cargo-test"))
        }
        _ => (rest_args.to_vec(), None),
    };

    // Layer B (specs.md §5.2/§5.3): the declarative fallback for the long
    // tail — on stdout only when no dedicated Layer A parser matched. stderr
    // gets its own, independent lookup: many tools put their noise there
    // (cargo's "Compiling ...", npm's warnings, docker build progress), and
    // a stdout-only filter never saw it. stderr is only captured when a
    // filter asks for it; otherwise it streams straight through as before.
    let camada_b_filters = camada_b::load_all();
    let camada_b_match = if subcommand.is_none() {
        camada_b::find_match(
            &camada_b_filters,
            invoked_name,
            rest_args,
            camada_b::Stream::Stdout,
        )
    } else {
        None
    };
    let stderr_match = camada_b::find_match(
        &camada_b_filters,
        invoked_name,
        rest_args,
        camada_b::Stream::Stderr,
    );

    // No filter for this command at all: hand over to the real binary with
    // live output. Capturing would hold back EVERYTHING until the process
    // exits — found 2026-09-24: `pnpm dev`/`npm run dev` (servers that never
    // exit) showed the agent nothing at all while running.
    if subcommand.is_none() && camada_b_match.is_none() && stderr_match.is_none() {
        stats::record_passthrough(&stats::command_key(invoked_name, rest_args));
        if let Err(e) = shim::exec_passthrough(&real_bin, rest_args) {
            eprintln!("schliffe: failed to run {invoked_name}: {e}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS; // unreachable on Unix (exec replaces the process)
    }

    let captured = match shim::run_captured(&real_bin, &run_args, stderr_match.is_some()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("schliffe: failed to run {invoked_name}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let raw = String::from_utf8_lossy(&captured.stdout);

    // Business rule 2 (specs.md §4): no success shortcut if the process did
    // not finish successfully. This is each filter's own responsibility (never
    // fabricate "success"), not something achieved by turning off filtering
    // entirely on any non-zero exit — in fact pytest's highest-value case
    // (collection failure) only exists precisely when the exit code is NOT
    // zero. Found live while testing this session (2026-07-26): the earlier
    // version turned off the filter exactly in the case we most wanted to
    // demonstrate. Layer B enforces the same rule internally for its own
    // shortcuts (`match_output`/`on_empty`) — see camada_b/engine.rs.
    let filtered = match subcommand {
        Some("git-status") => Some(filters::git_status::filter(&raw)),
        Some("git-log") => Some(filters::git_log::filter(&raw)),
        Some("git-diff") => Some(filters::git_diff::filter(&raw)),
        Some("pytest") => Some(filters::pytest::filter(&raw)),
        Some("cargo-test") => Some(filters::cargo_test::filter(&raw)),
        Some("git-pull") => Some(filters::git_pull::filter(&raw)),
        Some("git-branch") => Some(filters::git_branch::filter(&raw)),
        Some("table") => Some(filters::table::filter(&raw)),
        _ => camada_b_match.map(|f| camada_b::apply(&f.pipeline, &raw, captured.exit_code)),
    };
    let mut output = finalize(filtered, &raw);

    let mut before = raw.len();
    let mut after_err = 0;

    // stderr is written first: tools that split their output usually emit
    // progress/warnings (stderr) before the final summary (stdout).
    if let (Some(f), Some(err)) = (stderr_match, &captured.stderr) {
        let raw_err = String::from_utf8_lossy(err);
        let filtered_err = camada_b::apply_stderr(&f.pipeline, &raw_err, captured.exit_code);
        let out_err = finalize(Some(filtered_err), &raw_err);
        before += raw_err.len();
        after_err = out_err.len();
        if !out_err.trim().is_empty() {
            eprint!("{out_err}");
            if !out_err.ends_with('\n') {
                eprintln!();
            }
        }
    }

    if captured.stdout_streamed {
        stats::record_passthrough(&stats::command_key(invoked_name, rest_args));
        return ExitCode::from(captured.exit_code as u8);
    }

    // Cache (§8.2): only writes after confirming success — never caches a
    // process that failed or was interrupted (business rule 2/3).
    if let Some(key) = &git_show_cache_key
        && captured.exit_code == 0
    {
        store::put_keyed(key, &output);
    }

    // Deduplication (§8.3) — the only one of the two store techniques that
    // actually cuts tokens. Scoped to the agent's real session when it
    // exposes one (`CLAUDE_CODE_SESSION_ID`, or `SCHLIFFE_SESSION_ID` for any
    // other agent); otherwise approximated by the time window alone.
    if output.len() >= DEDUP_MIN_BYTES {
        let hash = store::put(&output); // ensures it's recoverable via `schliffe show`
        let window: u64 = std::env::var("SCHLIFFE_DEDUP_WINDOW_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1800);
        let session = ["SCHLIFFE_SESSION_ID", "CLAUDE_CODE_SESSION_ID"]
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()));
        if let store::Dedup::SeenRecently =
            store::check_and_record_dedup(&output, window, session.as_deref())
        {
            let msg = format!("(same as previous output — schliffe show {hash} to view it again)");
            if msg.len() < output.len() {
                output = msg;
            }
        }
    }

    stats::record(
        &stats::command_key(invoked_name, rest_args),
        before,
        output.len() + after_err,
    );

    print!("{output}");
    if !output.ends_with('\n') {
        println!();
    }

    ExitCode::from(captured.exit_code as u8)
}

/// Progressive disclosure + business rule 6, shared by stdout and stderr.
///
/// Progressive disclosure (specs.md §8.1): when the filter signals that real
/// content was dropped (not just reformatted), store the raw output and
/// append a recoverable hint. Checked BEFORE rule 6 on purpose: if the hint
/// doesn't fit the budget, rule 6 falls back to the full raw output — which
/// already IS the complete information, so nothing is lost either way.
///
/// Business rule 6: filtered output can never be larger than the original.
fn finalize(filtered: Option<String>, raw: &str) -> String {
    let filtered = filtered.map(|f| {
        if f.contains("lines omitted") || f.contains("more changed lines") {
            let hash = store::put(raw);
            format!("{f}\n(full output: schliffe show {hash})")
        } else {
            f
        }
    });
    match filtered {
        Some(f) if f.len() < raw.len() => f,
        _ => raw.to_string(),
    }
}

/// `git status` with zero or more flags (but no path arguments) can be
/// replaced by `--porcelain=v1 --branch` for a smaller, machine-readable
/// output.  Path arguments or the `--` separator mean the caller wants a
/// scoped query — replacing the args would widen it, so we leave it alone.
fn is_git_status_filterable(rest_args: &[String]) -> bool {
    rest_args.first().map(String::as_str) == Some("status")
        && rest_args[1..]
            .iter()
            .all(|a| a.starts_with('-') && a != "--")
}

fn looks_like_git_sha(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn git_status_bare() {
        assert!(is_git_status_filterable(&s(&["status"])));
    }

    #[test]
    fn git_status_with_flags() {
        assert!(is_git_status_filterable(&s(&["status", "-s"])));
        assert!(is_git_status_filterable(&s(&["status", "--short"])));
        assert!(is_git_status_filterable(&s(&["status", "--porcelain"])));
        assert!(is_git_status_filterable(&s(&[
            "status",
            "--porcelain=v2",
            "--branch"
        ])));
        assert!(is_git_status_filterable(&s(&["status", "-uno"])));
    }

    #[test]
    fn git_status_with_paths_not_filterable() {
        assert!(!is_git_status_filterable(&s(&["status", "src/"])));
        assert!(!is_git_status_filterable(&s(&["status", "--", "src/"])));
        assert!(!is_git_status_filterable(&s(&[
            "status", "-s", "--", "file.rs"
        ])));
    }

    #[test]
    fn git_status_not_status() {
        assert!(!is_git_status_filterable(&s(&["diff"])));
        assert!(!is_git_status_filterable(&s(&["log"])));
        assert!(!is_git_status_filterable(&[]));
    }
}
