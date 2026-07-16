//! HTTP (remote, OAuth) MCP transport, wrapping rmcp behind a sync facade.
//!
//! Talks to remote Streamable-HTTP MCP servers (Notion, Sentry, GitHub, …)
//! using rmcp, which owns the full OAuth 2.1 + PKCE + Dynamic Client
//! Registration flow. This module plugs in the file-backed credential store
//! (`oauth.rs`), runs the interactive auth flow once (browser → loopback
//! callback), and holds the live `RunningService` so tool calls reuse it.
//!
//! The agent loop is synchronous, so async rmcp calls are bridged with
//! `tauri::async_runtime::handle().block_on(...)` — safe because the agent runs
//! on plain `std::thread`s, not inside an async context.

use std::time::Duration;

use rmcp::model::ClientInfo;
use rmcp::transport::auth::{AuthClient, AuthorizationManager, OAuthState};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::ServiceExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::oauth::{FileCredentialStore, REDIRECT_URI};
use super::{tool_to_value, McpServer, RmcpConnection};

/// Where a server's OAuth token file lives: `<config_dir>/mcp/<name>.token.json`.
fn token_path(config_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    super::oauth::path_for_dir(config_dir, name)
}

/// Rewrite an SSE-only endpoint URL to its Streamable-HTTP counterpart. Per MCP
/// convention these live at `…/sse` and `…/mcp` respectively; rmcp can't do SSE.
/// Must run before OAuth: the token is audience-bound to the URL it's minted
/// for, so a `/sse`-audience token would be rejected by `/mcp`. Idempotent.
pub fn normalize_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if let Some(prefix) = trimmed.strip_suffix("/sse") {
        let slash = if url.ends_with('/') { "/" } else { "" };
        format!("{prefix}/mcp{slash}")
    } else {
        url.to_string()
    }
}

/// If a connect error looks like a transport mismatch (404/405 from POSTing an
/// initialize to an SSE-only endpoint), append a hint pointing at the `/mcp` URL.
fn with_transport_hint(url: &str, err: &str) -> String {
    let looks_like_not_found =
        err.contains("404") || err.contains("405") || err.contains("Method Not Allowed");
    if looks_like_not_found && !url.ends_with("/mcp") {
        format!(
            "{err}\n\nThis URL may be an SSE-only endpoint, which Bit doesn’t support. \
             Try the Streamable-HTTP URL instead — usually the same path ending in `/mcp` \
             (e.g. https://mcp.example.com/mcp)."
        )
    } else {
        err.to_string()
    }
}

/// Loopback port the OAuth callback server binds. Must match REDIRECT_URI.
const CALLBACK_PORT: u16 = 8473;

/// How long to wait for the user to finish signing in in their browser before
/// giving up. Generous: DCR + consent screens can take a while.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Connect to a remote (HTTP) MCP server using stored OAuth credentials.
/// `config_dir` is where the `mcp/<name>.token.json` token file lives.
pub fn connect(config_dir: &std::path::Path, server: &McpServer) -> Result<RmcpConnection, String> {
    let path = token_path(config_dir, &server.name);
    if !path.exists() {
        return Err("not authenticated — use “Add a service” in Settings first".into());
    }

    let url = normalize_url(&server.url);
    tauri::async_runtime::handle().block_on(async move {
        let mut mgr = AuthorizationManager::new(&url)
            .await
            .map_err(|e| format!("auth setup failed: {e}"))?;
        mgr.set_credential_store(FileCredentialStore::new(path));
        let have_creds = mgr
            .initialize_from_store()
            .await
            .map_err(|e| format!("credential load failed: {e}"))?;
        if !have_creds {
            return Err("not authenticated — use “Add a service” in Settings first".to_string());
        }
        if let Err(e) = mgr.discover_metadata().await {
            eprintln!(
                "[bit] mcp http {}: metadata discovery failed ({e}), refresh may fail",
                server.name
            );
        }
        let transport = StreamableHttpClientTransport::with_client(
            AuthClient::new(reqwest::Client::default(), mgr),
            StreamableHttpClientTransportConfig::with_uri(url.clone()),
        );
        let svc = ClientInfo::default()
            .serve(transport)
            .await
            .map_err(|e| with_transport_hint(&url, &format!("connect failed: {e}")))?;
        let tools = svc
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| format!("tools/list failed: {e}"))?;
        Ok(RmcpConnection::new(
            svc,
            tools.iter().map(tool_to_value).collect(),
        ))
    })
}

