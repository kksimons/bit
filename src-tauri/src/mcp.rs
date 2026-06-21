//! A small, synchronous stdio MCP (Model Context Protocol) client.
//!
//! Servers are configured Claude-Desktop-style (command + args + env) and spawned
//! as subprocesses; we speak newline-delimited JSON-RPC 2.0 over their stdio.
//! Their tools are merged into the agent's toolset, namespaced `mcp__<server>__<tool>`,
//! so the model can call them in the same tool-use loop as Bit's built-in tools.
//!
//! The protocol is deliberately minimal: initialize handshake, tools/list (cached),
//! tools/call. All access to a connection is serialized behind a per-server Mutex so
//! request→response is a simple blocking write-line/read-until-matching-id. Reads
//! time out (a wedged server can't hang the agent thread forever), and a failing
//! connection is dropped so the next call reconnects from scratch.
//!
//! HTTP (remote, OAuth) servers live in `mcp/http.rs` and use the rmcp SDK; the
//! stdio client in this file handles only local subprocess servers.

pub mod http;
pub mod oauth;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Manager;

/// MCP protocol version this client advertises on `initialize`.
/// (Slack/Google remote servers reject older/missing versions; stdio servers
/// built on the TS SDK honor whatever the client sends.)
const PROTOCOL_VERSION: &str = "2025-06-18";

/// How long to wait for a single JSON-RPC response before giving up and dropping
/// the connection. Generous enough for slow IMAP round-trips, short enough that a
/// wedged server can't hang the agent thread indefinitely.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Truncate tool results to keep large reads (a full inbox dump) out of the
/// model's context window — mirrors tools.rs's MAX_OUTPUT for built-ins.
const MAX_OUTPUT: usize = 4000;

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
    /// "stdio" (default) or "http". Controls which client the registry uses.
    #[serde(default = "default_transport")]
    pub transport: String,
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

