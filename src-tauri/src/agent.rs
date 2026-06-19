use crate::config::AgentConfig;
use serde_json::json;

const SYSTEM: &str = "You are Bit, a desktop companion modeled on the Bit from the film TRON. \
You can only answer the user out loud with a single word: \"yes\" or \"no\". \
Interpret what the user wants and respond truthfully and helpfully. Answer \"yes\" if the \
answer is affirmative or if you can/will do what they ask; answer \"no\" otherwise. \
Reply with exactly one lowercase word: yes or no.";

/// Ask the model and reduce its reply to a yes (true) / no (false) verdict.
/// Uses the Anthropic Messages wire format against the configured base URL.
pub fn ask(cfg: &AgentConfig, transcript: &str) -> Result<bool, String> {
    let url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'));
    let body = json!({
        "model": cfg.model,
        "max_tokens": 64,
        "system": SYSTEM,
        "messages": [ { "role": "user", "content": transcript } ],
    });

    let resp = ureq::post(&url)
        .set("authorization", &format!("Bearer {}", cfg.api_key))
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .send_json(body);

    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let detail = r.into_string().unwrap_or_default();
            return Err(format!("HTTP {code}: {detail}"));
        }
        Err(e) => return Err(e.to_string()),
    };

    let v: serde_json::Value = resp.into_json().map_err(|e| e.to_string())?;

    // Anthropic Messages response: { content: [ { type: "text", text }, ... ] }
    let mut text = String::new();
    if let Some(blocks) = v.get("content").and_then(|c| c.as_array()) {
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                }
            }
        }
    }

    Ok(reduce_yes_no(&text))
}

/// Map free text to a yes/no, by whichever of the two words appears first.
fn reduce_yes_no(text: &str) -> bool {
    let lower = text.to_lowercase();
    match (lower.find("yes"), lower.find("no")) {
        (Some(y), Some(n)) => y <= n,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => false,
    }
}
