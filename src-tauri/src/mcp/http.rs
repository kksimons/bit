//! HTTP (remote, OAuth) MCP transport, wrapping rmcp behind a sync facade.
//!
//! Where `mcp.rs`'s stdio client speaks newline JSON-RPC to a local subprocess,
//! this module talks to remote Streamable-HTTP MCP servers (Notion, Sentry,
//! GitHub, …) using the rmcp SDK, which owns the full OAuth 2.1 + PKCE + Dynamic
//! Client Registration flow. Our job is: (1) plug in the file-backed credential
//! store (`oauth.rs`), (2) run the interactive auth flow once (browser → loopback
//! callback), (3) hold the live `RunningService` so tool calls reuse it.
//!
//! The agent loop is synchronous, so async rmcp calls are bridged with
//! `tauri::async_runtime::handle().block_on(...)`. That's safe because the agent
//! runs on plain `std::thread`s, not inside an async context.

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, ClientInfo};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::auth::{AuthClient, AuthorizationManager, OAuthState};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use super::oauth::{FileCredentialStore, REDIRECT_URI};
use super::{flatten_mcp_content, McpServer};

/// Where a server's OAuth token file lives: `<config_dir>/mcp/<name>.token.json`.
/// Mirrors `FileCredentialStore::path_for` but takes a plain path (no AppHandle)
/// so the OAuth layer stays decoupled from Tauri and is testable.
fn token_path(config_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    config_dir
        .join("mcp")
        .join(format!("{}.token.json", super::oauth::sanitize_name(name)))
}

/// Rewrite a known-incompatible transport URL to the Streamable-HTTP one Bit
/// speaks. Per MCP convention, an SSE-only endpoint lives at `…/sse` and its
/// Streamable-HTTP counterpart lives at `…/mcp`. Bit (via rmcp) can't do SSE at
/// all, so a `/sse` URL is unusable as-is; we transparently use `/mcp` instead.
///
/// This MUST run before OAuth: the token is audience-bound to the URL it's
/// minted for, so a `/sse`-audience token would be rejected by `/mcp` (we hit
/// exactly this during testing). Normalizing up front means OAuth mints a
/// `/mcp`-audience token that `connect` can use directly. Idempotent: a URL
/// already ending in `/mcp` (or anything else) is returned unchanged.
pub fn normalize_url(url: &str) -> String {
    // Strip a trailing slash so `/sse/` and `/sse` are handled the same.
    let trimmed = url.trim_end_matches('/');
    if let Some(prefix) = trimmed.strip_suffix("/sse") {
        // Preserve any trailing slash the user included for tidiness.
        let slash = if url.ends_with('/') { "/" } else { "" };
        format!("{prefix}/mcp{slash}")
    } else {
        url.to_string()
    }
}

/// If a connect error looks like a transport mismatch (404/405 from POSTing an
/// initialize to an endpoint that only speaks SSE), append a hint pointing the
/// user at the `/mcp` URL. Returns the (possibly annotated) error string.
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

/// The concrete rmcp client service type we hold between calls.
type ClientService = RunningService<RoleClient, ClientInfo>;

/// A live connection to a remote (HTTP) MCP server. Holds the rmcp
/// `RunningService` (so the session, auth, and transport persist) plus the
/// cached tool list in the same raw-Value shape the stdio path uses.
pub struct HttpConnection {
    service: Arc<Mutex<Option<ClientService>>>,
    tools: Vec<Value>,
}

impl HttpConnection {
    /// The cached tools (raw serde Values; namespaced later by `tool_defs`).
    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    /// Connect using stored credentials. Returns Err if the server hasn't been
    /// authenticated yet (no token file) or the token/transport is broken.
    /// `config_dir` is the app's config dir (where the `mcp/<name>.token.json`
    /// token file lives) — pass the same dir used for the OAuth flow.
    pub fn connect(config_dir: &std::path::Path, server: &McpServer) -> Result<Self, String> {
        let path = token_path(config_dir, &server.name);
        if !path.exists() {
            return Err("not authenticated — use “Add a service” in Settings first".into());
        }

        let url = normalize_url(&server.url);
        tauri::async_runtime::handle().block_on(async move {
            // Build an AuthorizationManager with our file-backed store, then
            // load any saved token. initialize_from_store configures the client
            // from the stored client_id/token and returns false if none exists.
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
            // Metadata is needed for token refresh when the access token expires.
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
            Ok(HttpConnection {
                service: Arc::new(Mutex::new(Some(svc))),
                tools: tools.iter().map(tool_to_value).collect(),
            })
        })
    }

    /// Call a server-side tool by name. Bridges to async; flattens the result
    /// text the same way the stdio path does.
    pub fn call(&mut self, tool: &str, args: &Value) -> Result<String, String> {
        let service = self.service.clone();
        let tool = tool.to_string();
        let args = args.clone();
        tauri::async_runtime::handle().block_on(async move {
            let mut guard = service.lock().await;
            let Some(svc) = guard.as_mut() else {
                return Err("connection closed".to_string());
            };
            let result = svc
                .peer()
                .call_tool({
                    let mut p = CallToolRequestParams::new(tool.clone());
                    p.arguments = args.as_object().cloned();
                    p
                })
                .await
                .map_err(|e| format!("tool call failed: {e}"))?;
            // CallToolResult serializes to MCP's content-block shape, so reuse
            // the shared flattener via serde.
            let v = serde_json::to_value(&result).map_err(|e| e.to_string())?;
            Ok(flatten_mcp_content(&v))
        })
    }
}

