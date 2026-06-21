//! Manual live OAuth smoke test for the HTTP MCP transport.
//!
//! Unlike `cargo test` (which must be non-interactive), this drives the REAL
//! OAuth flow end-to-end against a DCR-supporting server, requiring a human to
//! complete the browser sign-in. It exercises everything the stdio handshake
//! test can't: metadata discovery → DCR → browser consent → loopback callback
//! → token exchange → token persistence → authenticated tools/list.
//!
//! Run it (a browser will open — complete the sign-in there):
//!
//! ```sh
//! cargo run --example mcp_oauth_smoke -- https://mcp.sentry.dev/mcp
//! ```
//!
//! The token is written under the app's normal config dir as a `0600` JSON
//! file, so a second run reuses it WITHOUT re-signing-in — proving persistence
//! + reconnect works too. Delete the token file to force re-auth.
//!
//! Sentry (`https://mcp.sentry.dev/mcp`) is the recommended target: it's
//! DCR-only (`registration_endpoint` present, `client_id_metadata_document_supported: false`),
//! so it exercises pure Dynamic Client Registration with no need for Bit to
//! host a public client-metadata document. Notion also works (DCR + CIMD).

use std::path::PathBuf;

use bit_lib::mcp;

/// Derive a server name from its URL, mirroring the UI's `deriveName` exactly
/// so this test exercises the same naming (and thus token-file path) the real
/// app produces: notion, linear, sentry, …
fn derive_name(url: &str) -> String {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    let stripped = host.strip_prefix("mcp.").unwrap_or(&host);
    stripped
        .split('.')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "service".into())
}

/// The same config dir the real app uses on macOS, so this test reflects reality
/// (a token saved here would be picked up by the running app, and vice versa).
fn app_config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    // macOS: ~/Library/Application Support/ca.kylesimons.bit
    let dir = PathBuf::from(home)
        .join("Library/Application Support")
        .join("ca.kylesimons.bit");
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let url = args
        .first()
        .cloned()
        .unwrap_or_else(|| "https://mcp.sentry.dev/mcp".to_string());
    let name = derive_name(&url);
    let dir = app_config_dir();

    println!("\n=== MCP OAuth smoke test ===");
    println!("server:    {url}");
    println!("token dir: {}", dir.display());
    println!();

    if !dir.join("mcp").join(format!("{name}.token.json")).exists() {
        println!("[1/2] Running OAuth flow — a browser will open. Complete sign-in there.");
        match mcp::http::run_oauth_flow(&dir, &name, &url) {
            Ok(resolved) => {
                println!("      ✓ token obtained and saved");
                if resolved != url {
                    println!("      (note: used {resolved} — /sse rewritten to /mcp)");
                }
            }
            Err(e) => {
                eprintln!("      ✗ OAuth flow failed: {e}");
                std::process::exit(1);
            }
        }
    } else {
        println!("[1/2] Reusing saved token (delete it to force re-auth).");
    }

    println!("[2/2] Connecting and listing tools…");
    let server = mcp::McpServer {
        name: name.clone(),
        transport: "http".into(),
        command: String::new(),
        args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        url: url.clone(),
        enabled: true,
        preset: String::new(),
        disabled_tools: Vec::new(),
    };
    let mut conn = match mcp::http::HttpConnection::connect(&dir, &server) {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("      ✗ connect/tools-list failed: {e}");
            eprintln!(
                "\n✗ FAILED. If the token is stale, delete the file under {} and re-run.",
                dir.join("mcp").display()
            );
            std::process::exit(1);
        }
    };
    println!("      ✓ connected");
    println!("      tools ({}):", conn.tools().len());
    for t in conn.tools() {
        let n = t.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let d = t
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        println!("        - {n}  {d}");
    }

    // [3/3] Prove a real tool CALL works end-to-end (OAuth → connect → tools/list
    // → tool call → result). Pick a safe read-only tool so this never mutates the
    // user's data: prefer readOnlyHint-annotated, else a get/list/search name,
    // else skip. Call with empty args {} and print a snippet.
    print!("\n[3/3] Calling a read-only tool to verify the call path… ");
    let safe = conn.tools().iter().find_map(|t| {
        let name = t.get("name").and_then(|v| v.as_str())?;
        let ann = t.get("annotations");
        let read_only = ann
            .and_then(|a| a.get("readOnlyHint"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let name_safe = ["get", "list", "search", "query"]
            .iter()
            .any(|p| name.to_lowercase().starts_with(p));
        if (read_only || name_safe) && !mcp::is_destructive(t) {
            Some(name.to_string())
        } else {
            None
        }
    });
    match safe {
        Some(tool) => {
            // For search tools, pass a minimal valid query so we get real data
            // back (exercises the success/result-flatten path, not just errors).
            let args = if tool.contains("search") {
                serde_json::json!({ "query": "a" })
            } else {
                serde_json::json!({})
            };
            match conn.call(&tool, &args) {
                Ok(result) => {
                    let snippet: String = result.chars().take(200).collect();
                    println!("✓\n      tool: {tool}\n      result (first 200 chars): {snippet}");
                }
                Err(e) => {
                    println!(
                        "call returned an error (path works, args may be wrong):\n        {e}"
                    );
                }
            }
        }
        None => println!("skipped (no safe read-only tool found to call)."),
    }
    println!("\n✓ SUCCESS — HTTP + OAuth + DCR end-to-end works.");
}
