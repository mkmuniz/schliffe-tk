use crate::core::secure::{is_store_hash, write_private};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Local content-addressed store (specs.md §8) — infrastructure shared by
/// progressive disclosure (8.1), cache (8.2), and deduplication (8.3).
/// File-per-hash layout, no in-RAM index (decision recorded in specs §13,
/// 2026-07-26): each shim call only reads the one file for the hash it needs.
const MAX_AGE_DAYS: u64 = 14;
const SWEEP_SAMPLE_RATE: u64 = 50; // ~2% chance of sweeping on any given write

fn store_root() -> PathBuf {
    if let Ok(dir) = std::env::var("SCHLIFFE_STORE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".schliffe").join("store")
}

fn hash_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    // 16 hex chars (64 bits) — negligible collision risk for the volume
    // estimated in specs §8.5 (~100 entries/day), short enough to quote in
    // text (e.g. "schliffe show a3f9c2d1e8b04f77").
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Path of an entry. `key_hash` is always a value `hash_hex` produced, so
/// it can't contain a path separator — callers taking a hash from outside
/// must check it with `is_store_hash` first (see `get`).
fn sharded_path(subdir: &str, key_hash: &str) -> PathBuf {
    debug_assert!(is_store_hash(key_hash), "unchecked hash: {key_hash:?}");
    let shard = &key_hash[..2];
    store_root().join(subdir).join(shard).join(key_hash)
}

/// Every store write goes through this: owner-only (0600), atomic, and it
/// never follows a symlink planted at the destination. The store holds raw
/// command output, which can contain secrets.
fn write_file(path: &Path, content: &str) {
    let _ = write_private(path, content.as_bytes());
}

/// Content-addressed store (CAS) — the same content always produces the same
/// hash, writes are idempotent (business rule 5: deterministic). Used by
/// progressive disclosure (8.1) and as dedup's backing store (8.3).
pub fn put(content: &str) -> String {
    let hash = hash_hex(content.as_bytes());
    let path = sharded_path("cas", &hash);
    if !path.exists() {
        write_file(&path, content);
    }
    maybe_sweep();
    hash
}

/// Reads an entry by hash. The hash comes straight from the command line
/// (`schliffe show <hash>`), so it is validated before it reaches a path:
/// anything that isn't a real store hash is rejected, which is what keeps
/// `schliffe show ../../../../etc/passwd` from reading arbitrary files.
pub fn get(hash: &str) -> Option<String> {
    if !is_store_hash(hash) {
        return None;
    }
    fs::read_to_string(sharded_path("cas", hash)).ok()
}

/// Cache keyed by an arbitrary string (8.2) — unlike the CAS, the key is the
/// *command's* identifier (e.g. `"git-show:v1:<sha>"`), not a hash of the
/// content, because it needs to be queryable BEFORE the result is known.
pub fn get_keyed(key: &str) -> Option<String> {
    let path = sharded_path("keyed", &hash_hex(key.as_bytes()));
    fs::read_to_string(path).ok()
}

pub fn put_keyed(key: &str, content: &str) {
    let path = sharded_path("keyed", &hash_hex(key.as_bytes()));
    write_file(&path, content);
    maybe_sweep();
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Result of the deduplication check (8.3, "the only technique that actually
/// cuts tokens" — cache alone only buys performance).
pub enum Dedup {
    /// Already appeared within the window — the caller already has the hash
    /// (computed it before calling, so it can write to the CAS regardless of
    /// the result).
    SeenRecently,
    Fresh,
}

/// Checks whether `content` has already been shown within the sliding
/// window. Always records the current appearance, even when `Fresh`.
///
/// `session`: when the agent exposes a real session id (Claude Code sets
/// `CLAUDE_CODE_SESSION_ID`, found 2026-09-24), it's folded into the hash, so
/// an output seen in ANOTHER session never collapses into "same as previous
/// output" — the agent in this session never saw it. Without an id, falls
/// back to the time window alone (the original approximation, specs §8.3).
pub fn check_and_record_dedup(content: &str, window_secs: u64, session: Option<&str>) -> Dedup {
    let hash = match session {
        Some(id) => hash_hex(format!("{id}\0{content}").as_bytes()),
        None => hash_hex(content.as_bytes()),
    };
    let log_path = store_root().join("seen.log");
    let now = now_secs();

    let mut kept: Vec<(u64, String)> = fs::read_to_string(&log_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (ts, h) = line.split_once(' ')?;
            let ts: u64 = ts.parse().ok()?;
            (now.saturating_sub(ts) <= window_secs).then(|| (ts, h.to_string()))
        })
        .collect();

    let result = if kept.iter().any(|(_, h)| h == &hash) {
        Dedup::SeenRecently
    } else {
        Dedup::Fresh
    };

    kept.push((now, hash));
    let serialized: String = kept.iter().map(|(ts, h)| format!("{ts} {h}\n")).collect();
    write_file(&log_path, &serialized);

    result
}

/// Lazy sweep (specs §13, cleanup policy decided 2026-07-26): with no
/// daemon, each write has a small chance of triggering a sweep instead of
/// running one every time (unnecessary I/O cost for the volume estimated in
/// specs §8.5).
fn maybe_sweep() {
    if now_secs().is_multiple_of(SWEEP_SAMPLE_RATE) {
        force_gc();
    }
}

/// Removes entries from `cas/` and `keyed/` older than `MAX_AGE_DAYS`.
/// Fail-open (business rule 3): an I/O error on one entry doesn't stop the
/// sweep of the rest.
pub fn force_gc() {
    let cutoff = now_secs().saturating_sub(MAX_AGE_DAYS * 24 * 60 * 60);
    for subdir in ["cas", "keyed"] {
        sweep_dir(&store_root().join(subdir), cutoff);
    }
}

fn sweep_dir(dir: &Path, cutoff: u64) {
    let Ok(shards) = fs::read_dir(dir) else {
        return;
    };
    for shard in shards.flatten() {
        let Ok(entries) = fs::read_dir(shard.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() < cutoff)
                .unwrap_or(false);
            if is_old {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

pub fn clear_all() -> std::io::Result<()> {
    let root = store_root();
    if root.exists() {
        fs::remove_dir_all(&root)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // $HOME is global to the process — store tests need to run serialized
    // with an isolated directory, otherwise they run in parallel and stomp
    // on the same real `~/.schliffe/store`.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn with_isolated_store<T>(f: impl FnOnce() -> T) -> T {
        let _guard = TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("schliffe-store-test-{}", now_secs()));
        // SAFETY: TEST_LOCK serializes every store test, so no other
        // thread reads the environment while it changes.
        unsafe {
            std::env::set_var("SCHLIFFE_STORE_DIR", &dir);
        }
        let result = f();
        let _ = fs::remove_dir_all(&dir);
        // SAFETY: still inside the TEST_LOCK critical section.
        unsafe {
            std::env::remove_var("SCHLIFFE_STORE_DIR");
        }
        result
    }

    #[test]
    fn put_get_roundtrip() {
        with_isolated_store(|| {
            let hash = put("test content");
            assert_eq!(get(&hash), Some("test content".to_string()));
        });
    }

    #[test]
    fn put_is_idempotent_same_hash() {
        with_isolated_store(|| {
            let h1 = put("same");
            let h2 = put("same");
            assert_eq!(h1, h2);
        });
    }

    #[test]
    fn keyed_cache_roundtrip() {
        with_isolated_store(|| {
            assert!(get_keyed("git-show:v1:abc123").is_none());
            put_keyed("git-show:v1:abc123", "cached output");
            assert_eq!(
                get_keyed("git-show:v1:abc123"),
                Some("cached output".to_string())
            );
        });
    }

    #[test]
    fn dedup_detects_repeat_within_window() {
        with_isolated_store(|| {
            assert!(matches!(
                check_and_record_dedup("output X", 1800, None),
                Dedup::Fresh
            ));
            assert!(matches!(
                check_and_record_dedup("output X", 1800, None),
                Dedup::SeenRecently
            ));
        });
    }

    #[test]
    fn dedup_ignores_entries_outside_window() {
        with_isolated_store(|| {
            check_and_record_dedup("output Y", 0, None); // zero window: expires immediately
            std::thread::sleep(std::time::Duration::from_secs(1));
            assert!(matches!(
                check_and_record_dedup("output Y", 0, None),
                Dedup::Fresh
            ));
        });
    }

    #[test]
    fn dedup_is_scoped_to_the_session() {
        with_isolated_store(|| {
            check_and_record_dedup("output Z", 1800, Some("session-a"));
            // Another session never saw it — must not collapse.
            assert!(matches!(
                check_and_record_dedup("output Z", 1800, Some("session-b")),
                Dedup::Fresh
            ));
            assert!(matches!(
                check_and_record_dedup("output Z", 1800, Some("session-a")),
                Dedup::SeenRecently
            ));
        });
    }
}
