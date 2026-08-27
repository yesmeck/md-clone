//! OAuth login for Notion public integrations, plus the credential store
//! that `sync` falls back to when no token is passed explicitly.
//!
//! Notion only supports the authorization-code flow (no device flow, no
//! PKCE-only public clients), so the user supplies their own integration's
//! client ID and secret; the CLI runs a localhost redirect server, opens the
//! browser, and exchanges the code for a long-lived access token.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const AUTHORIZE_URL: &str = "https://api.notion.com/v1/oauth/authorize";
const TOKEN_URL: &str = "https://api.notion.com/v1/oauth/token";
/// How long to wait for the user to finish the browser flow.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    #[serde(default)]
    pub workspace_name: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

pub fn credentials_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("cannot determine home directory")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("md2notion")
        .join("credentials.json"))
}

pub fn load_credentials() -> Result<Option<Credentials>> {
    let path = credentials_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let creds =
        serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(creds))
}

fn save_credentials(creds: &Credentials) -> Result<PathBuf> {
    let path = credentials_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    let mut data = serde_json::to_string_pretty(creds)?;
    data.push('\n');
    std::fs::write(&path, data).with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// Remove stored credentials; returns whether any existed.
pub fn logout() -> Result<bool> {
    let path = credentials_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub async fn login(client_id: &str, client_secret: &str, port: u16) -> Result<()> {
    // Bind before opening the browser so the redirect can't race us.
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("could not listen on localhost:{port} — is it in use? (--port picks another; remember to register the matching redirect URI)"))?;
    let redirect_uri = format!("http://localhost:{port}/callback");
    let state = random_hex(16)?;

    let auth_url = format!(
        "{AUTHORIZE_URL}?client_id={}&response_type=code&owner=user&redirect_uri={}&state={}",
        percent_encode(client_id),
        percent_encode(&redirect_uri),
        state,
    );
    println!("Opening your browser to authorize md2notion...");
    println!("If it does not open, visit:\n\n  {auth_url}\n");
    println!("(your integration must list {redirect_uri} as a redirect URI)");
    let _ = open_browser(&auth_url);

    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(&listener, &state))
        .await
        .context("timed out waiting for the browser authorization")??;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let resp = http
        .post(TOKEN_URL)
        .basic_auth(client_id, Some(client_secret))
        .json(&json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": redirect_uri,
        }))
        .send()
        .await
        .context("token exchange request failed")?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or_default();
    if !status.is_success() {
        let msg = body["error_description"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .unwrap_or("unknown error");
        bail!("token exchange failed ({status}): {msg}");
    }

    let creds = Credentials {
        access_token: body["access_token"]
            .as_str()
            .context("token response missing access_token")?
            .to_string(),
        workspace_name: body["workspace_name"].as_str().map(String::from),
        workspace_id: body["workspace_id"].as_str().map(String::from),
    };
    let workspace = creds.workspace_name.clone().unwrap_or_else(|| "unknown".into());
    let path = save_credentials(&creds)?;
    println!("Logged in to workspace {workspace:?}.");
    println!("Credentials saved to {} — `md2notion sync` will use them when no --token/NOTION_TOKEN is given.", path.display());
    Ok(())
}

/// Accept connections until the OAuth callback arrives; other requests
/// (favicon etc.) get a 404 and the loop continues.
async fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await.context("accept failed")?;
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);
        let Some(path) = request.split_whitespace().nth(1) else {
            continue;
        };
        if !path.starts_with("/callback") {
            let _ = respond(&mut stream, 404, "Not found").await;
            continue;
        }
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let params = parse_query(query);
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

        if let Some(err) = get("error") {
            let _ = respond(&mut stream, 200, "Authorization was denied. You can close this tab.").await;
            bail!("authorization denied: {err}");
        }
        if get("state").as_deref() != Some(expected_state) {
            let _ = respond(&mut stream, 400, "State mismatch — please retry the login.").await;
            bail!("OAuth state mismatch — possible CSRF or a stale login tab; run login again");
        }
        let Some(code) = get("code") else {
            let _ = respond(&mut stream, 400, "Missing authorization code.").await;
            bail!("callback did not include an authorization code");
        };
        let _ = respond(
            &mut stream,
            200,
            "md2notion is authorized. You can close this tab and return to the terminal.",
        )
        .await;
        return Ok(code);
    }
}

async fn respond(stream: &mut TcpStream, status: u16, message: &str) -> Result<()> {
    let reason = if status == 200 { "OK" } else { "Error" };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>md2notion</title>\
         <body style=\"font-family: sans-serif; margin: 4rem\"><p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let mut it = pair.splitn(2, '=');
            (
                percent_decode(it.next().unwrap_or("")),
                percent_decode(it.next().unwrap_or("")),
            )
        })
        .collect()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).context("failed to gather randomness")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_roundtrip() {
        let uri = "http://localhost:8237/callback";
        assert_eq!(percent_encode(uri), "http%3A%2F%2Flocalhost%3A8237%2Fcallback");
        assert_eq!(percent_decode(&percent_encode(uri)), uri);
    }

    #[test]
    fn query_parsing() {
        let params = parse_query("code=abc%2F123&state=xyz&empty=");
        assert_eq!(params[0], ("code".into(), "abc/123".into()));
        assert_eq!(params[1], ("state".into(), "xyz".into()));
        assert_eq!(params[2], ("empty".into(), "".into()));
    }

    #[test]
    fn random_hex_has_expected_length_and_charset() {
        let s = random_hex(16).unwrap();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
