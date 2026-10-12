//! `schliffe setup` — self-contained activation, the keystone every install
//! path reuses (specs.md §5.1, M8). A package manager (Homebrew, cargo, npm)
//! or a plugin only has to put the binary on the system; `schliffe setup`
//! then does what the shell installer used to do inline, but auditable
//! (`--dry-run`) and reversible (`--uninstall`):
//!
//!   1. symlink the shimmed commands (+ `schliffe`) to this binary;
//!   2. put `~/.schliffe/shims` first in `$PATH` across the login and
//!      interactive shell files;
//!   3. register the Claude Code hook (reusing `bornes::hook`);
//!   4. lock down `~/.schliffe` (it holds raw command output + prompts).
//!
//! Unix-only: shims are symlinks and the PATH logic targets POSIX shells.
//! Native Windows uses `install.ps1`.

use crate::bornes::hook;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Commands shimmed by default — the ones with a filter today (Layer A +
/// Layer B). A shim for a tool that isn't installed would make `command -v`
/// lie, so only existing tools are linked; re-run after installing more.
const DEFAULT_COMMANDS: &[&str] = &[
    "git",
    "cargo",
    "pytest",
    "docker",
    "npm",
    "pnpm",
    "yarn",
    "pip",
    "pip3",
    "dotnet",
    "go",
    "terraform",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ShellKind {
    Zsh,
    Bash,
}

pub fn run(dry_run: bool, no_hook: bool) -> ExitCode {
    if cfg!(windows) {
        eprintln!(
            "schliffe: `setup` is Unix-only (shims are symlinks). On native Windows use install.ps1."
        );
        return ExitCode::FAILURE;
    }
    let Some(home) = home_dir() else {
        eprintln!("schliffe: no HOME — cannot locate where to install");
        return ExitCode::FAILURE;
    };
    let real_bin = match real_binary() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("schliffe: cannot resolve this binary's path: {e}");
            return ExitCode::FAILURE;
        }
    };
    let shims_dir = shims_dir(&home);

    if dry_run {
        println!("schliffe setup --dry-run (no changes will be made):");
    }

    // 1. Shims.
    let (shimmed, skipped) = plan_shims(&shims_dir);
    if dry_run {
        println!(
            "  would link in {}: {} (+ schliffe) -> {}",
            shims_dir.display(),
            join_or_none(&shimmed),
            real_bin.display()
        );
        if !skipped.is_empty() {
            println!("  would skip (not installed here): {}", skipped.join(" "));
        }
    } else if let Err(e) = apply_shims(&shims_dir, &real_bin, &shimmed) {
        eprintln!("schliffe: failed to create shims: {e}");
        return ExitCode::FAILURE;
    } else {
        println!(
            "schliffe: shims in {} for: {} (+ schliffe)",
            shims_dir.display(),
            join_or_none(&shimmed)
        );
        if !skipped.is_empty() {
            println!(
                "schliffe: skipped (not installed here): {}",
                skipped.join(" ")
            );
        }
    }

    // 2. PATH across shell files.
    let kind = detect_shell();
    for (file, placement) in profile_targets(&home, kind) {
        let existing = std::fs::read_to_string(&file).ok();
        match path_edit(existing.as_deref(), &shims_dir, placement) {
            None => println!(
                "schliffe: PATH already configured in {} (nothing to do)",
                file.display()
            ),
            Some(new_content) => {
                if dry_run {
                    println!("  would update PATH in {}", file.display());
                } else if let Err(e) = write_profile(&file, &new_content) {
                    eprintln!("schliffe: failed to update {}: {e}", file.display());
                    return ExitCode::FAILURE;
                } else {
                    println!("schliffe: PATH configured in {}", file.display());
                }
            }
        }
    }

    // 4. Lock down the tree (holds raw output + prompts). Done before the hook
    //    so a failure there still leaves permissions correct.
    if !dry_run {
        harden_tree(&home);
    }

    // 3. Claude Code hook — reuses bornes::hook (backup + idempotent).
    // Skipped with --no-hook (e.g. the plugin ships its own hooks, so
    // registering here too would double-process every MCP result).
    let skip_hook = no_hook || std::env::var_os("SCHLIFFE_NO_HOOK").is_some();
    if dry_run {
        if skip_hook {
            println!("  would skip the Claude Code hook (--no-hook)");
        } else {
            println!("  would register the Claude Code hook (schliffe hook install)");
        }
    } else if skip_hook {
        println!("schliffe: skipping the Claude Code hook (--no-hook / SCHLIFFE_NO_HOOK)");
    } else if home.join(".claude").is_dir() {
        hook::install();
    } else {
        println!("schliffe: Claude Code not found (~/.claude missing) — skipped its hook.");
        println!("          Register it later with: schliffe hook install");
    }

    if !dry_run {
        println!(
            "\nschliffe: done. Open a new terminal to activate it (covers interactive use\n\
             and the login shell Claude Code invokes commands through)."
        );
    }
    ExitCode::SUCCESS
}

