//! MCP (Model Context Protocol) client for both local (stdio) and remote (http)
//! servers, built on the rmcp SDK. Servers are configured Claude-Desktop-style
//! (command + args + env) for stdio, or by URL + OAuth for http. Their tools are
//! merged into the agent's toolset, namespaced `mcp__<server>__<tool>`.
//!
//! `RmcpConnection` (below) holds one live service for either transport; the
//! `Registry` keeps one slot per server behind a Mutex so calls are serialized.
//! The agent loop is synchronous, so async rmcp calls are bridged with
//! `tauri::async_runtime::handle().block_on(...)` — safe because the agent runs
//! on plain std::threads, not inside an async context. HTTP/OAuth wiring (the
//! credential store, the browser/loopback flow) lives in `mcp/http.rs`.

pub mod http;
pub mod oauth;

use rmcp::model::{CallToolRequestParams, ClientInfo, Tool as RmcpTool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::ServiceExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tauri::Manager;
use tokio::sync::Mutex as AsyncMutex;

/// How an MCP server is reached: a local subprocess (stdio) or a remote
/// Streamable-HTTP endpoint (http, via OAuth). Serialized lowercase so the
/// config/UI keeps using "stdio"/"http" strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Stdio,
    Http,
}

// ============================ model + storage ============================

/// One configured MCP server, persisted in `mcp.json`. The "preset" flag marks
/// gallery entries (Gmail, …) so the UI can render them distinctly from a raw
/// Custom server; it carries no behavioral meaning at runtime.
///
/// Two transports: `stdio` (a local subprocess like the Gmail preset) or `http`
/// (a remote Streamable-HTTP server using OAuth, e.g. Notion / Sentry / GitHub).
/// Stdio uses `command`/`args`/`env`; http uses `url` and a stored OAuth token.
#[derive(Serialize, Deserialize, Clone)]
pub struct McpServer {
    pub name: String,
    #[serde(default)]
    pub transport: Transport,
    /// stdio only: the executable to spawn (e.g. "npx").
    #[serde(default)]
    pub command: String,
    /// stdio only: args passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// stdio only: environment variables for the subprocess (e.g. credentials).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// http only: the remote server's Streamable-HTTP endpoint.
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Gallery preset (e.g. "gmail"). Blank for user-authored Custom servers.
    #[serde(default)]
    pub preset: String,
    /// Tools the user has turned off for this server (denylist). Stored by the
    /// server-side tool name (NOT the mcp__ namespaced form). Default empty =
    /// all tools on. Disabled tools are neither advertised nor callable.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn store_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    Some(app.path().app_config_dir().ok()?.join("mcp.json"))
}

