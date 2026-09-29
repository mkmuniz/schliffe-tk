//! Security primitives shared by every module that touches disk or reads
//! untrusted input (2026-09-28 hardening pass — see `SECURITY.md`).
//!
//! Threat model in one line: Schliffe runs on the developer's machine, in
//! front of every command an AI agent runs, and stores what those commands
//! printed. So it handles two kinds of hostile input — **content** (command
//! output, MCP results, pasted logs, images: all attacker-influenceable if
//! the agent works on an untrusted repo or talks to an untrusted server)
//! and **arguments** (what the agent was talked into running, e.g. through
//! prompt injection). Neither may turn into a file read outside the store,
//! a write outside it, unbounded memory, or a crash.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

/// Hard cap on anything read from a pipe (hook stdin, MCP protocol lines).
/// A hostile MCP server or a runaway tool must not be able to make Schliffe
/// allocate without bound; past this the input is refused and the original
/// content is left untouched (fail-open, business rule 3).
pub const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;

/// Whether `s` is exactly a store hash as [`crate::core::store`] produces
/// them: 16 lowercase hex characters.
///
/// The one input the user (or an agent acting on injected instructions)
/// passes straight to a filesystem path is `schliffe show <hash>`. Without
/// this check, `schliffe show ../../../../etc/passwd` read arbitrary files
/// — and Schliffe's own recovery hints (`schliffe show <hash>`) sit in text
/// the model reads, so a crafted log or commit message could have talked
/// the agent into running one. Verified exploitable before this landed.
pub fn is_store_hash(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Creates `path` (and its parents) and makes every directory from
/// Schliffe's root down owner-only (0700), so a second account on the
/// machine can't read what commands printed.
pub fn create_dir_private(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        // Tighten this directory and its ancestors up to (and including)
        // the Schliffe root — this also repairs installs created before
        // the hardening pass, without a recursive walk on every write.
        let root = root_dir();
        let mut current = Some(path);
        while let Some(dir) = current {
            set_mode(dir, 0o700);
            if root.as_deref() == Some(dir) {
                break;
            }
            current = dir
                .parent()
                .filter(|p| root.as_deref().is_some_and(|r| p.starts_with(r) || *p == r));
        }
    }
    Ok(())
}

/// `~/.schliffe` — the only tree Schliffe writes to by default.
fn root_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".schliffe"))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

/// Writes `content` so that only the owner can read it, atomically, without
/// ever following a symlink at the destination.
///
/// Why it matters: the store holds **raw command output** — a `git diff`
/// that touches a `.env`, a `printenv`, a stack trace with a connection
/// string. Before this, those files were created 0644 (any local account
/// could read them). The temp-file-plus-rename also means a symlink planted
/// at the destination (`store/cas/ab/abcd…` → `~/.ssh/authorized_keys`) is
/// replaced rather than written through.
pub fn write_private(path: &Path, content: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        create_dir_private(dir)?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("schliffe"),
        std::process::id()
    ));
    {
        let mut file = create_new_private(&tmp)?;
        file.write_all(content)?;
        file.sync_all()?;
    }
    // Atomic: readers see either the old file or the whole new one, and a
    // symlink sitting at `path` is replaced, not followed.
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Creates a fresh 0600 file, failing rather than following an existing
/// symlink (`create_new` implies `O_EXCL`, which refuses symlinks).
fn create_new_private(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            // Leftover from a killed process (or a planted symlink):
            // remove and retry once. `remove_file` unlinks the symlink
            // itself, never its target.
            fs::remove_file(path)?;
            opts.open(path)
        }
        other => other,
    }
}

/// Opens an append-only, owner-only file (the stats log).
pub fn open_append_private(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        create_dir_private(dir)?;
    }
    let existed = path.symlink_metadata().is_ok();
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    // A file created before the hardening pass keeps its old mode, and
    // `mode()` only applies at creation — tighten it either way.
    #[cfg(unix)]
    if existed {
        set_mode(path, 0o600);
    }
    Ok(file)
}

/// Reads at most `MAX_INPUT_BYTES`, returning `None` when the input is
/// larger (rather than allocating whatever the other side sends).
pub fn read_limited(reader: impl Read) -> Option<String> {
    let mut buf = Vec::new();
    let mut limited = reader.take(MAX_INPUT_BYTES as u64 + 1);
    limited.read_to_end(&mut buf).ok()?;
    if buf.len() > MAX_INPUT_BYTES {
        return None;
    }
    String::from_utf8(buf).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("schliffe-sec-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn only_real_store_hashes_are_accepted() {
        assert!(is_store_hash("a3f9c2d1e8b04f77"));
        for bad in [
            "../../../../etc/passwd",
            "..",
            "a3f9c2d1e8b04f7",   // 15
            "a3f9c2d1e8b04f770", // 17
            "A3F9C2D1E8B04F77",  // uppercase
            "a3f9c2d1e8b04f7g",  // non-hex
            "€x",                // multi-byte (used to panic on slicing)
            "a3f9c2d1/../../x",
            "",
        ] {
            assert!(!is_store_hash(bad), "accepted {bad:?}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn written_files_are_owner_only_and_dirs_too() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("perms");
        let f = dir.join("sub").join("secret");
        write_private(&f, b"AWS_SECRET=...").unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "AWS_SECRET=...");
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(dir.join("sub")).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // No temp file left behind.
        let leftovers: Vec<_> = fs::read_dir(dir.join("sub"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_at_the_destination_is_replaced_not_followed() {
        let dir = tmp("symlink");
        let victim = dir.join("authorized_keys");
        fs::write(&victim, "original").unwrap();
        let planted = dir.join("entry");
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        write_private(&planted, b"command output").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "original");
        assert_eq!(fs::read_to_string(&planted).unwrap(), "command output");
        assert!(
            !fs::symlink_metadata(&planted)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_input_is_refused() {
        let big = vec![b'x'; MAX_INPUT_BYTES + 1];
        assert!(read_limited(&big[..]).is_none());
        assert_eq!(read_limited(&b"ok"[..]).as_deref(), Some("ok"));
    }
}