pub fn uninstall(dry_run: bool) -> ExitCode {
    if cfg!(windows) {
        eprintln!("schliffe: `setup --uninstall` is Unix-only.");
        return ExitCode::FAILURE;
    }
    let Some(home) = home_dir() else {
        eprintln!("schliffe: no HOME");
        return ExitCode::FAILURE;
    };
    let shims_dir = shims_dir(&home);

    if dry_run {
        println!("schliffe setup --uninstall --dry-run (no changes will be made):");
    }

    // Remove the shim symlinks (keep the dir; it may hold the user's own).
    if let Ok(entries) = std::fs::read_dir(&shims_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_symlink() {
                if dry_run {
                    println!("  would remove shim {}", path.display());
                } else {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        if !dry_run {
            println!("schliffe: removed shims in {}", shims_dir.display());
        }
    }

    // Strip our PATH block from every profile file.
    for file in all_profiles(&home) {
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        let cleaned = remove_path_block(&content, &shims_dir);
        if cleaned != content {
            if dry_run {
                println!("  would remove PATH line from {}", file.display());
            } else if let Err(e) = std::fs::write(&file, cleaned) {
                eprintln!("schliffe: failed to update {}: {e}", file.display());
            } else {
                println!("schliffe: removed PATH line from {}", file.display());
            }
        }
    }

    if dry_run {
        println!("  would unregister the Claude Code hook");
    } else {
        hook::uninstall();
    }
    ExitCode::SUCCESS
}

// ---- path/profile helpers (pure where possible, for tests) -----------------

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn shims_dir(home: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("SCHLIFFE_SHIMS_DIR") {
        return PathBuf::from(dir);
    }
    home.join(".schliffe").join("shims")
}

/// The real binary to point shims at: this executable, with symlinks resolved
/// so a shim never ends up pointing at another shim (which would loop).
fn real_binary() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(std::fs::canonicalize(&exe).unwrap_or(exe))
}

fn path_line(shims_dir: &Path) -> String {
    format!("export PATH=\"{}:$PATH\"", shims_dir.display())
}

fn detect_shell() -> ShellKind {
    let shell = std::env::var("SHELL").unwrap_or_default();
    if Path::new(&shell)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains("zsh"))
    {
        ShellKind::Zsh
    } else {
        ShellKind::Bash
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Placement {
    /// Login file (`.profile`/`.zprofile`): no interactivity guard — append.
    Append,
    /// Interactive bash (`.bashrc`): must precede the "if not interactive,
    /// exit" guard, so go at the very top.
    Prepend,
    /// Interactive zsh (`.zshrc`): after nvm/pyenv touch PATH, so append.
    AppendZsh,
}

fn profile_targets(home: &Path, kind: ShellKind) -> Vec<(PathBuf, Placement)> {
    match kind {
        ShellKind::Zsh => vec![
            (home.join(".zprofile"), Placement::Append),
            (home.join(".zshrc"), Placement::AppendZsh),
        ],
        ShellKind::Bash => vec![
            (home.join(".profile"), Placement::Append),
            (home.join(".bashrc"), Placement::Prepend),
        ],
    }
}

fn all_profiles(home: &Path) -> Vec<PathBuf> {
    [".profile", ".bashrc", ".zprofile", ".zshrc"]
        .iter()
        .map(|f| home.join(f))
        .collect()
}

/// Returns the new file content, or `None` when the shims dir is already on
/// the PATH there (nothing to do). Pure: operates on content, not files.
fn path_edit(existing: Option<&str>, shims_dir: &Path, placement: Placement) -> Option<String> {
    let marker = shims_dir.display().to_string();
    if existing.is_some_and(|c| c.contains(&marker)) {
        return None;
    }
    let line = path_line(shims_dir);
    let block = match placement {
        Placement::Append => {
            format!("\n# Schliffe — $PATH shim (specs.md §5.1)\n{line}\n")
        }
        Placement::AppendZsh => format!(
            "\n# Schliffe — $PATH shim (specs.md §5.1). Keep at the END, after\n\
             # nvm/pyenv/etc., so the shims stay first in PATH.\n{line}\n"
        ),
        Placement::Prepend => format!(
            "# Schliffe — $PATH shim (specs.md §5.1). Must come BEFORE any\n\
             # interactivity guard the rest of the file has (e.g. Ubuntu's\n\
             # default \"if not interactive, exit\").\n{line}\n"
        ),
    };
    Some(match (placement, existing) {
        (Placement::Prepend, Some(current)) => format!("{block}\n{current}"),
        (_, Some(current)) => format!("{current}{block}"),
        (_, None) => block,
    })
}

/// Removes the Schliffe PATH comment block + the `export PATH=...shims...`
/// line from a profile's content. Pure.
fn remove_path_block(content: &str, shims_dir: &Path) -> String {
    let marker = shims_dir.display().to_string();
    let mut out: Vec<&str> = Vec::new();
    for line in content.lines() {
        let is_ours = line.contains("Schliffe — $PATH shim")
            || (line.contains("export PATH=") && line.contains(&marker))
            || line
                .trim_start()
                .starts_with("# nvm/pyenv/etc., so the shims stay first")
            || line
                .trim_start()
                .starts_with("# interactivity guard the rest of the file")
            || line
                .trim_start()
                .starts_with("# default \"if not interactive, exit\"")
            || line.trim_start().starts_with("# Keep at the END, after");
        if !is_ours {
            out.push(line);
        }
    }
    let mut joined = out.join("\n");
    // Collapse the blank line our block used to be padded with.
    while joined.contains("\n\n\n") {
        joined = joined.replace("\n\n\n", "\n\n");
    }
    if content.ends_with('\n') && !joined.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

// ---- IO (not unit-tested; exercised by the e2e installer path) -------------

fn plan_shims(shims_dir: &Path) -> (Vec<String>, Vec<String>) {
    let real_path = path_without_shims(shims_dir);
    let mut shimmed = Vec::new();
    let mut skipped = Vec::new();
    for cmd in DEFAULT_COMMANDS {
        let already = shims_dir.join(cmd).is_symlink();
        if already || which(cmd, &real_path) {
            shimmed.push((*cmd).to_string());
        } else {
            skipped.push((*cmd).to_string());
        }
    }
    (shimmed, skipped)
}

fn apply_shims(shims_dir: &Path, real_bin: &Path, shimmed: &[String]) -> std::io::Result<()> {
    crate::core::secure::create_dir_private(shims_dir)?;
    for cmd in shimmed
        .iter()
        .map(String::as_str)
        .chain(std::iter::once("schliffe"))
    {
        let link = shims_dir.join(cmd);
        let _ = std::fs::remove_file(&link);
        symlink(real_bin, &link)?;
    }
    Ok(())
}

// Shims are Unix symlinks. `setup` returns early on Windows (see `run`), so
// this is never reached there — but it must still compile, hence the stub.
#[cfg(unix)]
fn symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(not(unix))]
fn symlink(_src: &Path, _dst: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "schliffe setup is Unix-only",
    ))
}