pub fn load_all(app: &tauri::AppHandle) -> Vec<McpServer> {
    store_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_all(app: &tauri::AppHandle, all: &[McpServer]) -> Result<(), String> {
    let path = store_path(app).ok_or("no config dir")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(all).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

/// Create or update a server, matched by case-insensitive name.
pub fn upsert(app: &tauri::AppHandle, server: McpServer) -> Result<McpServer, String> {
    let mut all = load_all(app);
    if let Some(existing) = all
        .iter_mut()
        .find(|s| s.name.eq_ignore_ascii_case(&server.name))
    {
        *existing = server.clone();
    } else {
        all.push(server.clone());
    }
    save_all(app, &all)?;
    Ok(server)
}

/// Replace one server's disabled-tool denylist. Used by the per-tool toggles
/// (and “Disable all destructive”) — cheap: just rewrites `mcp.json`, no
/// reconnect needed since filtering happens at advertise/call time.
pub fn set_disabled_tools(
    app: &tauri::AppHandle,
    name: &str,
    disabled: Vec<String>,
) -> Result<(), String> {
    let mut all = load_all(app);
    let server = all
        .iter_mut()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("no MCP server named '{name}'"))?;
    server.disabled_tools = disabled;
    save_all(app, &all)
}

pub fn delete(app: &tauri::AppHandle, name: &str) -> Result<(), String> {
    let mut all = load_all(app);
    let before = all.len();
    all.retain(|s| !s.name.eq_ignore_ascii_case(name));
    if all.len() == before {
        return Err(format!("no MCP server named '{name}'"));
    }
    save_all(app, &all)
}

// ============================ connection ============================

/// The concrete rmcp client service type held between calls (stdio or http).
type ClientService = RunningService<RoleClient, ClientInfo>;

/// One rmcp-backed MCP connection — used for BOTH stdio and http transports.
/// Holds the live `RunningService` (so the session/auth/transport persist across
/// calls) plus the cached tool list in the raw-Value shape `tool_defs` and
/// `is_destructive` expect. The agent loop is sync, so async rmcp calls are
/// bridged with `block_on`.
pub struct RmcpConnection {
    service: Arc<AsyncMutex<Option<ClientService>>>,
    tools: Vec<Value>,
}

impl RmcpConnection {
    /// Construct from a live service + its tool list (shared by stdio and http).
    pub(crate) fn new(svc: ClientService, tools: Vec<Value>) -> Self {
        Self {
            service: Arc::new(AsyncMutex::new(Some(svc))),
            tools,
        }
    }

    /// Cached tools/list (raw serde Values; namespaced later by `tool_defs`).
    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    /// Stdio: spawn the server subprocess, run the initialize handshake + tools/list.
    pub fn connect_stdio(server: &McpServer) -> Result<Self, String> {
        // Clone the config so we can move it into the async block; the tokio
        // child-process spawn must run inside the runtime (block_on).
        let command = server.command.clone();
        let args = server.args.clone();
        let env = server.env.clone();
        tauri::async_runtime::handle().block_on(async move {
            let mut cmd = tokio::process::Command::new(&command);
            // Inherit the parent env (so PATH resolves `npx`), then layer the
            // server's configured env (credentials) on top.
            cmd.args(&args)
                .envs(env.iter().map(|(k, v)| (k.clone(), v.clone())));
            let (transport, _stderr) = TokioChildProcess::builder(cmd)
                .stderr(Stdio::null()) // server stderr is never parsed; discard it
                .spawn()
                .map_err(|e| format!("couldn't start '{command}' (is it installed?): {e}"))?;
            let svc = ClientInfo::default()
                .serve(transport)
                .await
                .map_err(|e| format!("MCP handshake failed: {e}"))?;
            let tools = svc
                .peer()
                .list_all_tools()
                .await
                .map_err(|e| format!("tools/list failed: {e}"))?;
            Ok(Self::new(svc, tools.iter().map(tool_to_value).collect()))
        })
    }

    /// Call a server-side tool by name. Bridges to async; flattens the result
    /// text the same way for both transports.
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
            // CallToolResult serializes to MCP's content-block shape; reuse the
            // shared flattener via serde.
            let v = serde_json::to_value(&result).map_err(|e| e.to_string())?;
            Ok(flatten_mcp_content(&v))
        })
    }
}

