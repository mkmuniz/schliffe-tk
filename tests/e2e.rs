//! End-to-end tests against the compiled binary: the shim is exercised the
//! way a shell would run it (a symlink named after the tool, placed ahead of
//! a fake "real" tool in PATH), so every behavior validated live by hand
//! (agent gating, exit codes, fail-open, recovery, dedup, determinism) is now
//! part of `cargo test`. Unix-only: shims are symlinks and fakes are sh
//! scripts.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_schliffe");

/// An isolated sandbox: its own shims dir, fake-tools dir and store, so
/// tests never touch `~/.schliffe` and can run in parallel.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "schliffe-e2e-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = fs::remove_dir_all(&root);
        for sub in ["shims", "real", "store"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        symlink(BIN, root.join("shims").join("schliffe")).unwrap();
        Sandbox { root }
    }

    fn shims(&self) -> PathBuf {
        self.root.join("shims")
    }

    /// A fake real tool: a script that prints `stdout`/`stderr` and exits.
    fn fake_tool(&self, name: &str, stdout: &str, stderr: &str, exit: i32) {
        let out = self.root.join(format!("{name}.stdout"));
        let err = self.root.join(format!("{name}.stderr"));
        fs::write(&out, stdout).unwrap();
        fs::write(&err, stderr).unwrap();
        let script = format!(
            "#!/bin/sh\ncat '{}'\ncat '{}' >&2\nexit {exit}\n",
            out.display(),
            err.display()
        );
        let path = self.root.join("real").join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn shim(&self, name: &str) {
        symlink(BIN, self.shims().join(name)).unwrap();
    }

    /// Runs `name args...` through the shims dir, as an AI agent (`agent`)
    /// or as a plain script. stdout is a pipe here (never a TTY).
    /// Runs `name args...` the way an agent does: through a shell (the
    /// shim only filters when its parent is a shell). The trailing
    /// `exit $?` keeps the shell from exec-ing the command, so the shell
    /// really is the parent.
    fn run(&self, name: &str, args: &[&str], agent: bool, extra: &[(&str, &str)]) -> Output {
        self.run_via(true, name, args, agent, extra)
    }

    /// `via_shell = false`: spawned directly by another program (like
    /// Claude Code's own internal git calls, an MCP server, a test runner).
    fn run_via(
        &self,
        via_shell: bool,
        name: &str,
        args: &[&str],
        agent: bool,
        extra: &[(&str, &str)],
    ) -> Output {
        let path = format!(
            "{}:{}:/usr/bin:/bin",
            self.shims().display(),
            self.root.join("real").display()
        );
        let mut cmd = if via_shell {
            let mut c = Command::new("/bin/sh");
            c.arg("-c")
                .arg("\"$0\" \"$@\"; exit $?")
                .arg(self.shims().join(name))
                .args(args);
            c
        } else {
            let mut c = Command::new(self.shims().join(name));
            c.args(args);
            c
        };
        cmd.env_clear()
            .env("PATH", path)
            .env("HOME", &self.root)
            .env("SCHLIFFE_SHIMS_DIR", self.shims())
            .env("SCHLIFFE_STORE_DIR", self.root.join("store"))
            .env("SCHLIFFE_FILTERS_DIR", self.root.join("no-extra-filters"));
        if agent {
            cmd.env("CLAUDECODE", "1");
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

const GIT_LOG: &str = "commit 1111111111aaaa\nAuthor: Ana <a@x.io>\nDate:   Sat Jul 25 22:42:21 2026 -0300\n\n    feat: first\n\n    Long body sentence one here. Another sentence two.\n\ncommit 2222222222bbbb\nAuthor: Bia <b@x.io>\nDate:   Fri Jul 24 10:00:00 2026 -0300\n\n    fix: second\n\n    Body of the second commit.\n";

fn git_sandbox() -> Sandbox {
    let sb = Sandbox::new();
    sb.fake_tool("git", GIT_LOG, "", 0);
    sb.shim("git");
    sb
}

#[test]
fn without_agent_output_is_untouched() {
    let sb = git_sandbox();
    let out = sb.run("git", &["log"], false, &[]);
    assert_eq!(stdout(&out), GIT_LOG);
    assert!(out.status.success());
}

#[test]
fn agent_gets_filtered_output() {
    let sb = git_sandbox();
    let out = sb.run("git", &["log"], true, &[]);
    let text = stdout(&out);
    assert!(text.starts_with("1111111111 2026-07-25 22:42 Ana — feat: first"));
    assert!(text.contains("2222222222 2026-07-24 10:00 Bia — fix: second"));
    assert!(text.len() < GIT_LOG.len());
}

#[test]
fn agent_marker_without_a_shell_parent_is_not_filtered() {
    // Claude Code's own internal git calls, MCP servers, test runners...
    // inherit CLAUDECODE but aren't the model: raw output, nothing logged.
    let sb = git_sandbox();
    let out = sb.run_via(false, "git", &["log"], true, &[]);
    assert_eq!(stdout(&out), GIT_LOG);
    assert!(!sb.root.join(".schliffe").join("stats.log").exists());
}

#[test]
fn schliffe_disable_wins_inside_an_agent() {
    let sb = git_sandbox();
    let out = sb.run("git", &["log"], true, &[("SCHLIFFE_DISABLE", "1")]);
    assert_eq!(stdout(&out), GIT_LOG);
}

#[test]
fn exit_code_and_stderr_are_preserved() {
    let sb = Sandbox::new();
    sb.fake_tool("git", GIT_LOG, "fatal: something broke\n", 3);
    sb.shim("git");
    let out = sb.run("git", &["log"], true, &[]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stderr(&out), "fatal: something broke\n");
}

#[test]
fn missing_real_binary_behaves_like_command_not_found() {
    let sb = Sandbox::new();
    sb.shim("terraform"); // no fake "real" terraform
    let out = sb.run("terraform", &["plan"], true, &[]);
    assert_eq!(out.status.code(), Some(127));
    assert!(stderr(&out).contains("command not found"));
}

#[test]
fn shim_never_resolves_itself() {
    // SCHLIFFE_SHIMS_DIR points elsewhere, so the folder check can't help —
    // only the "is this my own executable" guard stops the recursion.
    // A tool name that exists nowhere else in PATH (macOS ships a real
    // /usr/bin/git, which would be found legitimately).
    let sb = Sandbox::new();
    sb.shim("schliffe-e2e-tool");
    let other = sb.root.join("elsewhere");
    fs::create_dir_all(&other).unwrap();
    let out = sb.run(
        "schliffe-e2e-tool",
        &["x"],
        true,
        &[("SCHLIFFE_SHIMS_DIR", other.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(127));
}

#[test]
fn omitted_content_is_recoverable_with_schliffe_show() {
    let sb = git_sandbox();
    let out = sb.run("git", &["log"], true, &[]);
    let text = stdout(&out);
    let hash = text
        .split("schliffe show ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("recovery hint present");
    let shown = sb.run("schliffe", &["show", hash], false, &[]);
    assert_eq!(stdout(&shown), GIT_LOG);
}

#[test]
fn dedup_is_scoped_to_the_session() {
    // Dedup only kicks in from 200 bytes of output, so use a longer log.
    let sb = Sandbox::new();
    sb.fake_tool("git", &GIT_LOG.repeat(4), "", 0);
    sb.shim("git");
    let a = [("CLAUDE_CODE_SESSION_ID", "a")];
    let b = [("CLAUDE_CODE_SESSION_ID", "b")];
    let first = stdout(&sb.run("git", &["log"], true, &a));
    let repeat = stdout(&sb.run("git", &["log"], true, &a));
    let other_session = stdout(&sb.run("git", &["log"], true, &b));
    assert!(repeat.starts_with("(same as previous output"));
    assert_eq!(other_session, first);
}

/// Determinism (specs §8.4): the same input must always produce
/// byte-identical output, or the provider's prompt cache is invalidated
/// on every repeat. Each run gets a fresh store so dedup can't kick in.
#[test]
fn output_is_deterministic() {
    let runs: Vec<String> = (0..3)
        .map(|_| {
            let sb = git_sandbox();
            stdout(&sb.run("git", &["log"], true, &[]))
        })
        .collect();
    assert_eq!(runs[0], runs[1]);
    assert_eq!(runs[1], runs[2]);
}

#[test]
fn stderr_filters_apply_only_for_agents() {
    let sb = Sandbox::new();
    let noise = "#0 building with \"default\" instance\n#1 [internal] load build definition\n#1 DONE 0.1s\n#2 [1/2] FROM alpine\n#3 [2/2] RUN make\n#3 0.51 error: boom\n";
    sb.fake_tool("docker", "", noise, 1);
    sb.shim("docker");

    let agent = sb.run("docker", &["build", "."], true, &[]);
    assert_eq!(
        stderr(&agent),
        "#2 [1/2] FROM alpine\n#3 [2/2] RUN make\n#3 0.51 error: boom\n"
    );
    assert_eq!(agent.status.code(), Some(1));

    let script = sb.run("docker", &["build", "."], false, &[]);
    assert_eq!(stderr(&script), noise);
}

#[test]
fn filter_never_inflates_output() {
    // A tiny git log: the one-line form plus hint would not be smaller, so
    // rule 6 must hand back the original bytes.
    let sb = Sandbox::new();
    let tiny = "commit a\n\n    x\n";
    sb.fake_tool("git", tiny, "", 0);
    sb.shim("git");
    let out = sb.run("git", &["log"], true, &[]);
    assert!(stdout(&out).len() <= tiny.len() + 1);
}

#[test]
fn compress_meta_command_summarizes_stdin() {
    use std::io::Write;
    let sb = Sandbox::new();
    let mut child = Command::new(sb.shims().join("schliffe"))
        .args(["compress", "--sentences", "1"])
        .env("SCHLIFFE_STORE_DIR", sb.root.join("store"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"The cache is fast. The parser handles diffs. Rare zebra quantum words appear here.",
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = stdout(&out);
    assert!(out.status.success());
    assert_eq!(text.trim().matches('.').count(), 1, "{text}");
}

/// A long-running command with no filter (a dev server) must stream its
/// output live — capturing it would show the agent nothing until exit.
#[test]
fn unfiltered_long_running_command_streams_live() {
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};

    let sb = Sandbox::new();
    let path = sb.root.join("real").join("pnpm");
    fs::write(&path, "#!/bin/sh\necho ready\nsleep 30\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    sb.shim("pnpm");

    let mut child = Command::new(sb.shims().join("pnpm"))
        .arg("dev")
        .env_clear()
        .env(
            "PATH",
            format!(
                "{}:{}:/usr/bin:/bin",
                sb.shims().display(),
                sb.root.join("real").display()
            ),
        )
        .env("HOME", &sb.root)
        .env("SCHLIFFE_SHIMS_DIR", sb.shims())
        .env("SCHLIFFE_STORE_DIR", sb.root.join("store"))
        .env("CLAUDECODE", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let elapsed = start.elapsed();
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(line, "ready\n");
    assert!(
        elapsed < Duration::from_secs(15),
        "output was buffered ({elapsed:?})"
    );
}

#[test]
fn stats_record_agent_commands_only_without_arguments() {
    let sb = git_sandbox();
    sb.run("git", &["log", "--author=secret-token"], true, &[]); // filtered
    sb.run("git", &["push", "origin"], true, &[]); // no filter: passthrough
    sb.run("git", &["log"], false, &[]); // not an agent: never recorded

    let log = fs::read_to_string(sb.root.join(".schliffe").join("stats.log")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "{log}");
    assert!(lines[0].contains("\tgit log\t"));
    assert!(lines[1].ends_with("\tgit push\t-\t-"));
    assert!(!log.contains("secret-token") && !log.contains("origin"));

    let report = stdout(&sb.run("schliffe", &["stats"], false, &[]));
    assert!(report.contains("top savings"), "{report}");
    assert!(report.contains("git push 1×"), "{report}");
}

fn run_with_stdin(sb: &Sandbox, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Output {
    use std::io::Write;
    let mut cmd = Command::new(sb.shims().join("schliffe"));
    cmd.args(args)
        .env_clear()
        .env("HOME", &sb.root)
        .env("SCHLIFFE_STORE_DIR", sb.root.join("store"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn hook_replaces_remote_mcp_output_and_ignores_the_rest() {
    let sb = Sandbox::new();
    let items: Vec<String> = (0..200)
        .map(|i| format!(r#"{{"id":{i},"name":"node {i}","parent":null}}"#))
        .collect();
    let payload = format!(r#"{{"nodes":[{}]}}"#, items.join(","));
    let input = serde_json_string(&[
        ("tool_name", "\"mcp__figma__get_metadata\"".into()),
        (
            "tool_response",
            format!(r#"[{{"type":"text","text":{}}}]"#, json_str(&payload)),
        ),
    ]);
    let out = run_with_stdin(&sb, &["hook", "post-tool-use"], &input, &[]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains(r#""hookEventName":"PostToolUse""#), "{text}");
    assert!(text.contains("updatedToolOutput"));
    assert!(!text.contains("parent"));
    assert!(text.len() < input.len());

    // A tool Schliffe doesn't handle: no output at all = original kept.
    let bash = r#"{"tool_name":"Bash","tool_response":{"stdout":"hi"}}"#;
    let out = run_with_stdin(&sb, &["hook", "post-tool-use"], bash, &[]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "");

    // Garbage in: still exit 0, no output (never breaks a tool call).
    let out = run_with_stdin(&sb, &["hook", "post-tool-use"], "not json", &[]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "");
}

#[test]
fn hook_install_and_uninstall_keep_other_settings() {
    let sb = Sandbox::new();
    let settings = sb.root.join("claude-settings.json");
    fs::write(
        &settings,
        r#"{"model":"opus","hooks":{"PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"my-linter"}]}]}}"#,
    )
    .unwrap();
    let env = [("SCHLIFFE_CLAUDE_SETTINGS", settings.to_str().unwrap())];

    for _ in 0..2 {
        // twice: must stay a single entry
        let out = run_with_stdin(&sb, &["hook", "install"], "", &env);
        assert!(out.status.success(), "{}", stderr(&out));
    }
    let after = fs::read_to_string(&settings).unwrap();
    assert_eq!(after.matches("hook post-tool-use").count(), 1, "{after}");
    assert!(after.contains("\"model\": \"opus\"") && after.contains("my-linter"));
    assert!(after.contains("Read|mcp__.*"));
    assert_eq!(
        after.matches("hook user-prompt-submit").count(),
        1,
        "{after}"
    );
    assert!(after.contains("UserPromptSubmit"));

    let out = run_with_stdin(&sb, &["hook", "uninstall"], "", &env);
    assert!(out.status.success());
    let after = fs::read_to_string(&settings).unwrap();
    assert!(!after.contains("post-tool-use") && after.contains("my-linter"));
    assert!(!after.contains("user-prompt-submit"));
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn serde_json_string(fields: &[(&str, String)]) -> String {
    let body: Vec<String> = fields.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
    format!("{{{}}}", body.join(","))
}

#[test]
fn report_reads_transcripts() {
    let sb = Sandbox::new();
    let projects = sb.root.join("projects").join("-Users-jane-acme");
    fs::create_dir_all(&projects).unwrap();
    let now = "2099-01-01T00:00:00.000Z"; // always inside the period
    fs::write(
        projects.join("s.jsonl"),
        format!(
            "{{\"type\":\"assistant\",\"cwd\":\"/Users/jane/acme\",\"timestamp\":\"{now}\",\"message\":{{\"id\":\"m1\",\"usage\":{{\"input_tokens\":1,\"cache_read_input_tokens\":400000,\"cache_creation_input_tokens\":1000,\"output_tokens\":300}},\"content\":[]}}}}\n"
        ),
    )
    .unwrap();
    let out = run_with_stdin(
        &sb,
        &["report", "--days", "3"],
        "",
        &[(
            "SCHLIFFE_CLAUDE_PROJECTS",
            sb.root.join("projects").to_str().unwrap(),
        )],
    );
    let text = stdout(&out);
    assert!(out.status.success());
    assert!(text.contains("1 sessions, 1 replies"), "{text}");
    assert!(text.contains("acme"), "{text}");
    assert!(text.contains("re-reading the conversation"), "{text}");
}

/// `schliffe show <hash>` used to build a path straight from its argument:
/// `schliffe show ../../../../etc/passwd` read arbitrary files. Schliffe's
/// own recovery hints ("schliffe show <hash>") live in text the model
/// reads, so a crafted log could have talked an agent into running one.
#[test]
fn show_refuses_anything_that_is_not_a_store_hash() {
    let sb = git_sandbox();
    let secret = sb.root.join("private-key.txt");
    fs::write(&secret, "SUPER-SECRET-VALUE").unwrap();

    // The hash from a real recovery hint still works, so the store itself
    // isn't broken by the validation.
    let hint = stdout(&sb.run("git", &["log"], true, &[]));
    let good = hint
        .split("schliffe show ")
        .nth(1)
        .and_then(|r| r.split(')').next())
        .expect("recovery hint")
        .to_string();
    let shown = sb.run("schliffe", &["show", &good], false, &[]);
    assert_eq!(
        stdout(&shown),
        GIT_LOG,
        "hash={good:?} stderr={:?}",
        stderr(&shown)
    );

    for attempt in [
        "../../../../../../etc/passwd",
        "../../private-key.txt",
        "..",
        "/etc/passwd",
        "cas/../../../private-key.txt",
    ] {
        let out = sb.run("schliffe", &["show", attempt], false, &[]);
        assert!(!out.status.success(), "accepted {attempt:?}");
        assert!(
            !stdout(&out).contains("SUPER-SECRET") && !stdout(&out).contains("root:"),
            "leaked a file with {attempt:?}: {}",
            stdout(&out)
        );
    }
    // A multi-byte argument used to panic while slicing the first 2 bytes.
    let out = sb.run("schliffe", &["show", "€xyz"], false, &[]);
    assert!(!stderr(&out).contains("panicked"), "{}", stderr(&out));
}

/// The store keeps raw command output — a `git diff` touching a `.env`, a
/// stack trace with a connection string. It was world-readable (0644).
#[test]
#[cfg(unix)]
fn everything_written_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let sb = git_sandbox();
    sb.run("git", &["log"], true, &[]);

    let mut checked = 0;
    let mut stack = vec![sb.root.join(".schliffe")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o077;
        assert_eq!(mode, 0, "directory readable by others: {}", dir.display());
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o077;
                assert_eq!(mode, 0, "file readable by others: {}", path.display());
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "nothing was written, so nothing was checked");
}

/// Drain pipes concurrently and fail promptly if a proxy lifecycle regresses.
fn run_with_deadline(mut cmd: Command, input: Vec<u8>) -> Output {
    use std::io::{Read, Write};
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let mut stdout = child.stdout.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).unwrap();
        buf
    });
    let mut stderr = child.stderr.take().unwrap();
    let err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).unwrap();
        buf
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("process hung beyond test deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    writer.join().unwrap();
    Output {
        status,
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
    }
}

fn mcp_command(sb: &Sandbox, script: &str) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.args(["mcp", "--keep-schemas", "--", "/bin/sh", "-c", script])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &sb.root)
        .env("SCHLIFFE_NO_STATS", "1");
    cmd
}

#[test]
fn mcp_session_can_exceed_64_mib_with_small_frames() {
    let sb = Sandbox::new();
    // Valid notifications, each comfortably below the per-message budget.
    let script = r#"i=0
while [ "$i" -lt 65 ]; do
    printf '{"jsonrpc":"2.0","method":"notice","params":"'
    head -c 1048576 /dev/zero | tr '\000' x
    printf '"}\n'
    i=$((i + 1))
done
printf '{"jsonrpc":"2.0","method":"finished"}\n'
"#;
    let out = run_with_deadline(mcp_command(&sb, script), Vec::new());
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(out.stdout.len() > 64 * 1024 * 1024);
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 66);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(text.lines().last().unwrap()).unwrap()["method"],
        "finished"
    );
}

#[test]
fn mcp_oversized_server_frame_is_not_forwarded_or_allowed_to_hang() {
    let sb = Sandbox::new();
    let script = "read request\nhead -c 67108865 /dev/zero | tr '\\000' x\nexec sleep 30";
    let out = run_with_deadline(
        mcp_command(&sb, script),
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n".to_vec(),
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("limit"));
    let response: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(response["id"], 1);
    assert!(response.get("error").is_some());
}

#[test]
fn mcp_oversized_client_frame_stops_the_server() {
    let sb = Sandbox::new();
    let out = run_with_deadline(
        mcp_command(&sb, "exec sleep 30"),
        vec![b'x'; 64 * 1024 * 1024 + 1],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("limit"));
    assert!(out.stdout.is_empty());
}

#[test]
fn mcp_stdout_eof_does_not_wait_forever_for_child_exit() {
    let sb = Sandbox::new();
    let out = run_with_deadline(
        mcp_command(&sb, "read request\nexec 1>&-\nexec sleep 30"),
        b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n".to_vec(),
    );
    assert!(!out.status.success());
    let response: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(response["id"], 7);
    assert!(response.get("error").is_some());
}

#[test]
fn mcp_client_eof_allows_a_slow_valid_response() {
    let sb = Sandbox::new();
    let out = run_with_deadline(
        mcp_command(
            &sb,
            "read request\nsleep 2\nprintf '{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\\n'",
        ),
        b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n".to_vec(),
    );
    assert!(out.status.success());
    let response: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(response.get("result").is_some());
    assert!(response.get("error").is_none());
}

#[test]
fn oversized_captured_streams_pass_through_exactly_and_keep_exit_code() {
    let sb = Sandbox::new();
    sb.shim("npm");
    let script = "#!/bin/sh\nhead -c 67108865 /dev/zero\nhead -c 67108865 /dev/zero >&2\nexit 23\n";
    let real = sb.root.join("real/npm");
    fs::write(&real, script).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
    let mut cmd = Command::new(sb.shims().join("npm"));
    cmd.arg("install")
        .env_clear()
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", sb.root.join("real").display()),
        )
        .env("HOME", &sb.root)
        .env("SCHLIFFE_FORCE", "1")
        .env("SCHLIFFE_NO_STATS", "1");
    let out = run_with_deadline(cmd, Vec::new());
    assert_eq!(out.status.code(), Some(23));
    for stream in [&out.stdout, &out.stderr] {
        assert_eq!(stream.len(), 64 * 1024 * 1024 + 1);
        assert!(stream.iter().all(|&b| b == 0));
    }
}

#[test]
fn mcp_pending_request_limit_rejects_excess_without_forwarding_it() {
    let sb = Sandbox::new();
    let input: String = (0..1025)
        .map(|id| format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"ping\"}}\n"))
        .collect();
    let script =
        "n=0\nwhile IFS= read -r request; do n=$((n + 1)); done\nprintf '%s\\n' \"$n\" >&2";
    let out = run_with_deadline(mcp_command(&sb, script), input.into_bytes());
    assert_eq!(stderr(&out).trim(), "1024");
    let text = stdout(&out);
    let replies: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(replies.len(), 1025);
    let rejected = replies.iter().find(|r| r["id"] == 1024).unwrap();
    assert!(
        rejected["error"]["message"]
            .as_str()
            .unwrap()
            .contains("too many pending")
    );
}

#[test]
fn mcp_http_reuses_schema_and_result_compression() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let sb = Sandbox::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for request_number in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let n = stream.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let header_end = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")?
                        .trim()
                        .parse::<usize>()
                        .ok()
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let n = stream.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..n]);
            }
            let body = if request_number == 0 {
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": {"tools": [
                        {"name":"search","description":format!("Searches documents. {}", "Supports pagination and filters. ".repeat(30)),"inputSchema":{"type":"object","properties":{"query":{"type":"string"},"category":{"type":"string"},"limit":{"type":"integer"}}}},
                        {"name":"update","description":format!("Updates a document. {}", "Requires confirmation and a document id. ".repeat(30)),"inputSchema":{"type":"object","properties":{"id":{"type":"string"},"title":{"type":"string"},"body":{"type":"string"}}}}
                    ]}
                })
            } else {
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 2,
                    "result": {"content": [{"type":"text","text":serde_json::json!({"items":[null,null,null,null,null,null,null,null,null,null,"eleven","twelve"],"message":"x".repeat(400)}).to_string()}]}
                })
            };
            let body = body.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: e2e\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            )
            .unwrap();
        }
    });

    let mut command = Command::new(BIN);
    command
        .args(["mcp", "--url", &format!("http://{address}")])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &sb.root)
        .env("SCHLIFFE_NO_STATS", "1");
    let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"search\"}}\n".to_vec();
    let out = run_with_deadline(command, input);
    server.join().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let lines: Vec<_> = stdout(&out)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["result"]["tools"].as_array().unwrap().len(), 3);
    assert!(stdout(&out).len() < 2_000);
}