/// PATH entries with the shims dir and the empty string removed — so a re-run
/// doesn't discover its own shims and think every tool is installed. Also
/// unions in the user's login-shell PATH (nvm/dotnet live there, unseen from
/// an agent's trimmed environment).
fn path_without_shims(shims_dir: &Path) -> Vec<PathBuf> {
    let shims = shims_dir.to_string_lossy().into_owned();
    let mut raw = std::env::var("PATH").unwrap_or_default();
    if let Some(login) = login_shell_path() {
        raw.push(':');
        raw.push_str(&login);
    }
    let mut seen = std::collections::HashSet::new();
    raw.split(':')
        .filter(|p| !p.is_empty() && *p != shims)
        .filter(|p| seen.insert(p.to_string()))
        .map(PathBuf::from)
        .collect()
}

fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
    let out = std::process::Command::new(shell)
        .args(["-lic", "printf %s \"$PATH\""])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout);
    path.lines()
        .last()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn which(cmd: &str, dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|d| {
        let p = d.join(cmd);
        p.is_file() || p.is_symlink()
    })
}

fn write_profile(file: &Path, content: &str) -> std::io::Result<()> {
    let mut f = std::fs::File::create(file)?;
    f.write_all(content.as_bytes())
}

fn harden_tree(home: &Path) {
    // New files are written owner-only by the binary; this repairs an older
    // tree in one pass. Best-effort — never blocks setup.
    let tree = home.join(".schliffe");
    let _ = std::process::Command::new("chmod")
        .args(["-R", "go-rwx"])
        .arg(&tree)
        .status();
}