/// Convert an rmcp `Tool` into the raw-Value shape the rest of this module
/// expects (matching what a stdio server's tools/list yields), so `tool_defs`
/// can namespace both uniformly AND `is_destructive` can read annotations from
/// either. MCP uses camelCase `inputSchema`/`destructiveHint`; preserved as-is.
pub(crate) fn tool_to_value(tool: &RmcpTool) -> Value {
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

/// Entry in the global registry: either a live connection or the last error
/// that prevented establishing one (so the UI can show "error: …" and the agent
/// gets a clean Err rather than a silent reconnect on every call).
enum Slot {
    Connected(RmcpConnection),
    /// Cached error from the last failed connect; cleared on next retry.
    Error(String),
}

/// All configured servers' connections, keyed by name. The map itself is behind
/// an Arc so the Registry handle can be cloned cheaply (AppState owns one, and
/// commands/pre-warm threads grab clones).
#[derive(Default, Clone)]
pub struct Registry {
    inner: Arc<Mutex<BTreeMap<String, Arc<Mutex<Slot>>>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get-or-create the slot for a server. The slot persists for the app's
    /// lifetime so a once-connected server stays connected across calls.
    fn slot(&self, name: &str) -> Arc<Mutex<Slot>> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(s) = inner.get(name) {
            return s.clone();
        }
        let s = Arc::new(Mutex::new(Slot::Error("not connected yet".into())));
        inner.insert(name.to_string(), s.clone());
        s
    }

    /// Force-drop a server's connection (used on delete / config change) so the
    /// next call reconnects with fresh settings.
    pub fn invalidate(&self, name: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.remove(name);
    }

    /// Establish a fresh connection to `server`, picking the transport by
    /// `server.transport`. Stdio spawns the subprocess; http loads stored OAuth
    /// credentials and connects. Both yield the same `RmcpConnection`.
    fn connect(app: &tauri::AppHandle, server: &McpServer) -> Result<RmcpConnection, String> {
        match server.transport {
            Transport::Stdio => RmcpConnection::connect_stdio(server),
            Transport::Http => {
                let dir = app
                    .path()
                    .app_config_dir()
                    .map_err(|e| format!("no config dir: {e}"))?;
                crate::mcp::http::connect(&dir, server)
            }
        }
    }

    /// Ensure the slot holds a live connection; return the cached tools if so.
    /// Establishes (or re-establishes) the connection lazily on first use or
    /// after a prior failure.
    pub fn ensure(&self, app: &tauri::AppHandle, server: &McpServer) -> Result<Vec<Value>, String> {
        let slot = self.slot(&server.name);
        let mut guard = slot.lock().unwrap();
        match &mut *guard {
            Slot::Connected(_) => {}
            Slot::Error(prev) => {
                // Try to (re)connect; on failure, cache the new error.
                match Self::connect(app, server) {
                    Ok(conn) => {
                        let tools = conn.tools().to_vec();
                        *guard = Slot::Connected(conn);
                        return Ok(tools);
                    }
                    Err(e) => {
                        *prev = e.clone();
                        return Err(e);
                    }
                }
            }
        }
        // Already connected: return its cached tools.
        if let Slot::Connected(conn) = &*guard {
            Ok(conn.tools().to_vec())
        } else {
            unreachable!("guard was just matched as Connected")
        }
    }

    /// How many tools an enabled, connected server advertises (0 if not
    /// connected or failed). Used for the UI "connected · N tools" status.
    pub fn tool_count(&self, server: &McpServer) -> usize {
        let slot = self.slot(&server.name);
        let guard = slot.lock().unwrap();
        match &*guard {
            Slot::Connected(conn) => conn.tools().len(),
            _ => 0,
        }
    }

    /// Is the slot currently in a connected (not error) state? Best-effort,
    /// for UI status; a true connection test runs tools/list via `probe`.
    pub fn is_connected(&self, name: &str) -> bool {
        let Some(slot) = self.inner.lock().unwrap().get(name).cloned() else {
            return false;
        };
        let guard = slot.lock().unwrap();
        matches!(*guard, Slot::Connected(_))
    }

    /// Last cached error for a server (for the UI), if any.
    pub fn last_error(&self, name: &str) -> Option<String> {
        let slot = self.inner.lock().unwrap().get(name).cloned()?;
        let guard = slot.lock().unwrap();
        if let Slot::Error(e) = &*guard {
            Some(e.clone())
        } else {
            None
        }
    }

    /// Call a tool on a server. Establishes the connection if needed. The tool
    /// `name` is the server-side name (NOT the mcp__ namespaced form).
    fn call_tool(
        &self,
        app: &tauri::AppHandle,
        server: &McpServer,
        tool: &str,
        args: &Value,
    ) -> Result<String, String> {
        let slot = self.slot(&server.name);
        // (Re)connect if the slot isn't holding a live connection. Drop the
        // slot-level lock while we (re)connect so the UI's status reads don't
        // block on a slow npx cold-start.
        {
            let needs_connect = matches!(*slot.lock().unwrap(), Slot::Error(_));
            if needs_connect {
                match Self::connect(app, server) {
                    Ok(conn) => {
                        *slot.lock().unwrap() = Slot::Connected(conn);
                    }
                    Err(e) => {
                        *slot.lock().unwrap() = Slot::Error(e.clone());
                        return Err(e);
                    }
                }
            }
        }
        // Issue the call under the slot lock. On failure, assume the server is
        // wedged/dead and drop it so the next call reconnects cleanly.
        let mut guard = slot.lock().unwrap();
        let outcome = match &mut *guard {
            Slot::Connected(conn) => conn.call(tool, args),
            Slot::Error(_) => unreachable!("upgraded to Connected above"),
        };
        if let Err(e) = &outcome {
            *guard = Slot::Error(e.clone());
        }
        outcome
    }
}

