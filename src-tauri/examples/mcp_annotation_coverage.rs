//! Reports `annotations` coverage for a server's tools — the data that tells us
//! whether destructive-detection works off real server hints or falls back to
//! the name heuristic. Reuses a saved OAuth token (no browser) via the same
//! connect path as the app.
//!
//! ```sh
//! cargo run --example mcp_annotation_coverage -- https://mcp.notion.com/mcp
//! cargo run --example mcp_annotation_coverage -- https://mcp.linear.app/mcp
//! ```

use std::collections::BTreeMap;

use bit_lib::mcp;

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

fn app_config_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = std::path::PathBuf::from(home)
        .join("Library/Application Support")
        .join("ca.kylesimons.bit");
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://mcp.notion.com/mcp".to_string());
    let name = derive_name(&url);
    let dir = app_config_dir();

    let server = mcp::McpServer {
        name: name.clone(),
        transport: "http".into(),
        command: String::new(),
        args: Vec::new(),
        env: BTreeMap::new(),
        url: url.clone(),
        enabled: true,
        preset: String::new(),
        disabled_tools: Vec::new(),
    };

    let conn = match mcp::http::HttpConnection::connect(&dir, &server) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("connect failed: {e}");
            std::process::exit(1);
        }
    };

    let tools = conn.tools();
    let total = tools.len();
    let with_annotations = tools
        .iter()
        .filter(|t| t.get("annotations").map(|a| !a.is_null()).unwrap_or(false))
        .count();
    let flagged_destructive = tools.iter().filter(|t| mcp::is_destructive(t)).count();

    println!("\n=== {name} ({url}) ===");
    println!("tools: {total}");
    println!("with annotations: {with_annotations}/{total}");
    println!("Bit flags destructive: {flagged_destructive}/{total}");
    println!();
    println!("per-tool detail:");
    for t in tools {
        let n = t.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let ann = t.get("annotations");
        let has_ann = ann.map(|a| !a.is_null()).unwrap_or(false);
        let destr = mcp::is_destructive(t);
        let mark = if destr { "⚠" } else { " " };
        let ann_str = if has_ann {
            let a = ann.unwrap();
            format!(
                "read_only={:?} destructive={:?}",
                a.get("readOnlyHint").and_then(|v| v.as_bool()),
                a.get("destructiveHint").and_then(|v| v.as_bool()),
            )
        } else {
            "(no annotations — heuristic used)".to_string()
        };
        println!("  {mark} {n:32} {ann_str}");
    }
}
