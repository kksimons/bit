use crate::config::AgentConfig;
use crate::{tools, workflows};
use serde_json::{json, Value};
use std::sync::OnceLock;
use tauri::Manager;

/// Cap the body size the agent reads from an LLM endpoint (8 MiB). A yes/no
/// verdict over a few tool turns is tiny; this only guards against a runaway
/// server streaming megabytes back.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

const SYSTEM: &str = "You are Bit, a desktop companion: a floating geometric polyhedron. \
Act on the user's Mac using the tools available to you (run their saved workflows, open apps and \
URLs, toggle Focus/Do Not Disturb, and — if enabled — other actions). When the user asks for \
something, DO it by calling the appropriate tool(s) before answering. \
\
You may be connected to external services (email, docs, issue trackers, etc.) via MCP. Each \
connected service exposes tools named mcp__<service>__<tool>. When the user asks about one of \
these services — e.g. \"do I have unread email?\", \"any new issues?\", \"search my docs\" — call the \
matching tool to check, then answer from its result. Prefer read-only tools (search/list/get) to \
answer \"do I have…\" questions. Disabled tools are unavailable; never claim you checked \
something you couldn't. \
\
You manage named multi-step workflows: when the request matches a saved workflow (by name or \
trigger phrase, e.g. \"let's work on Heatsink\"), call run_workflow. When asked to set up or change \
a routine, call save_workflow (note: drafts save disabled until the user enables them). \
\
You have long-term memory tools. recall_history searches your past actions and notes — call it \
whenever the user refers to something done before (e.g. \"it\", \"that card\", \"the one I just made\", \
\"what did I just do\") so you can recover context like a card number from a previous command. \
remember saves a concise one-line lesson to memory for future recall — use it after a failure or \
when you learn something durable about the user's setup (e.g. which columns a board has, how a \
service is named). These persist across conversations. \
\
The request comes from speech-to-text and may be garbled. If you cannot confidently tell what they \
want, do NOT guess — just answer \"no\". \
\
Honesty is critical: only answer \"yes\" if you ACTUALLY completed the action and the tool results \
confirm success. If a tool errors or you couldn't do it, answer \"no\". Never claim unverified success. \
\
You can ONLY speak with the word \"yes\" or \"no\". For personality you MAY repeat it up to three times \
for emphasis (\"yes yes yes\", \"no no no\") when the moment fits, but usually once is right. Your final \
message must be only the word yes or no, repeated 1 to 3 times, lowercase, nothing else.";

const MAX_TURNS: usize = 8;

fn base_url(cfg: &AgentConfig) -> String {
    let b = cfg.base_url.trim().trim_end_matches('/');
    if !b.is_empty() {
        return b.to_string();
    }
    match cfg.provider.as_str() {
        "anthropic" => "https://api.anthropic.com",
        "openai" => "https://api.openai.com/v1",
        "openrouter" => "https://openrouter.ai/api/v1",
        _ => "https://api.z.ai/api/anthropic",
    }
    .to_string()
}

fn workflows_context(app: &tauri::AppHandle) -> String {
    let all = workflows::load_all(app);
    let enabled: Vec<_> = all.iter().filter(|w| w.enabled).collect();
    if enabled.is_empty() {
        return "\n\nThe user has no enabled workflows yet.".into();
    }
    let mut s =
        String::from("\n\nSaved workflows you can run (call run_workflow with the exact name):");
    for w in enabled {
        let triggers = if w.trigger_phrases.is_empty() {
            String::new()
        } else {
            format!(" — triggers: {}", w.trigger_phrases.join(", "))
        };
        s.push_str(&format!("\n- \"{}\"{triggers}", w.name));
    }
    s
}