/// Run the interactive OAuth flow and persist the resulting token:
/// discover → DCR-register → browser → loopback callback → exchange.
///
/// Returns the URL actually used (may differ from the input if `normalize_url`
/// rewrote `/sse` → `/mcp`); the caller saves it so the token's audience matches.
pub fn run_oauth_flow(
    config_dir: &std::path::Path,
    name: &str,
    url: &str,
) -> Result<String, String> {
    let path = token_path(config_dir, name);
    let url = normalize_url(url);

    tauri::async_runtime::handle().block_on(async move {
        let mut state = OAuthState::new(&url, None)
            .await
            .map_err(|e| format!("auth init failed: {e}"))?;

        // Plug in our file store before starting, so the token exchange
        // writes straight to disk.
        if let OAuthState::Unauthorized(ref mut mgr) = state {
            mgr.set_credential_store(FileCredentialStore::new(path));
        }

        // Empty scopes ⇒ let the SDK auto-select from the server's advertised scopes.
        state
            .start_authorization(&[], REDIRECT_URI, Some("Bit"))
            .await
            .map_err(|e| format!("authorization start failed: {e}"))?;

        let auth_url = match &state {
            OAuthState::Session(s) => s.get_authorization_url().to_string(),
            _ => return Err("unexpected OAuth state after start_authorization".into()),
        };

        // Open the browser at the consent URL, then await the callback.
        open_browser(&auth_url);
        let (code, csrf) = wait_for_callback()
            .await
            .map_err(|e| format!("authorization callback failed: {e}"))?;

        // Exchange the code; FileCredentialStore::save persists the token atomically.
        if let OAuthState::Session(s) = &state {
            s.handle_callback(&code, &csrf)
                .await
                .map_err(|e| format!("token exchange failed: {e}"))?;
        }

        Ok(url)
    })
}

/// Open `url` in the user's default browser.
fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .spawn()
        .map_err(|e| eprintln!("[bit] couldn't open browser: {e}"));
}

/// Bind the loopback callback listener. tokio's `TcpListener::bind` sets
/// `SO_REUSEADDR` on Unix, so an interrupted OAuth flow (socket lingering in
/// TIME_WAIT) won't make the next bind fail with "Address already in use".
async fn bind_loopback(port: u16) -> Result<TcpListener, String> {
    TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        // AddrInUse is most often a stale Bit process holding the port.
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!(
                "Bit's sign-in callback port {port} is already in use — likely a \
                     stale Bit process. Quit Bit fully (tray → Quit Bit) and try again. \
                     (Underlying error: {e})"
            )
        } else {
            format!("couldn't bind callback port {port}: {e}")
        }
    })
}