fn default_transport() -> String {
    "stdio".into()
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

/// A live connection to one MCP server: the spawned child plus its cached
/// `tools/list`. Guarded by a Mutex so calls are strictly serialized (one
/// request/response on the wire at a time).
struct Connection {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Monotonic JSON-RPC request id.
    next_id: u64,
    /// Cached tools from `tools/list` (the raw server objects).
    tools: Vec<Value>,
}

impl Connection {
    /// Spawn the server, run the initialize handshake, and cache tools/list.
    fn connect(server: &McpServer) -> Result<Self, String> {
        let mut cmd = std::process::Command::new(&server.command);
        cmd.args(&server.args)
            .envs(server.env.iter().map(|(k, v)| (k.clone(), v.clone())))
            // Inherit PATH so `npx` resolves and can fetch packages.
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Server logs go to stderr; we never parse it, so discard.
            .stderr(Stdio::null());

        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "couldn't start '{}' (is it installed?): {e}",
                server.command
            )
        })?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut conn = Connection {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            tools: Vec::new(),
        };

        // 1. initialize — server echoes capabilities + (usually) the protocol version.
        let init = conn.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "bit", "version": env!("CARGO_PKG_VERSION") },
            }),
        )?;
        let _ = init; // capabilities acknowledged; version negotiation is lenient here.

        // 2. notifications/initialized — required by the spec to leave the handshake.
        conn.notify("notifications/initialized", json!({}))?;

        // 3. tools/list — cache for advertising to the model.
        let tools_resp = conn.request("tools/list", json!({}))?;
        conn.tools = tools_resp
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(conn)
    }

    /// Send a request and read until we see its matching response id, skipping
    /// notifications/log lines servers may emit between them. Times out after
    /// READ_TIMEOUT so a wedged server can't hang the agent loop.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|e| e.to_string())?;
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .map_err(|e| format!("write failed: {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("flush failed: {e}"))?;

        self.read_until_id(id)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .map_err(|e| e.to_string())?;
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .map_err(|e| format!("write failed: {e}"))?;
        self.stdin.flush().map_err(|e| format!("flush failed: {e}"))
    }

    /// Read stdout lines until we find a JSON-RPC response whose `id` matches.
    /// Non-JSON lines (some servers print banners to stdout despite the spec)
    /// and notifications (no `id`) are skipped. Returns Err on timeout or a
    /// server-reported JSON-RPC error.
    fn read_until_id(&mut self, want_id: u64) -> Result<Value, String> {
        let deadline = std::time::Instant::now() + READ_TIMEOUT;
        let mut buf = String::new();
        loop {
            if std::time::Instant::now() > deadline {
                return Err(format!(
                    "timed out after {}s waiting for response to request {want_id}",
                    READ_TIMEOUT.as_secs()
                ));
            }
            buf.clear();
            let n = self
                .stdout
                .read_line(&mut buf)
                .map_err(|e| format!("read failed: {e}"))?;
            if n == 0 {
                return Err("server closed stdout".into());
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue, // a non-JSON banner line; skip it
            };
            // Notifications have no id; ignore them while waiting for our reply.
            let Some(id) = msg.get("id") else { continue };
            if id.as_u64() != Some(want_id) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                return Err(format!(
                    "server error: {}",
                    err.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("(unknown)")
                ));
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Best-effort shutdown: kill the child so we never leak a subprocess.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Connection {
    /// Call a server-side tool by name. Returns the flattened text content.
    fn call_tool(&mut self, tool: &str, args: &Value) -> Result<String, String> {
        let result = self.request("tools/call", json!({ "name": tool, "arguments": args }))?;
        Ok(flatten_mcp_content(&result))
    }
}

/// A live connection regardless of transport. Both transports cache their
/// `tools/list` as the same raw serde Values, so the registry and `tool_defs`
/// never need to know which is which. Only `call` differs.
enum AnyConn {
    Stdio(Connection),
    Http(crate::mcp::http::HttpConnection),
}

impl AnyConn {
    fn tools(&self) -> &[Value] {
        match self {
            AnyConn::Stdio(c) => &c.tools,
            AnyConn::Http(c) => c.tools(),
        }
    }

    fn call(&mut self, tool: &str, args: &Value) -> Result<String, String> {
        match self {
            AnyConn::Stdio(c) => c.call_tool(tool, args),
            AnyConn::Http(c) => c.call(tool, args),
        }
    }
}

/// Entry in the global registry: either a live connection or the last error
/// that prevented establishing one (so the UI can show "error: …" and the agent
/// gets a clean Err rather than a silent reconnect on every call).
enum Slot {
    Connected(AnyConn),
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
    /// credentials and connects via rmcp.
    fn connect(app: &tauri::AppHandle, server: &McpServer) -> Result<AnyConn, String> {
        match server.transport.as_str() {
            "http" => {
                let dir = app
                    .path()
                    .app_config_dir()
                    .map_err(|e| format!("no config dir: {e}"))?;
                crate::mcp::http::HttpConnection::connect(&dir, server).map(AnyConn::Http)
            }
            _ => Connection::connect(server).map(AnyConn::Stdio),
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
pub fn flatten_mcp_content(result: &Value) -> String {
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
    if out.len() > MAX_OUTPUT {
        out.truncate(MAX_OUTPUT);
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
    if let Some(read_only) = ann
        .and_then(|a| a.get("readOnlyHint"))
        .and_then(|v| v.as_bool())
    {
        if read_only {
            return false;
        }
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
    let after_prefix = DESTRUCTIVE_PREFIXES
        .iter()
        .find_map(|p| lower.strip_prefix(p))
        .map(|rest| rest.is_empty() || !rest.chars().next().unwrap_or(' ').is_alphanumeric());
    after_prefix.unwrap_or(false)
}

/// `mcp__<server>__<tool>` — the form we advertise and that the model calls.
pub fn namespaced(server: &str, tool: &str) -> String {
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
        let big = "x".repeat(MAX_OUTPUT * 2);
        let v = json!({ "content": [{ "type": "text", "text": big }] });
        let out = flatten_mcp_content(&v);
        assert!(out.ends_with("…[truncated]"));
        assert!(out.len() < MAX_OUTPUT + 64);
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
            transport: "stdio".into(),
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
        let conn = Connection::connect(&server).expect("handshake should succeed");
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
}