/// Lists the user's connected MCP services so the model knows which external
/// apps it can reach. Only counts servers whose tools are actually live, which
/// is exactly what `tool_defs` advertises.
fn mcp_context(app: &tauri::AppHandle) -> String {
    let registry = app.state::<crate::mcp::Registry>();
    let mut lines: Vec<String> = Vec::new();
    for s in crate::mcp::load_all(app) {
        if !s.enabled {
            continue;
        }
        let n = registry.tool_count(&s.name);
        if n > 0 {
            lines.push(format!(
                "  - {} ({} tools, call them as mcp__{}__<tool>)",
                s.name, n, s.name
            ));
        }
    }
    if lines.is_empty() {
        "\n\nNo external services are connected yet.".into()
    } else {
        format!(
            "\n\nConnected services you can query via their mcp__<service>__ tools:\n{}",
            lines.join("\n")
        )
    }
}

/// Returns (verdict, times, tool_count): true=yes/false=no, repeated 1..=3 for
/// emphasis, and how many tool calls the loop made.
pub fn ask(
    app: &tauri::AppHandle,
    cfg: &AgentConfig,
    transcript: &str,
) -> Result<(bool, u8, usize), String> {
    let dev = crate::config::load_settings(app).developer_mode;
    let mut tools = tools::definitions(dev);
    tools
        .as_array_mut()
        .expect("definitions always returns an array")
        .extend(crate::mcp::tool_defs(app));
    let system = format!("{SYSTEM}{}{}", workflows_context(app), mcp_context(app));
    if matches!(cfg.provider.as_str(), "openai" | "openrouter") {
        ask_openai(app, cfg, &system, transcript, &to_openai_tools(&tools))
    } else {
        ask_anthropic(app, cfg, &system, transcript, &tools)
    }
}

/// Run one tool, log it to memory, nudge the thinking animation. Shared by both
/// provider loops.
fn run_and_log(app: &tauri::AppHandle, name: &str, input: &Value) -> (String, bool) {
    let t0 = std::time::Instant::now();
    let (text, is_error) = match tools::execute(app, name, input) {
        Ok(t) => (t, false),
        Err(e) => (e, true),
    };
    crate::memory::log_tool_call(app, name, input, &text, is_error, t0.elapsed().as_millis());
    crate::nudge_thinking(app);
    (text, is_error)
}

// ---------- Anthropic Messages API (Z.AI, Anthropic) ----------

fn ask_anthropic(
    app: &tauri::AppHandle,
    cfg: &AgentConfig,
    system: &str,
    transcript: &str,
    tools: &Value,
) -> Result<(bool, u8, usize), String> {
    let url = format!("{}/v1/messages", base_url(cfg));
    let mut headers = vec![("anthropic-version".into(), "2023-06-01".into())];
    if cfg.provider == "anthropic" {
        headers.push(("x-api-key".into(), cfg.api_key.clone()));
    } else {
        headers.push(("authorization".into(), format!("Bearer {}", cfg.api_key)));
    }
    let mut messages: Vec<Value> = vec![json!({ "role": "user", "content": transcript })];
    let mut tools_called = 0usize;

    for _ in 0..MAX_TURNS {
        crate::nudge_thinking(app);
        let body = json!({
            "model": cfg.model,
            "max_tokens": 1024,
            "system": system,
            "tools": tools,
            "messages": messages,
        });
        let v = post(&url, &headers, body)?;
        let stop = v.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("");
        let content = v
            .get("content")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();

        if stop == "tool_use" {
            messages.push(json!({ "role": "assistant", "content": content.clone() }));
            let mut results = Vec::new();
            for block in &content {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                    continue;
                }
                let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let (text, is_error) = run_and_log(app, name, &input);
                tools_called += 1;
                results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": text,
                    "is_error": is_error,
                }));
            }
            messages.push(json!({ "role": "user", "content": results }));
            continue;
        }

        let mut text = String::new();
        for block in &content {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                }
            }
        }
        let (verdict, times) = reduce_verdict(&text);
        return Ok((verdict, times, tools_called));
    }
    Err("tool loop exceeded max turns".into())
}

// ---------- OpenAI Chat Completions API (OpenAI, OpenRouter) ----------

