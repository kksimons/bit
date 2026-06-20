use crate::config::AgentConfig;
use crate::{tools, workflows};
use serde_json::{json, Value};

const SYSTEM: &str = "You are Bit, a desktop companion modeled on the Bit from the film TRON. \
Act on the user's Mac using the tools available to you (run their saved workflows, open apps and \
URLs, toggle Focus/Do Not Disturb, and — if enabled — other actions). When the user asks for \
something, DO it by calling the appropriate tool(s) before answering. \
\
You manage named multi-step workflows: when the request matches a saved workflow (by name or \
trigger phrase, e.g. \"let's work on Heatsink\"), call run_workflow. When asked to set up or change \
a routine, call save_workflow (note: drafts save disabled until the user enables them). \
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

fn is_openai(provider: &str) -> bool {
    matches!(provider, "openai" | "openrouter")
}

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

/// Returns (verdict, times): true=yes/false=no, repeated 1..=3 for emphasis.
pub fn ask(
    app: &tauri::AppHandle,
    cfg: &AgentConfig,
    transcript: &str,
) -> Result<(bool, u8), String> {
    let dev = crate::config::load_settings(app).developer_mode;
    let tools = tools::definitions(dev);
    if is_openai(&cfg.provider) {
        ask_openai(app, cfg, transcript, &to_openai_tools(&tools))
    } else {
        ask_anthropic(app, cfg, transcript, &tools)
    }
}

// ---------- Anthropic Messages API (Z.AI, Anthropic) ----------

fn anthropic_headers(cfg: &AgentConfig) -> Vec<(String, String)> {
    let mut h = vec![("anthropic-version".into(), "2023-06-01".into())];
    if cfg.provider == "anthropic" {
        h.push(("x-api-key".into(), cfg.api_key.clone()));
    } else {
        h.push(("authorization".into(), format!("Bearer {}", cfg.api_key)));
    }
    h
}

fn ask_anthropic(
    app: &tauri::AppHandle,
    cfg: &AgentConfig,
    transcript: &str,
    tools: &Value,
) -> Result<(bool, u8), String> {
    let url = format!("{}/v1/messages", base_url(cfg));
    let headers = anthropic_headers(cfg);
    let system = format!("{SYSTEM}{}", workflows_context(app));
    let mut messages: Vec<Value> = vec![json!({ "role": "user", "content": transcript })];

    for _ in 0..MAX_TURNS {
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
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                println!("[bit] tool: {name} {input}");
                let (text, is_error) = match tools::execute(app, name, &input) {
                    Ok(t) => (t, false),
                    Err(e) => (e, true),
                };
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
        return Ok(reduce_verdict(&text));
    }
    Err("tool loop exceeded max turns".into())
}

// ---------- OpenAI Chat Completions API (OpenAI, OpenRouter) ----------

fn openai_headers(cfg: &AgentConfig) -> Vec<(String, String)> {
    let mut h = vec![("authorization".into(), format!("Bearer {}", cfg.api_key))];
    if cfg.provider == "openrouter" {
        h.push((
            "http-referer".into(),
            "https://github.com/kksimons/bit".into(),
        ));
        h.push(("x-title".into(), "Bit".into()));
    }
    h
}

/// Convert our Anthropic-style tool defs to OpenAI's function-tool shape.
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
    transcript: &str,
    tools: &Value,
) -> Result<(bool, u8), String> {
    let url = format!("{}/chat/completions", base_url(cfg));
    let headers = openai_headers(cfg);
    let system = format!("{SYSTEM}{}", workflows_context(app));
    let mut messages: Vec<Value> = vec![
        json!({ "role": "system", "content": system }),
        json!({ "role": "user", "content": transcript }),
    ];

    for _ in 0..MAX_TURNS {
        let body = json!({
            "model": cfg.model,
            "messages": messages,
            "tools": tools,
            "tool_choice": "auto",
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
                println!("[bit] tool: {name} {input}");
                let result = tools::execute(app, name, &input).unwrap_or_else(|e| e);
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": result,
                }));
            }
            continue;
        }

        let text = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
        return Ok(reduce_verdict(text));
    }
    Err("tool loop exceeded max turns".into())
}

// ---------- shared ----------

fn post(url: &str, headers: &[(String, String)], body: Value) -> Result<Value, String> {
    let mut req = ureq::post(url).set("content-type", "application/json");
    for (k, v) in headers {
        req = req.set(k.as_str(), v.as_str());
    }
    match req.send_json(body) {
        Ok(r) => r.into_json().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(code, r)) => Err(format!(
            "HTTP {code}: {}",
            r.into_string().unwrap_or_default()
        )),
        Err(e) => Err(e.to_string()),
    }
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