/// Run the interactive OAuth flow for a server and persist the resulting token.
/// Steps: discover metadata → DCR-register → open browser → wait for loopback
/// callback → exchange code → (the FileCredentialStore saves automatically).
/// `config_dir` is the app's config dir (where the token file is written).
///
/// Returns the URL actually used for OAuth — which may differ from the input if
/// `normalize_url` rewrote a `/sse` endpoint to `/mcp`. The caller should save
/// the returned URL so the token's audience matches what `connect` will use.
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

        // Plug in our file store BEFORE starting, so the token exchange writes
        // straight to disk (no in-memory-only token that we'd have to persist).
        if let OAuthState::Unauthorized(ref mut mgr) = state {
            mgr.set_credential_store(FileCredentialStore::new(path));
        }

        // Empty scopes ⇒ let the SDK auto-select from the server's advertised
        // scopes (the standard "just sign in" UX, parity with `claude mcp add`).
        state
            .start_authorization(&[], REDIRECT_URI, Some("Bit"))
            .await
            .map_err(|e| format!("authorization start failed: {e}"))?;

        let auth_url = match &state {
            OAuthState::Session(s) => s.get_authorization_url().to_string(),
            _ => return Err("unexpected OAuth state after start_authorization".into()),
        };

        // Open the user's browser at the consent URL, then await the callback.
        open_browser(&auth_url);
        let (code, csrf) = wait_for_callback()
            .await
            .map_err(|e| format!("authorization callback failed: {e}"))?;

        // Exchange the code for a token. The FileCredentialStore::save runs as
        // part of this, persisting the token bundle atomically.
        if let OAuthState::Session(s) = &state {
            s.handle_callback(&code, &csrf)
                .await
                .map_err(|e| format!("token exchange failed: {e}"))?;
        }

        Ok(url)
    })
}

/// Convert an rmcp `Tool` into the same raw-Value shape the stdio path yields
/// (so `tool_defs` can namespace both uniformly AND `is_destructive` can read
/// annotations from either). MCP uses camelCase `inputSchema`/`destructiveHint`;
/// we preserve that casing — callers normalize as needed.
fn tool_to_value(tool: &rmcp::model::Tool) -> Value {
    let annotations = tool.annotations.as_ref().map(|a| {
        json!({
            "readOnlyHint": a.read_only_hint,
            "destructiveHint": a.destructive_hint,
            "idempotentHint": a.idempotent_hint,
            "openWorldHint": a.open_world_hint,
        })
    });
    json!({
        "name": tool.name.as_ref(),
        "description": tool.description.as_ref().map(|c| c.as_ref()),
        "inputSchema": tool.input_schema.as_ref(),
        "annotations": annotations,
    })
}

/// Open `url` in the user's default browser. macOS uses `open`; the fallbacks
/// cover Linux/Windows for completeness though Bit is Mac-focused.
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

/// Bind the loopback callback listener with `SO_REUSEADDR` set. This matters:
/// if a previous OAuth flow was interrupted (app force-quit, dev process killed),
/// the socket can linger in `TIME_WAIT` and a plain `bind` would fail with
/// “Address already in use” for a minute or two. SO_REUSEADDR lets us rebind
/// immediately. We also translate the “in use” error into a Bit-specific hint,
/// since the most likely cause is a stale Bit process still holding the port.
async fn bind_loopback(port: u16) -> Result<TcpListener, String> {
    use socket2::{Domain, Protocol, Socket, Type};

    // Build a non-blocking std socket with SO_REUSEADDR, then hand it to tokio.
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))
        .and_then(|s| {
            // Only ever listen on loopback — the OAuth callback must not be
            // reachable from the network.
            s.set_reuse_address(true)?;
            let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
            s.bind(&addr.into())?;
            s.listen(16)?;
            s.set_nonblocking(true)?;
            Ok(s)
        })
        .map_err(|e| format!("couldn't bind callback port {port}: {e}"))?;

    TcpListener::from_std(sock.into()).map_err(|e| {
        // The common user-facing case: another process owns the port. Most
        // often that's a stale Bit (interrupted sign-in). Tell them that.
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

/// Run a minimal loopback HTTP server on CALLBACK_PORT and wait for the OAuth
/// `redirect_uri` callback (`GET /callback?code=…&state=…`). Returns the code
/// and the CSRF `state` token. Times out after CALLBACK_TIMEOUT so a user who
/// abandons sign-in doesn't hang the flow forever. Raw TCP — no extra deps.
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
                for pair in q.split('&') {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    let val = percent_decode(v);
                    match k {
                        "code" => code = Some(val),
                        "state" => state = Some(val),
                        _ => {}
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

/// Minimal percent-decoding for the code/state query values (handles %2F etc.).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_basics() {
        assert_eq!(percent_decode("abc"), "abc");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("%20"), " ");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%ZZ"), "%ZZ"); // bad hex passes through
    }

    /// Reproduce the exact failure mode that bit the user: bind the callback
    /// port, drop it, and immediately rebind. Without SO_REUSEADDR this fails
    /// with “Address already in use” while the socket sits in TIME_WAIT.
    /// Uses an ephemeral port (not 8473) so parallel test runs don't collide.
    #[tokio::test]
    async fn loopback_rebinds_immediately_after_drop() {
        let port = pick_ephemeral_port();
        let first = bind_loopback(port)
            .await
            .expect("first bind should succeed");
        drop(first);
        // Rebind right away — this is where pre-hardening it failed.
        let second = bind_loopback(port)
            .await
            .expect("rebind after drop should succeed (SO_REUSEADDR)");
        drop(second);
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

    /// Grab a free ephemeral port by binding a socket to :0 and reading the
    /// assigned port, then dropping it (knowing bind_loopback sets SO_REUSEADDR).
    fn pick_ephemeral_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }
}