/// Convert Anthropic-style tool defs to OpenAI's function-tool shape.
fn to_openai_tools(anthropic: &Value) -> Value {
    let arr = anthropic.as_array().cloned().unwrap_or_default();
    let out: Vec<Value> = arr
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.get("name").cloned().unwrap_or_default(),
                    "description": t.get("description").cloned().unwrap_or_default(),
                    "parameters": t.get("input_schema").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}})),
                }
            })
        })
        .collect();
    Value::Array(out)
}

fn ask_openai(
    app: &tauri::AppHandle,
    cfg: &AgentConfig,
    system: &str,
    transcript: &str,
    tools: &Value,
) -> Result<(bool, u8, usize), String> {
    let url = format!("{}/chat/completions", base_url(cfg));
    let mut headers = vec![("authorization".into(), format!("Bearer {}", cfg.api_key))];
    if cfg.provider == "openrouter" {
        headers.push((
            "http-referer".into(),
            "https://github.com/kksimons/bit".into(),
        ));
        headers.push(("x-title".into(), "Bit".into()));
    }
    let mut messages: Vec<Value> = vec![
        json!({ "role": "system", "content": system }),
        json!({ "role": "user", "content": transcript }),
    ];
    let mut tools_called = 0usize;

    for _ in 0..MAX_TURNS {
        crate::nudge_thinking(app);
        let body = json!({
            "model": cfg.model,
            "messages": messages,
            "tools": tools,
        });
        let v = post(&url, &headers, body)?;
        let msg = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .cloned()
            .ok_or("no choices in response")?;

        let calls = msg
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();

        if !calls.is_empty() {
            messages.push(msg.clone());
            for call in &calls {
                let id = call.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let f = call.get("function");
                let name = f
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                let args_str = f
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}");
                let input: Value = serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
                let (result, _is_error) = run_and_log(app, name, &input);
                tools_called += 1;
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": result,
                }));
            }
            continue;
        }

        let text = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
        let (verdict, times) = reduce_verdict(text);
        return Ok((verdict, times, tools_called));
    }
    Err("tool loop exceeded max turns".into())
}

// ---------- shared ----------

/// One shared async HTTP client so connections are pooled across turns, skipping
/// a TLS handshake on every POST after the first. Uses reqwest (already pulled
/// by rmcp) so we avoid shipping a second HTTP stack (ureq).
fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// POST JSON and read the JSON response. The agent loop is synchronous (runs on
/// std::threads), so we bridge to reqwest's async API via `block_on` — the same
/// pattern `mcp/http.rs` uses. Safe because we are never inside an async context.
fn post(url: &str, headers: &[(String, String)], body: Value) -> Result<Value, String> {
    tauri::async_runtime::handle().block_on(async move {
        let mut req = http().post(url).json(&body);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        // Cap the body so a runaway server can't OOM us. A yes/no verdict over a
        // few tool turns is tiny; 8 MiB is a generous ceiling.
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err("response exceeded max size".into());
        }
        if !status.is_success() {
            let snippet = String::from_utf8_lossy(&bytes);
            let snippet = snippet.chars().take(500).collect::<String>();
            return Err(format!("HTTP {}: {snippet}", status.as_u16()));
        }
        serde_json::from_slice::<Value>(&bytes).map_err(|e| format!("bad JSON: {e}"))
    })
}

/// Map free text to (verdict, times): count yes/no words, pick the winner, cap 3.
fn reduce_verdict(text: &str) -> (bool, u8) {
    let lower = text.to_lowercase();
    let mut yes = 0u8;
    let mut no = 0u8;
    for tok in lower.split(|c: char| !c.is_alphabetic()) {
        match tok {
            "yes" | "yeah" | "yep" | "yup" => yes = yes.saturating_add(1),
            "no" | "nope" | "nah" => no = no.saturating_add(1),
            _ => {}
        }
    }
    if yes == 0 && no == 0 {
        return (false, 1);
    }
    let verdict = yes >= no;
    let times = if verdict { yes } else { no }.clamp(1, 3);
    (verdict, times)
}
