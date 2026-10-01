//! Local OAuth token storage for remote MCP connections.
//!
//! Tokens are credentials, not application data: they never go through the
//! normal content store, are keyed by the exact resource and issuer, and are
//! written with the same owner-only atomic primitive used for other secrets.
//!
//! The transport integration lands in the next M15 change; keep this module
//! warning-free while that flow is built in small, reviewable steps.

#![allow(dead_code)]

use crate::core::secure::{read_limited, write_private};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_TOKEN_FILE_BYTES: usize = 16 * 1024;
const EXPIRY_SKEW_SECONDS: u64 = 60;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TokenSet {
    pub access_token: String,
    pub token_type: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<u64>,
    pub issuer: String,
    pub resource: String,
}

impl TokenSet {
    pub fn expired(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|expires_at| now.saturating_add(EXPIRY_SKEW_SECONDS) >= expires_at)
    }
}

/// Loads credentials for one exact `(resource, issuer)` pair.
pub fn load(resource: &str, issuer: &str) -> io::Result<Option<TokenSet>> {
    let path = token_path(resource, issuer)?;
    let Some(file) = open_private_read(&path)? else {
        return Ok(None);
    };
    let Some(json) = read_limited(file) else {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "Schliffe OAuth token file is too large or not UTF-8",
        ));
    };
    let tokens: TokenSet = serde_json::from_str(&json).map_err(|e| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("Schliffe OAuth token file is invalid: {e}"),
        )
    })?;
    validate(&tokens, resource, issuer)?;
    Ok(Some(tokens))
}

/// Stores credentials without exposing them to the content-addressed store,
/// stats, reports, logs or recovery hints.
pub fn save(tokens: &TokenSet) -> io::Result<()> {
    validate(tokens, &tokens.resource, &tokens.issuer)?;
    let path = token_path(&tokens.resource, &tokens.issuer)?;
    let json = serde_json::to_vec(tokens).map_err(io::Error::other)?;
    if json.len() > MAX_TOKEN_FILE_BYTES {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "Schliffe OAuth token set is too large",
        ));
    }
    write_private(&path, &json)
}

pub fn remove(resource: &str, issuer: &str) -> io::Result<()> {
    let path = token_path(resource, issuer)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn validate(tokens: &TokenSet, resource: &str, issuer: &str) -> io::Result<()> {
    if tokens.resource != resource || tokens.issuer != issuer {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "Schliffe OAuth token is bound to a different server",
        ));
    }
    for value in [
        &tokens.access_token,
        &tokens.token_type,
        &tokens.issuer,
        &tokens.resource,
    ] {
        if value.is_empty() || value.chars().any(char::is_control) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "Schliffe OAuth token metadata is invalid",
            ));
        }
    }
    if let Some(refresh) = &tokens.refresh_token
        && refresh.chars().any(char::is_control)
    {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "Schliffe OAuth refresh token is invalid",
        ));
    }
    Ok(())
}

fn token_path(resource: &str, issuer: &str) -> io::Result<PathBuf> {
    if resource.is_empty() || issuer.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "Schliffe OAuth resource and issuer are required",
        ));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home)
        .join(".schliffe")
        .join("oauth")
        .join(format!("{}.json", key(resource, issuer))))
}

fn key(resource: &str, issuer: &str) -> String {
    let digest = Sha256::digest(format!("{issuer}\0{resource}").as_bytes());
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn open_private_read(path: &Path) -> io::Result<Option<File>> {
    match File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[allow(dead_code)]
fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TokenSet {
        TokenSet {
            access_token: "access-secret".into(),
            token_type: "Bearer".into(),
            refresh_token: Some("refresh-secret".into()),
            expires_at: Some(2_000),
            issuer: "https://auth.example.test".into(),
            resource: "https://mcp.example.test/mcp".into(),
        }
    }

    #[test]
    fn expiry_has_a_small_renewal_window() {
        let tokens = sample();
        assert!(!tokens.expired(1_900));
        assert!(tokens.expired(1_940));
    }

    #[test]
    fn key_is_stable_and_binds_both_endpoints() {
        assert_eq!(key("resource", "issuer"), key("resource", "issuer"));
        assert_ne!(key("resource-a", "issuer"), key("resource-b", "issuer"));
        assert_ne!(key("resource", "issuer-a"), key("resource", "issuer-b"));
    }

    #[test]
    fn mismatched_metadata_is_rejected() {
        let tokens = sample();
        let error = validate(&tokens, "https://other.example.test/mcp", &tokens.issuer)
            .expect_err("resource binding must be enforced");
        assert_eq!(error.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn control_characters_are_rejected() {
        let mut tokens = sample();
        tokens.access_token.push('\n');
        assert!(validate(&tokens, &tokens.resource, &tokens.issuer).is_err());
    }
}