/// Pull readable text out of an MCP `tools/call` result, which is a list of
/// content blocks (`{type:"text", text:"…"}`, etc.). Flatten all text blocks,
/// separated by newlines; cap the total to keep the model context bounded.
pub(crate) fn flatten_mcp_content(result: &Value) -> String {
    let mut out = String::new();
    if let Some(arr) = result.get("content").and_then(|c| c.as_array()) {
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
    }
    if out.trim().is_empty() {
        out = serde_json::to_string(result).unwrap_or_else(|_| "(empty result)".into());
    }
    if out.len() > crate::MAX_OUTPUT {
        // Truncate on a UTF-8 char boundary (plain `truncate` panics mid-char,
        // e.g. on emoji/CJK in MCP results at the cut point).
        let cut = out
            .char_indices()
            .take_while(|(i, _)| *i <= crate::MAX_OUTPUT)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(crate::MAX_OUTPUT);
        out.truncate(cut);
        out.push_str("\n…[truncated]");
    }
    out
}

// ============================ agent-facing API ============================

/// Tool definitions for every ENABLED server, namespaced as
/// `mcp__<server>__<tool>` and shaped as Anthropic tool defs
/// (`name`, `description`, `input_schema`). Never connects on failure — a
/// server that won't start simply contributes no tools and the agent proceeds.
pub fn tool_defs(app: &tauri::AppHandle) -> Vec<Value> {
    let registry = app.state::<Registry>();
    let mut out = Vec::new();
    for server in load_all(app) {
        if !server.enabled {
            continue;
        }
        let tools = match registry.ensure(app, &server) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[bit] mcp '{}': {e}", server.name);
                continue;
            }
        };
        for tool in tools {
            let Some(raw_name) = tool.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            // Denylist: the user turned this tool off in Settings. Skipping it
            // here means the model never sees it — it can't call what it can't
            // name. (call() refuses it as a backstop too.)
            if server
                .disabled_tools
                .iter()
                .any(|d| d.eq_ignore_ascii_case(raw_name))
            {
                continue;
            }
            // MCP uses `inputSchema` (camelCase); Anthropic/OpenAI want
            // `input_schema` / `parameters`. Normalize once, here.
            let schema = tool
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type":"object","properties":{}}));
            out.push(json!({
                "name": namespaced(&server.name, raw_name),
                "description": format!(
                    "[{}] {}",
                    server.name,
                    tool.get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or(raw_name)
                ),
                "input_schema": schema,
            }));
        }
    }
    out
}

/// Verbs whose presence at the start of a tool name strongly suggests the
/// tool removes/destroys data. Conservative on purpose — no `remove`, `clear`,
/// `reset` (too many false positives like “remove from cart”). This is a
/// FALLBACK for servers that omit MCP `annotations`; annotation hints win when
/// present.
const DESTRUCTIVE_PREFIXES: &[&str] = &[
    "delete", "destroy", "drop", "purge", "erase", "wipe", "trash",
];

