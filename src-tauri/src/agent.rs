use crate::config::AgentConfig;
use crate::tools;
use serde_json::{json, Value};

const SYSTEM: &str = "You are Bit, a desktop companion modeled on the Bit from the film TRON. \
You can act on the user's Mac using the provided tools (run shell commands, open apps and URLs, \
run AppleScript, toggle Focus/Do Not Disturb). When the user asks you to do something, actually \
DO it by calling the appropriate tool(s) before answering. \
\
The user's request comes from speech-to-text and may be garbled. If you cannot confidently tell \
what they want, do NOT guess and do NOT run placeholder or echo commands — just answer \"no\". \
\
Honesty is critical: only answer \"yes\" if you ACTUALLY completed the requested action and the \
tool results confirm success. If any tool returns an error (is_error true) or a non-zero exit code, \
or you could not carry out the request, answer \"no\". Never claim success you did not verify. \
\
You can ONLY speak to the user with a single word: \"yes\" or \"no\". \
Your final message must be exactly one lowercase word: yes or no.";

const MAX_TURNS: usize = 6;

/// Run the agent loop: call the model, execute any tool calls, repeat until it
/// gives a final answer, then reduce that to a yes (true) / no (false) verdict.
pub fn ask(cfg: &AgentConfig, transcript: &str) -> Result<bool, String> {
    let url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'));
    let mut messages: Vec<Value> = vec![json!({ "role": "user", "content": transcript })];

    for _ in 0..MAX_TURNS {
        let body = json!({
            "model": cfg.model,
            "max_tokens": 1024,
            "system": SYSTEM,
            "tools": tools::definitions(),
            "messages": messages,
        });
        let v = post(&url, &cfg.api_key, body)?;
        let stop = v.get("stop_reason").and_then(|s| s.as_str()).unwrap_or("");
        let content = v
            .get("content")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();

        if stop == "tool_use" {
            // Record the assistant turn verbatim, then run each tool call.
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
                let (text, is_error) = match tools::execute(name, &input) {
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

        // Final answer: collect text blocks and reduce to yes/no.
        let mut text = String::new();
        for block in &content {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                }
            }
        }
        return Ok(reduce_yes_no(&text));
    }

    Err("tool loop exceeded max turns".into())
}

fn post(url: &str, key: &str, body: Value) -> Result<Value, String> {
    match ureq::post(url)
        .set("authorization", &format!("Bearer {key}"))
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .send_json(body)
    {
        Ok(r) => r.into_json().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(code, r)) => {
            Err(format!("HTTP {code}: {}", r.into_string().unwrap_or_default()))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Map free text to yes/no by whichever word appears first.
fn reduce_yes_no(text: &str) -> bool {
    let lower = text.to_lowercase();
    match (lower.find("yes"), lower.find("no")) {
        (Some(y), Some(n)) => y <= n,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => false,
    }
}
