use std::env;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Resolves the real binary for `name` in $PATH, skipping Schliffe's own shims folder.
///
/// Real bug found and fixed this session (2026-07-26): the first version
/// tried to *discover* its own folder from `argv[0]`, assuming the shell
/// always passes the fully resolved path. Not true — bash can pass just the
/// bare name ("git"), no directory at all. That made the "skip my own
/// folder" check fail silently, resolve itself as the "real binary", and
/// reprocess its own already-filtered output a second time (filter applied
/// twice).
///
/// Fix: don't *discover* the shims folder, **know** it ahead of time — it's
/// Schliffe itself that creates the symlinks there during installation, so
/// there's no need to infer anything at runtime.
///
/// Business rule 7 (specs.md §4): always inherits the same $PATH as the
/// parent process, never resolves a binary on its own outside of that.
///
/// Second guard (found live on macOS, 2026-09-24): skipping the shims folder
/// isn't enough on its own — with `SCHLIFFE_SHIMS_DIR` pointing somewhere else
/// while `~/.schliffe/shims` was still in PATH, the shim resolved ITSELF as the
/// real `git` and re-invoked itself until the OS refused to fork (EAGAIN).
/// So any candidate that is this very executable (after following symlinks)
/// is skipped too, whatever folder it sits in.
///
/// Third guard (2026-09-28 hardening): `$PATH` entries that aren't absolute
/// are skipped. A relative entry — `.`, an empty entry from a stray `:` —
/// resolves against the *current directory*, so an agent working inside a
/// cloned repository that ships an executable named `git` would run it.
/// Verified exploitable before this landed: with `.` in `$PATH`, a file
/// `./git` in an untrusted checkout was executed. A shell does the same,
/// but Schliffe sits in front of every command an agent runs on untrusted
/// code, so it refuses instead. `SCHLIFFE_ALLOW_RELATIVE_PATH=1` restores
/// the old behavior for anyone who relies on it.
pub fn resolve_real_binary(name: &str) -> Option<PathBuf> {
    let own_dir = shims_dir();
    let own_exe = env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    let path_var = env::var_os("PATH")?;
    let allow_relative = env::var_os("SCHLIFFE_ALLOW_RELATIVE_PATH").is_some_and(|v| !v.is_empty());

    for dir in env::split_paths(&path_var) {
        if !allow_relative && !dir.is_absolute() {
            // Also covers the empty entry (`PATH=/usr/bin:`), which means
            // "the current directory" to the shell.
            continue;
        }
        let dir_canon = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if Some(&dir_canon) == own_dir.as_ref() {
            continue;
        }
        let candidate = dir.join(name);
        if own_exe.is_some() && candidate.canonicalize().ok() == own_exe {
            continue;
        }
        if is_executable(&candidate) {
            return Some(candidate);
        }
        // npm/pnpm/yarn are `.cmd` wrappers on Windows, not `.exe` — looking
        // for `.exe` only meant their shims never found the real tool.
        #[cfg(windows)]
        for ext in ["exe", "cmd", "bat"] {
            let with_ext = dir.join(format!("{name}.{ext}"));
            if with_ext.is_file() {
                return Some(with_ext);
            }
        }
    }
    None
}

/// Folder where Schliffe's shims live — configurable via `SCHLIFFE_SHIMS_DIR` to
/// make testing easier (several installs side by side), defaulting to
/// `~/.schliffe/shims`. Canonicalized to compare reliably against `$PATH`
/// entries (which can have different forms of the same path).
fn shims_dir() -> Option<PathBuf> {
    let raw = match env::var_os("SCHLIFFE_SHIMS_DIR") {
        Some(v) => PathBuf::from(v),
        None => {
            let home = env::var_os("HOME")?;
            PathBuf::from(home).join(".schliffe").join("shims")
        }
    };
    Some(raw.canonicalize().unwrap_or(raw))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// Whether the caller is an AI agent — the only case where filtering pays
/// off. "Not a TTY" alone isn't enough: found while activating on macOS
/// (2026-09-24), VS Code's own Git panel, git hooks (husky/lint-staged) and
/// npm scripts also call `git log`/`git diff`/`npm` through a pipe, and would
/// get truncated output they can't parse. Agents mark their shell
/// environment: Claude Code sets `CLAUDECODE=1`, and `AI_AGENT` is a generic
/// marker other agents are adopting. `SCHLIFFE_FORCE=1` opts any other tool in,
/// `SCHLIFFE_DISABLE=1` turns filtering off even inside an agent.
///
/// Second condition (2026-09-27): the command must have been launched by a
/// shell. The agent marker is inherited by EVERY process under the agent,
/// not just the commands the model runs: Claude Code's own internal `git`
/// calls (for its UI), MCP servers, test runners and node scripts spawn
/// `git`/`docker` directly — ~8k such calls a day showed up as "agent
/// commands" in `stats`, and a program parsing `git log` would have
/// received the compacted text. The model's commands always go through a
/// shell (verified: parent `zsh`, grandparent the `claude` binary).
/// `SCHLIFFE_FORCE=1` skips this check.
pub fn agent_active() -> bool {
    let is_set = |name: &str| env::var_os(name).filter(|v| !v.is_empty()).is_some();
    agent_active_from(is_set, parent_is_shell)
}

fn agent_active_from(is_set: impl Fn(&str) -> bool, parent_is_shell: impl Fn() -> bool) -> bool {
    if is_set("SCHLIFFE_DISABLE") {
        return false;
    }
    if is_set("SCHLIFFE_FORCE") {
        return true;
    }
    ["CLAUDECODE", "AI_AGENT"].iter().any(|name| is_set(name)) && parent_is_shell()
}

const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ash", "ksh", "mksh", "fish", "tcsh", "csh", "nu", "busybox",
];