/// Heuristic + spec union: should this tool be flagged possibly-destructive?
/// Used by the UI for the ⚠ badge and the “Disable all destructive” button.
/// Honors real `annotations` when the server provides them (readOnlyHint wins
/// as a non-destructive override; destructiveHint forces it on), then falls
/// back to a name-prefix check for servers that don’t annotate (Gmail).
pub fn is_destructive(tool: &Value) -> bool {
    let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let ann = tool.get("annotations");
    // Spec: readOnlyHint=true means no side effects at all — never destructive.
    if let Some(true) = ann
        .and_then(|a| a.get("readOnlyHint"))
        .and_then(|v| v.as_bool())
    {
        return false;
    }
    // Explicit destructiveHint from the server: trust it either way.
    if let Some(destructive) = ann
        .and_then(|a| a.get("destructiveHint"))
        .and_then(|v| v.as_bool())
    {
        return destructive;
    }
    // No usable annotation — fall back to the name heuristic.
    let lower = name.to_lowercase();
    DESTRUCTIVE_PREFIXES.iter().any(|p| {
        lower
            .strip_prefix(p)
            .is_some_and(|rest| rest.is_empty() || !rest.starts_with(|c: char| c.is_alphanumeric()))
    })
}

/// `mcp__<server>__<tool>` — the form we advertise and that the model calls.
pub(crate) fn namespaced(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}
/// Split a namespaced tool name back into (server, tool).
fn parse_namespaced(full: &str) -> Option<(&str, &str)> {
    let rest = full.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    Some((server, tool))
}

/// One tool's UI-facing metadata: name, short description, destructive flag,
/// and whether the user currently has it enabled (per the server's denylist).
#[derive(serde::Serialize, Clone)]
pub struct ToolView {
    pub name: String,
    pub description: String,
    pub destructive: bool,
    pub enabled: bool,
}

/// List a server's tools for the “Manage tools” UI. Connects if needed (so the
/// list is fresh) but doesn't filter by disabled_tools — the UI shows ALL tools
/// with their enabled state, so the user can re-enable things. Returns Err if
/// the server can't be reached (the UI surfaces it).
pub fn tools_view(app: &tauri::AppHandle, name: &str) -> Result<Vec<ToolView>, String> {
    let server = load_all(app)
        .into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("no MCP server named '{name}'"))?;
    let registry = app.state::<Registry>();
    let tools = registry.ensure(app, &server)?;
    Ok(tools
        .iter()
        .map(|t| {
            let tname = t
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            ToolView {
                enabled: !server
                    .disabled_tools
                    .iter()
                    .any(|d| d.eq_ignore_ascii_case(&tname)),
                destructive: is_destructive(t),
                description: t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string(),
                name: tname,
            }
        })
        .collect())
}

/// Execute a namespaced `mcp__…` tool call. Routes to the right server, falls
/// back to a clean error string the agent can act on. Called from tools::execute.
pub fn call(app: &tauri::AppHandle, full_name: &str, args: &Value) -> Result<String, String> {
    let Some((server_name, tool)) = parse_namespaced(full_name) else {
        return Err(format!("malformed MCP tool name: {full_name}"));
    };
    let server = load_all(app)
        .into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(server_name))
        .ok_or_else(|| format!("MCP server '{server_name}' is not configured"))?;
    if !server.enabled {
        return Err(format!("MCP server '{}' is disabled", server.name));
    }
    if server
        .disabled_tools
        .iter()
        .any(|d| d.eq_ignore_ascii_case(tool))
    {
        return Err(format!(
            "MCP tool '{}__{tool}' has been turned off in Bit's Connections settings",
            server.name
        ));
    }
    let registry = app.state::<Registry>();
    registry.call_tool(app, &server, tool, args)
}

// ============================ presets ============================

/// Gallery preset metadata: a label, the credential fields the UI collects,
/// and a factory that builds the `McpServer` skeleton. The factory is a function
/// (not a const) because `McpServer` owns heap-allocated Strings.
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Empty credential fields the UI collects, with human labels + help text.
    pub fields: &'static [PresetField],
    /// Launch details for this preset's server (command + args). The UI merges
    /// the collected field values into `env` before saving.
    pub command: &'static str,
    pub args: &'static [&'static str],
}