fn join_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shims() -> PathBuf {
        PathBuf::from("/home/u/.schliffe/shims")
    }

    #[test]
    fn append_adds_block_when_absent() {
        let out = path_edit(Some("export FOO=1\n"), &shims(), Placement::Append).unwrap();
        assert!(out.starts_with("export FOO=1\n"));
        assert!(out.contains("/home/u/.schliffe/shims:$PATH"));
    }

    #[test]
    fn prepend_puts_block_first() {
        let out = path_edit(
            Some("[ -z \"$PS1\" ] && return\n"),
            &shims(),
            Placement::Prepend,
        )
        .unwrap();
        assert!(out.starts_with("# Schliffe"));
        let shim_pos = out.find("shims:$PATH").unwrap();
        let guard_pos = out.find("return").unwrap();
        assert!(shim_pos < guard_pos, "shim must come before the guard");
    }

    #[test]
    fn already_present_is_noop() {
        let existing = "export PATH=\"/home/u/.schliffe/shims:$PATH\"\n";
        assert!(path_edit(Some(existing), &shims(), Placement::Append).is_none());
        assert!(path_edit(Some(existing), &shims(), Placement::Prepend).is_none());
    }

    #[test]
    fn missing_file_creates_block() {
        let out = path_edit(None, &shims(), Placement::Append).unwrap();
        assert!(out.contains("shims:$PATH"));
    }

    #[test]
    fn remove_strips_our_block_only() {
        let original = "export FOO=1\n";
        let with = path_edit(Some(original), &shims(), Placement::Append).unwrap();
        let cleaned = remove_path_block(&with, &shims());
        assert!(!cleaned.contains("shims:$PATH"));
        assert!(!cleaned.contains("Schliffe"));
        assert!(cleaned.contains("export FOO=1"));
    }

    #[test]
    fn remove_is_idempotent_and_safe_on_unrelated_content() {
        let content = "export BAR=2\nalias g=git\n";
        assert_eq!(remove_path_block(content, &shims()), content);
    }

    #[test]
    fn zsh_targets_zprofile_and_zshrc() {
        let t = profile_targets(Path::new("/home/u"), ShellKind::Zsh);
        assert_eq!(t[0].0, PathBuf::from("/home/u/.zprofile"));
        assert_eq!(t[1].1, Placement::AppendZsh);
    }

    #[test]
    fn bash_targets_profile_and_bashrc() {
        let t = profile_targets(Path::new("/home/u"), ShellKind::Bash);
        assert_eq!(t[0].0, PathBuf::from("/home/u/.profile"));
        assert_eq!(t[1].1, Placement::Prepend);
    }
}