/// Whether the parent process is a shell. When the parent can't be
/// identified, assumes it is (the behavior before this check existed).
fn parent_is_shell() -> bool {
    match parent_name() {
        Some(name) => {
            let base = name
                .rsplit('/')
                .next()
                .unwrap_or(&name)
                .trim_start_matches('-');
            SHELLS.contains(&base)
        }
        None => true,
    }
}

#[cfg(target_os = "linux")]
fn parent_name() -> Option<String> {
    let ppid = std::os::unix::process::parent_id();
    std::fs::read_link(format!("/proc/{ppid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| {
            std::fs::read_to_string(format!("/proc/{ppid}/comm"))
                .ok()
                .map(|s| s.trim().to_string())
        })
}

#[cfg(target_os = "macos")]
fn parent_name() -> Option<String> {
    // SAFETY: libproc is part of libSystem, always linked on macOS; this
    // is the documented signature of proc_pidpath(2).
    unsafe extern "C" {
        // libproc, part of libSystem (always linked on macOS).
        fn proc_pidpath(pid: i32, buffer: *mut u8, buffersize: u32) -> i32;
    }
    let ppid = std::os::unix::process::parent_id() as i32;
    let mut buf = vec![0u8; 4096];
    // SAFETY: the buffer is valid and writable for its full length, and
    // proc_pidpath writes at most `buffersize` bytes into it.
    let len = unsafe { proc_pidpath(ppid, buf.as_mut_ptr(), buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    buf.truncate(len as usize);
    String::from_utf8(buf).ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn parent_name() -> Option<String> {
    None
}

pub struct CapturedRun {
    pub stdout: Vec<u8>,
    /// Oversized stdout has already been forwarded byte-for-byte.
    pub stdout_streamed: bool,
    /// Only `Some` when stderr was requested and stayed within the budget.
    pub stderr: Option<Vec<u8>>,
    pub exit_code: i32,
}

/// Runs the real binary capturing stdout — used on the non-interactive
/// (agent) path, where the output will be filtered before it reaches the
/// agent. stderr passes straight through (same as the original command)
/// unless `capture_stderr` is set, i.e. a Layer B filter targets stderr.
pub fn run_captured(
    real_bin: &Path,
    args: &[String],
    capture_stderr: bool,
) -> std::io::Result<CapturedRun> {
    let mut child = Command::new(real_bin)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(if capture_stderr {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .spawn()?;
    // Drain both pipes concurrently: a full stderr pipe must never block
    // a child whose stdout we are reading. No command execution timeout.
    let stderr = child.stderr.take().map(|pipe| {
        std::thread::spawn(move || {
            capture_bounded(
                pipe,
                io::stderr().lock(),
                crate::core::secure::MAX_INPUT_BYTES,
            )
        })
    });
    let stdout = capture_bounded(
        child.stdout.take().expect("stdout piped"),
        io::stdout().lock(),
        crate::core::secure::MAX_INPUT_BYTES,
    );
    if stdout.is_err() {
        let _ = child.kill();
    }
    let stderr = stderr
        .map(|t| {
            t.join()
                .unwrap_or_else(|_| Err(io::Error::other("stderr reader panicked")))
        })
        .transpose();
    if stderr.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let stdout = stdout?;
    Ok(CapturedRun {
        stdout_streamed: stdout.is_none(),
        stdout: stdout.unwrap_or_default(),
        stderr: stderr?.flatten(),
        exit_code: status.code().unwrap_or(1),
    })
}

/// Keep ordinary output available for the existing filters. Once a stream
/// exceeds the budget, forward the buffered prefix and all remaining bytes
/// without filtering, extra newlines, or a second execution of the command.
fn capture_bounded(
    mut reader: impl Read,
    mut passthrough: impl Write,
    limit: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut captured = Some(Vec::new());
    let mut chunk = [0; 16 * 1024];
    loop {
        let n = match reader.read(&mut chunk) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if n == 0 {
            break;
        }
        if let Some(buf) = captured.as_mut() {
            if n <= limit.saturating_sub(buf.len()) {
                buf.extend_from_slice(&chunk[..n]);
                continue;
            }
            passthrough.write_all(buf)?;
            captured = None;
        }
        passthrough.write_all(&chunk[..n])?;
        passthrough.flush()?;
    }
    Ok(captured)
}

/// Interactive (TTY) path: replaces the current process with the real
/// binary, without filtering anything — total passthrough. On Unix this is
/// a real exec (same PID, no extra process). On Windows, spawns and waits
/// (there's no exec-replace in std).
#[cfg(unix)]
pub fn exec_passthrough(real_bin: &Path, args: &[String]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let err = Command::new(real_bin).args(args).exec();
    Err(err)
}

#[cfg(windows)]
pub fn exec_passthrough(real_bin: &Path, args: &[String]) -> std::io::Result<()> {
    let status = Command::new(real_bin).args(args).status()?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_vars(vars: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |name| vars.contains(&name)
    }

    #[test]
    fn bounded_capture_preserves_small_and_oversized_bytes() {
        for input in [
            b"".as_slice(),
            b"12345678",
            b"123456789",
            b"\xff\x00untrusted\n",
        ] {
            let mut forwarded = Vec::new();
            let captured = capture_bounded(input, &mut forwarded, 8).unwrap();
            if input.len() <= 8 {
                assert_eq!(captured.unwrap(), input);
                assert!(forwarded.is_empty());
            } else {
                assert!(captured.is_none());
                assert_eq!(forwarded, input);
            }
        }
    }

    #[test]
    fn bounded_capture_forwards_prefix_and_later_chunks_once() {
        let input = vec![b'x'; 100_000];
        let mut forwarded = Vec::new();
        assert!(
            capture_bounded(input.as_slice(), &mut forwarded, 20_000)
                .unwrap()
                .is_none()
        );
        assert_eq!(forwarded, input);
    }

    #[test]
    fn plain_pipe_without_agent_is_not_filtered() {
        assert!(!agent_active_from(with_vars(&[]), || true));
    }

    /// A relative `$PATH` entry resolves against the current directory, so
    /// an untrusted checkout shipping an executable named `git` would be
    /// run. Verified exploitable before the guard existed.
    #[test]
    fn relative_path_entries_are_never_used() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("schliffe-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        let planted = dir.join("repo").join("schliffe-fake-tool");
        std::fs::write(&planted, "#!/bin/sh\necho pwned\n").unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();

        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.join("repo")).unwrap();
        // SAFETY: single-threaded section of this test; restored below.
        unsafe {
            std::env::set_var("PATH", ".::/nonexistent");
        }
        let found = resolve_real_binary("schliffe-fake-tool");
        // SAFETY: same single-threaded section; restores the environment for
        // whatever runs next in this process.
        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin");
        }
        std::env::set_current_dir(previous).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(found.is_none(), "resolved a relative PATH entry: {found:?}");
    }

    #[test]
    fn agent_markers_enable_filtering() {
        assert!(agent_active_from(with_vars(&["CLAUDECODE"]), || true));
        assert!(agent_active_from(with_vars(&["AI_AGENT"]), || true));
        assert!(agent_active_from(with_vars(&["SCHLIFFE_FORCE"]), || false));
    }

    #[test]
    fn disable_wins_over_agent_markers() {
        assert!(!agent_active_from(
            with_vars(&["CLAUDECODE", "SCHLIFFE_DISABLE"]),
            || true
        ));
    }

    #[test]
    fn agent_marker_without_a_shell_parent_is_not_the_model() {
        // e.g. Claude Code's own `git` calls, an MCP server, a test runner.
        assert!(!agent_active_from(with_vars(&["CLAUDECODE"]), || false));
    }
}