pub struct PresetField {
    pub env_key: &'static str,
    pub label: &'static str,
    pub placeholder: &'static str,
    pub secret: bool,
}

/// Gmail via a community IMAP MCP server (app-password auth, no OAuth/GCP).
/// Verified: 19 tools incl. search_emails, get_primary_emails, list_labels.
const GMAIL_PRESET: Preset = Preset {
    id: "gmail",
    label: "Gmail",
    description: "Read your inbox — “do I have unread mail?”, “any email from …?”",
    fields: &[
        PresetField {
            env_key: "GMAIL_EMAIL",
            label: "Your Gmail address",
            placeholder: "you@gmail.com",
            secret: false,
        },
        PresetField {
            env_key: "GMAIL_APP_PASSWORD",
            label: "App Password (16 characters)",
            placeholder: "e.g. zfuy wpew zrde nmij",
            secret: true,
        },
    ],
    command: "npx",
    args: &["-y", "gmail-mcp-imap"],
};

pub fn presets() -> &'static [Preset] {
    &[GMAIL_PRESET]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_round_trip() {
        let n = namespaced("gmail", "search_emails");
        assert_eq!(n, "mcp__gmail__search_emails");
        assert_eq!(parse_namespaced(&n), Some(("gmail", "search_emails")));
    }

    #[test]
    fn rejects_non_mcp_names() {
        assert_eq!(parse_namespaced("run_shell"), None);
        assert_eq!(parse_namespaced("mcp__onlyoneunderscore"), None);
        assert_eq!(parse_namespaced("mcp__a__b__c"), Some(("a", "b__c")));
    }

    #[test]
    fn flatten_extracts_text_blocks() {
        let v = json!({
            "content": [
                { "type": "text", "text": "Subject: Hi" },
                { "type": "text", "text": "Body here" },
                { "type": "image", "data": "..." }
            ]
        });
        assert_eq!(flatten_mcp_content(&v), "Subject: Hi\nBody here");
    }

    #[test]
    fn flatten_truncates_large_output() {
        let big = "x".repeat(crate::MAX_OUTPUT * 2);
        let v = json!({ "content": [{ "type": "text", "text": big }] });
        let out = flatten_mcp_content(&v);
        assert!(out.ends_with("…[truncated]"));
        assert!(out.len() < crate::MAX_OUTPUT + 64);
    }

    #[test]
    fn destructive_annotation_wins() {
        // Server says destructive=true → flagged, regardless of name.
        assert!(is_destructive(&json!({
            "name": "send_message",
            "annotations": { "destructiveHint": true }
        })));
        // Server says destructive=false → not flagged, even if name starts with delete.
        assert!(!is_destructive(&json!({
            "name": "delete_safe_copy",
            "annotations": { "destructiveHint": false }
        })));
    }

    #[test]
    fn readonly_is_never_destructive() {
        // readOnlyHint=true overrides everything.
        assert!(!is_destructive(&json!({
            "name": "delete_everything",
            "annotations": { "readOnlyHint": true, "destructiveHint": true }
        })));
    }

    #[test]
    fn destructive_name_heuristic_fallback() {
        // No annotations → name heuristic (servers like gmail-mcp-imap omit them).
        for name in [
            "delete_email",
            "destroy_all",
            "drop_table",
            "purge_cache",
            "wipe_disk",
            "trash_item",
            "erase_history",
        ] {
            assert!(
                is_destructive(&json!({ "name": name })),
                "{name} should be destructive"
            );
        }
        // Non-destructive names + the heuristic word as a non-prefix → safe.
        for name in [
            "send_email",
            "get_primary_emails",
            "search",
            "list_labels",
            "undeleted",
            "nodelete",
        ] {
            assert!(
                !is_destructive(&json!({ "name": name })),
                "{name} should NOT be destructive"
            );
        }
    }

    /// End-to-end handshake against the REAL gmail-mcp-imap server.
    /// Skipped by default (needs npx + network to fetch the package) — run with
    /// `cargo test -- --ignored mcp`. Dummy creds are fine: the MCP handshake +
    /// tools/list complete before any IMAP login, so this proves the full client
    /// wiring (spawn → initialize → notifications/initialized → tools/list)
    /// without real Google credentials.
    #[test]
    #[ignore]
    fn gmail_handshake_lists_tools() {
        // Skip fast if npx isn’t available, so a missing toolchain doesn’t fail CI.
        if std::process::Command::new("npx")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("[test] npx not found; skipping live handshake");
            return;
        }
        let server = McpServer {
            name: "gmail".into(),
            transport: Transport::Stdio,
            command: "npx".into(),
            args: vec!["-y".into(), "gmail-mcp-imap".into()],
            env: [
                ("GMAIL_EMAIL".to_string(), "dummy@example.com".to_string()),
                (
                    "GMAIL_APP_PASSWORD".to_string(),
                    "zzzzzzzzzzzzzzzz".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
            url: String::new(),
            enabled: true,
            preset: "gmail".into(),
            disabled_tools: Vec::new(),
        };
        // Exercise the stdio client directly (no AppHandle needed in a unit
        // test): spawn → initialize → tools/list.
        let conn = RmcpConnection::connect_stdio(&server).expect("handshake should succeed");
        let names: Vec<&str> = conn
            .tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(
            names.contains(&"search_emails"),
            "missing search_emails: {names:?}"
        );
        assert!(
            names.contains(&"get_primary_emails"),
            "missing get_primary_emails"
        );
        assert!(
            names.len() >= 10,
            "expected many tools, got {}: {names:?}",
            names.len()
        );
    }

    /// End-to-end characterization of the stdio MCP client against a local echo
    /// server fixture. This is the safety net for any rewrite of the hand-rolled
    /// client (e.g. onto rmcp): spawn → initialize → tools/list → tools/call,
    /// banner skipping, and destructive detection on the listed tools. Runs when
    /// `bun` or `node` is available; skips gracefully otherwise.
    #[test]
    fn stdio_client_end_to_end() {
        let mut runner = None;
        for c in ["bun", "node"] {
            if std::process::Command::new(c)
                .arg("--version")
                .output()
                .is_ok()
            {
                runner = Some(c);
                break;
            }
        }
        let Some(runner) = runner else {
            eprintln!("[test] bun/node not found; skipping stdio end-to-end");
            return;
        };
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/echo-mcp.mjs");
        let server = McpServer {
            name: "echo".into(),
            transport: Transport::Stdio,
            command: runner.into(),
            args: vec![fixture.into()],
            env: BTreeMap::new(),
            url: String::new(),
            enabled: true,
            preset: String::new(),
            disabled_tools: Vec::new(),
        };
        let mut conn =
            RmcpConnection::connect_stdio(&server).expect("stdio connect should succeed");

        // tools/list returned both tools, and the non-JSON startup banner was
        // skipped (otherwise we'd see 3 tools or fail to parse).
        let names: Vec<&str> = conn
            .tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert_eq!(names, ["echo", "delete_thing"], "got: {names:?}");

        // Destructive detection flows through the raw tool defs the server sent:
        // "echo" is safe; "delete_thing" is flagged by the name heuristic.
        let tool = |n: &str| {
            conn.tools
                .iter()
                .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(n))
                .unwrap()
        };
        assert!(
            !is_destructive(tool("echo")),
            "echo should not be destructive"
        );
        assert!(
            is_destructive(tool("delete_thing")),
            "delete_thing should be destructive"
        );

        // Namespacing is the form the agent sees + calls.
        assert_eq!(namespaced("echo", "echo"), "mcp__echo__echo");

        // tools/call returns the flattened text content.
        let out = conn
            .call("echo", &json!({ "text": "hello world" }))
            .expect("tools/call should succeed");
        assert!(out.contains("hello world"), "got: {out}");
    }
}
