//! OAuth 2.1 authorization-code flow for protected MCP HTTP servers.
//!
//! This module deliberately owns the whole browser flow so bearer tokens do
//! not pass through the normal command-output, stats or recovery paths.

use crate::core::oauth::{self, TokenSet};
use base64::Engine;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

const MAX_METADATA_BYTES: usize = 1024 * 1024;
const CALLBACK_LIMIT: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
struct ProtectedResource {
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AuthorizationServer {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    #[serde(default)]
    client_id_metadata_document_supported: bool,
}

#[derive(Debug, Deserialize)]
struct Registration {
    client_id: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default = "default_token_type")]
    token_type: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

fn default_token_type() -> String {
    "Bearer".into()
}

/// Authorizes an MCP resource and stores the resulting token securely.
pub fn authorize(
    client: &Client,
    resource: &str,
    challenge: Option<&str>,
) -> Result<TokenSet, String> {
    let resource_url = Url::parse(resource).map_err(|e| format!("invalid MCP URL: {e}"))?;
    let metadata_url = challenge
        .and_then(resource_metadata_url)
        .or_else(|| well_known(&resource_url, ".well-known/oauth-protected-resource"))
        .ok_or_else(|| "MCP OAuth resource metadata URL is unavailable".to_string())?;
    let protected: ProtectedResource = get_json(client, &metadata_url)?;
    let issuer = protected
        .authorization_servers
        .first()
        .ok_or_else(|| "MCP OAuth metadata listed no authorization server".to_string())?;
    let issuer_url = Url::parse(issuer).map_err(|e| format!("invalid OAuth issuer: {e}"))?;
    require_secure_oauth_url(&issuer_url)?;
    let server_url = well_known(&issuer_url, ".well-known/oauth-authorization-server")
        .ok_or_else(|| "OAuth issuer cannot form a metadata URL".to_string())?;
    let server: AuthorizationServer = get_json(client, &server_url)?;
    if server.issuer != *issuer {
        return Err("OAuth issuer does not match the discovered authorization server".into());
    }
    let authorization_endpoint = Url::parse(&server.authorization_endpoint)
        .map_err(|e| format!("invalid OAuth authorization endpoint: {e}"))?;
    let token_endpoint = Url::parse(&server.token_endpoint)
        .map_err(|e| format!("invalid OAuth token endpoint: {e}"))?;
    require_secure_oauth_url(&authorization_endpoint)?;
    require_secure_oauth_url(&token_endpoint)?;

    if let Ok(Some(tokens)) = oauth::load(resource, issuer) {
        if !tokens.expired(now_seconds()) {
            return Ok(tokens);
        }
        if let Some(refresh_token) = &tokens.refresh_token
            && let Ok(response) = client
                .post(token_endpoint.clone())
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                ])
                .send()
                .map_err(|e| e.to_string())
                .and_then(parse_response::<TokenResponse>)
        {
            let renewed = TokenSet {
                access_token: response.access_token,
                token_type: response.token_type,
                refresh_token: response
                    .refresh_token
                    .or_else(|| tokens.refresh_token.clone()),
                expires_at: response
                    .expires_in
                    .map(|seconds| now_seconds().saturating_add(seconds)),
                issuer: issuer.clone(),
                resource: resource.to_string(),
            };
            oauth::save(&renewed)
                .map_err(|e| format!("could not store renewed OAuth token: {e}"))?;
            return Ok(renewed);
        }
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("cannot open the local OAuth callback: {e}"))?;
    listener
        .set_nonblocking(false)
        .map_err(|e| format!("cannot configure the OAuth callback: {e}"))?;
    let callback = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr().map_err(|e| e.to_string())?.port()
    );
    let client_id = if server.client_id_metadata_document_supported {
        // CIMD is preferred by current MCP servers. A stable public metadata
        // document is not shipped by this local binary yet, so use DCR when
        // the server offers it and fail clearly otherwise.
        register_client(client, &server, &callback)?.unwrap_or_default()
    } else {
        register_client(client, &server, &callback)?
            .ok_or_else(|| "OAuth server requires a pre-registered client".to_string())?
    };
    if client_id.is_empty() {
        return Err(
            "OAuth server requires a client metadata document; this build has no public client URL"
                .into(),
        );
    }

    let verifier = random_string(32)?;
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let state = random_string(24)?;
    let redirect = Url::parse(&callback).map_err(|e| e.to_string())?;
    let mut auth = authorization_endpoint;
    {
        let mut query = auth.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", &client_id);
        query.append_pair("redirect_uri", redirect.as_str());
        query.append_pair("code_challenge", &challenge);
        query.append_pair("code_challenge_method", "S256");
        query.append_pair("state", &state);
        query.append_pair("resource", resource);
        if !protected.scopes_supported.is_empty() {
            query.append_pair("scope", &protected.scopes_supported.join(" "));
        }
    }
    open_browser(auth.as_str())?;
    let (code, returned_state) = receive_callback(listener)?;
    if returned_state != state {
        return Err("OAuth callback state did not match".into());
    }
    let response: TokenResponse = client
        .post(token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", callback.as_str()),
            ("client_id", client_id.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .map_err(|e| format!("OAuth token request failed: {e}"))
        .and_then(parse_response::<TokenResponse>)?;
    let tokens = TokenSet {
        access_token: response.access_token,
        token_type: response.token_type,
        refresh_token: response.refresh_token,
        expires_at: response
            .expires_in
            .map(|seconds| now_seconds().saturating_add(seconds)),
        issuer: issuer.clone(),
        resource: resource.to_string(),
    };
    oauth::save(&tokens).map_err(|e| format!("could not store OAuth token securely: {e}"))?;
    Ok(tokens)
}

fn register_client(
    client: &Client,
    server: &AuthorizationServer,
    callback: &str,
) -> Result<Option<String>, String> {
    let Some(endpoint) = &server.registration_endpoint else {
        return Ok(None);
    };
    let endpoint =
        Url::parse(endpoint).map_err(|e| format!("invalid OAuth registration endpoint: {e}"))?;
    require_secure_oauth_url(&endpoint)?;
    let body = serde_json::to_vec(&serde_json::json!({
        "client_name": "Schliffe",
        "application_type": "native",
        "redirect_uris": [callback],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none"
    }))
    .map_err(|e| format!("OAuth registration request is invalid: {e}"))?;
    let response = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .map_err(|e| format!("OAuth client registration failed: {e}"))
        .and_then(parse_response::<Registration>)?;
    Ok(Some(response.client_id))
}

fn receive_callback(listener: TcpListener) -> Result<(String, String), String> {
    let (mut stream, _) = listener
        .accept()
        .map_err(|e| format!("OAuth callback failed: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    (&mut stream)
        .take((CALLBACK_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("OAuth callback read failed: {e}"))?;
    if bytes.len() > CALLBACK_LIMIT {
        return Err("OAuth callback was too large".into());
    }
    let request =
        String::from_utf8(bytes).map_err(|_| "OAuth callback was not UTF-8".to_string())?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|line| line.split_whitespace().next())
        .ok_or_else(|| "OAuth callback was not a GET request".to_string())?;
    let url = Url::parse(&format!("http://localhost{target}"))
        .map_err(|e| format!("OAuth callback URL is invalid: {e}"))?;
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nSchliffe authorization received. You can close this window.";
    let _ = stream.write_all(response);
    if let Some(error) = params.get("error") {
        return Err(format!("OAuth authorization failed: {error}"));
    }
    let code = params
        .get("code")
        .cloned()
        .ok_or_else(|| "OAuth callback did not contain a code".to_string())?;
    let state = params
        .get("state")
        .cloned()
        .ok_or_else(|| "OAuth callback did not contain state".to_string())?;
    Ok((code, state))
}

fn parse_response<T: DeserializeOwned>(response: reqwest::blocking::Response) -> Result<T, String> {
    let status = response.status();
    let body = response
        .bytes()
        .map_err(|e| format!("OAuth response could not be read: {e}"))?;
    if body.len() > MAX_METADATA_BYTES {
        return Err("OAuth response exceeded the size limit".into());
    }
    if !status.is_success() {
        return Err(format!("OAuth server returned HTTP {}", status.as_u16()));
    }
    serde_json::from_slice(&body).map_err(|e| format!("OAuth response was not valid JSON: {e}"))
}

fn get_json<T: for<'de> Deserialize<'de>>(client: &Client, url: &Url) -> Result<T, String> {
    let response = client
        .get(url.clone())
        .send()
        .map_err(|e| format!("OAuth discovery failed: {e}"))?;
    parse_response(response)
}

fn resource_metadata_url(challenge: &str) -> Option<Url> {
    challenge
        .split_whitespace()
        .find_map(|part| {
            part.strip_prefix("resource_metadata=\"")
                .and_then(|v| v.strip_suffix('"'))
        })
        .and_then(|value| Url::parse(value).ok())
}

fn well_known(base: &Url, path: &str) -> Option<Url> {
    let mut url = base.clone();
    url.set_path(&format!("/{path}"));
    url.set_query(None);
    Some(url)
}

fn require_secure_oauth_url(url: &Url) -> Result<(), String> {
    if url.scheme() == "https"
        || url.host_str() == Some("127.0.0.1")
        || url.host_str() == Some("localhost")
    {
        Ok(())
    } else {
        Err("OAuth endpoints must use HTTPS (or loopback for local development)".into())
    }
}

fn random_string(bytes: usize) -> Result<String, String> {
    let mut random = vec![0u8; bytes];
    getrandom::fill(&mut random).map_err(|e| format!("secure random generation failed: {e}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random))
}

fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    command
        .arg(url)
        .spawn()
        .map_err(|e| format!("could not open the OAuth browser: {e}"))?;
    Ok(())
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_resource_metadata_from_www_authenticate() {
        let header = r#"Bearer resource_metadata="https://mcp.example/.well-known/oauth-protected-resource" scope="read""#;
        assert_eq!(
            resource_metadata_url(header).unwrap().as_str(),
            "https://mcp.example/.well-known/oauth-protected-resource"
        );
    }

    #[test]
    fn rejects_plaintext_remote_oauth_endpoints() {
        assert!(
            require_secure_oauth_url(&Url::parse("http://auth.example/authorize").unwrap())
                .is_err()
        );
        assert!(
            require_secure_oauth_url(&Url::parse("http://127.0.0.1:9000/authorize").unwrap())
                .is_ok()
        );
        assert!(
            require_secure_oauth_url(&Url::parse("https://auth.example/authorize").unwrap())
                .is_ok()
        );
    }

    #[test]
    fn builds_well_known_url_without_leaking_query_parameters() {
        let base = Url::parse("https://auth.example/tenant?token=secret").unwrap();
        let metadata = well_known(&base, ".well-known/oauth-authorization-server").unwrap();
        assert_eq!(
            metadata.as_str(),
            "https://auth.example/.well-known/oauth-authorization-server"
        );
        assert!(metadata.query().is_none());
    }
}