/// Minimal loopback HTTP server: wait for the OAuth `redirect_uri` callback
/// (`GET /callback?code=…&state=…`) and return the code + CSRF state. Times
/// out after CALLBACK_TIMEOUT so an abandoned sign-in doesn't hang forever.
async fn wait_for_callback() -> Result<(String, String), String> {
    let listener = bind_loopback(CALLBACK_PORT).await?;

    let inner = async {
        loop {
            let (mut sock, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let mut reader = tokio::io::BufReader::new(&mut sock);
            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .await
                .map_err(|e| e.to_string())?;

            // Parse "GET /callback?code=…&state=… HTTP/1.1".
            let query = request_line
                .split_whitespace()
                .nth(1)
                .and_then(|path| path.split_once('?').map(|(_, q)| q));

            // Always respond so the browser tab resolves, even on a bad request.
            let body = if query.is_some() {
                "<!doctype html><meta charset=utf-8><title>Bit</title>\
                 <style>body{font:14px -apple-system,sans-serif;text-align:center;padding:60px}</style>\
                 <h2>✓ Connected</h2><p>You can close this tab and return to Bit.</p>"
            } else {
                "<!doctype html><meta charset=utf-8><title>Bit</title>\
                 <h2>Waiting…</h2>"
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Connection: close\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;

            if let Some(q) = query {
                let mut code = None;
                let mut state = None;
                // Parse the querystring via the `url` crate (already a dep via
                // reqwest) instead of hand-rolling percent-decoding + splitting.
                if let Ok(u) = url::Url::parse(&format!("http://x/?{q}")) {
                    for (k, v) in u.query_pairs() {
                        match k.as_ref() {
                            "code" => code = Some(v.into_owned()),
                            "state" => state = Some(v.into_owned()),
                            _ => {}
                        }
                    }
                }
                if let (Some(c), Some(s)) = (code, state) {
                    return Ok((c, s));
                }
            }
            // No usable code on this request; loop and accept the next.
        }
    };

    tokio::time::timeout(CALLBACK_TIMEOUT, inner)
        .await
        .map_err(|_| {
            format!(
                "timed out after {}s waiting for sign-in",
                CALLBACK_TIMEOUT.as_secs()
            )
        })?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The interrupted-OAuth-flow mitigation is SO_REUSEADDR on the loopback
    /// socket, which tokio's `TcpListener::bind` sets on Unix by default. Bind on
    /// an ephemeral port (0) to verify the loopback bind path works without a
    /// fixed-port collision.
    #[test]
    fn loopback_bind_succeeds() {
        let listener = tauri::async_runtime::block_on(async { bind_loopback(0).await })
            .expect("bind should succeed");
        // Bound to loopback, not 0.0.0.0.
        assert!(listener.local_addr().unwrap().ip().is_loopback());
    }

    #[test]
    fn normalize_url_rewrites_sse_to_mcp() {
        // The Cloudflare case: docs advertise /sse, Bit needs /mcp.
        assert_eq!(
            normalize_url("https://mcp.cloudflare.com/sse"),
            "https://mcp.cloudflare.com/mcp"
        );
        // Trailing slash handled too.
        assert_eq!(
            normalize_url("https://mcp.cloudflare.com/sse/"),
            "https://mcp.cloudflare.com/mcp/"
        );
    }

    #[test]
    fn normalize_url_is_idempotent_and_passthrough() {
        // Already /mcp → unchanged (so connect after a saved /mcp is a no-op).
        assert_eq!(
            normalize_url("https://mcp.notion.com/mcp"),
            "https://mcp.notion.com/mcp"
        );
        // A path-less or non-sse URL passes through untouched.
        assert_eq!(normalize_url("https://example.com"), "https://example.com");
        assert_eq!(
            normalize_url("https://example.com/api/v1/mcp"),
            "https://example.com/api/v1/mcp"
        );
        // Don’t touch /sse in the MIDDLE of a path, only as the terminal segment.
        assert_eq!(
            normalize_url("https://example.com/sse/things"),
            "https://example.com/sse/things"
        );
    }

    #[test]
    fn transport_hint_adds_guidance_on_404() {
        let with = with_transport_hint(
            "https://mcp.example.com/sse",
            "connect failed: HTTP 404 Not Found",
        );
        assert!(with.contains("Streamable-HTTP"), "hint should mention /mcp");
        // No hint when the URL is already /mcp (so it’s not a transport mismatch).
        let without = with_transport_hint(
            "https://mcp.example.com/mcp",
            "connect failed: HTTP 404 Not Found",
        );
        assert_eq!(without, "connect failed: HTTP 404 Not Found");
        // No hint on unrelated errors.
        let auth = with_transport_hint("https://mcp.example.com/sse", "Auth required");
        assert_eq!(auth, "Auth required");
    }
}
