use crate::workflows::{self, GhosttyTab, Step, Workflow};
use serde_json::{json, Value};
use std::process::Command;
use tauri::Manager;

const MAX_OUTPUT: usize = 4000;

/// Tools the model may call. By default the model gets only SAFE, bounded tools
/// (run your enabled workflows + open apps/URLs + Focus + move). Raw execution
/// (shell, AppleScript, ad-hoc terminal tabs) is added only in Developer mode.
pub fn definitions(developer_mode: bool) -> Value {
    let mut tools = vec![
        json!({
            "name": "open_app",
            "description": "Open a macOS application by name, e.g. \"Ghostty\", \"OrbStack\", \"Safari\".",
            "input_schema": { "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"] }
        }),
        json!({
            "name": "open_url",
            "description": "Open a URL in the user's default browser.",
            "input_schema": { "type": "object", "properties": { "url": { "type": "string" } }, "required": ["url"] }
        }),
        json!({
            "name": "set_focus",
            "description": "Turn macOS Do Not Disturb on (true) or off (false).",
            "input_schema": { "type": "object", "properties": { "enabled": { "type": "boolean" } }, "required": ["enabled"] }
        }),
        json!({
            "name": "move_bit",
            "description": "Move the Bit pet to the other screen (or opposite corner on a single screen). Use when the user shoos it away, e.g. 'get out of here', 'go away', 'shoo'.",
            "input_schema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "list_workflows",
            "description": "List the user's saved workflows (named multi-step routines) with their trigger phrases and steps.",
            "input_schema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "run_workflow",
            "description": "Run a saved, enabled workflow by name. Use when the user's request matches a workflow's name or trigger phrases (e.g. 'let's work on Heatsink').",
            "input_schema": { "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"] }
        }),
        json!({
            "name": "save_workflow",
            "description": "Draft a named workflow when the user asks you to set up/save a routine. IMPORTANT: drafts are saved DISABLED — tell the user to review and enable it in Settings before it can run. Steps are objects with a \"type\": \
        \"shell\" {command}, \"open_app\" {name}, \"open_url\" {url}, \"applescript\" {script}, \"focus\" {enabled:bool}, \"delay\" {ms:number}, or \"ghostty\" {tabs:[{dir, command?, title?}]}.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "trigger_phrases": { "type": "array", "items": { "type": "string" } },
                    "steps": { "type": "array", "items": { "type": "object" } }
                },
                "required": ["name", "steps"]
            }
        }),
    ];

    if developer_mode {
        tools.push(json!({
            "name": "run_shell",
            "description": "Run a command via the login shell (zsh -lc). Developer mode only.",
            "input_schema": { "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] }
        }));
        tools.push(json!({
            "name": "run_applescript",
            "description": "Run an AppleScript snippet via osascript. Developer mode only.",
            "input_schema": { "type": "object", "properties": { "script": { "type": "string" } }, "required": ["script"] }
        }));
        tools.push(json!({
            "name": "open_terminal_tabs",
            "description": "Open one Ghostty window with multiple tabs (each a dir + optional command). Developer mode only.",
            "input_schema": {
                "type": "object",
                "properties": { "tabs": { "type": "array", "items": {
                    "type": "object",
                    "properties": { "dir": {"type":"string"}, "command": {"type":"string"}, "title": {"type":"string"} },
                    "required": ["dir"]
                } } },
                "required": ["tabs"]
            }
        }));
    }

    Value::Array(tools)
}

fn developer_mode(app: &tauri::AppHandle) -> bool {
    crate::config::load_settings(app).developer_mode
}

/// Execute a tool call and return text output (or an error string).
pub fn execute(app: &tauri::AppHandle, name: &str, input: &Value) -> Result<String, String> {
    let arg = |key: &str| input.get(key).and_then(|v| v.as_str()).map(str::to_owned);

    // Raw-execution tools are only available in Developer mode.
    if matches!(name, "run_shell" | "run_applescript" | "open_terminal_tabs")
        && !developer_mode(app)
    {
        return Err(format!(
            "'{name}' is disabled. Bit only runs your saved workflows and safe actions. \
             Turn on Developer mode in Settings to allow raw commands."
        ));
    }

    match name {
        "run_shell" => run_shell(&arg("command").ok_or("missing command")?),
        "open_app" => open_app(&arg("name").ok_or("missing name")?),
        "open_url" => open_url(&arg("url").ok_or("missing url")?),
        "run_applescript" => run_applescript(&arg("script").ok_or("missing script")?),
        "set_focus" => {
            let enabled = input
                .get("enabled")
                .and_then(|v| v.as_bool())
                .ok_or("missing enabled")?;
            set_focus(enabled)
        }
        "move_bit" => {
            let win = app
                .get_webview_window("bit")
                .ok_or("bit window not found")?;
            crate::motion::shoo(&win)
        }
        "open_terminal_tabs" => {
            let tabs: Vec<GhosttyTab> =
                serde_json::from_value(input.get("tabs").cloned().ok_or("missing tabs")?)
                    .map_err(|e| format!("bad tabs: {e}"))?;
            workflows::run_ghostty(&tabs)
        }
        "list_workflows" => {
            let all = workflows::load_all(app);
            serde_json::to_string(&all).map_err(|e| e.to_string())
        }
        "run_workflow" => {
            let n = arg("name").ok_or("missing name")?;
            let wf = workflows::find(app, &n).ok_or(format!("no workflow named '{n}'"))?;
            if !wf.enabled {
                return Err(format!("workflow '{}' is disabled", wf.name));
            }
            workflows::run(&wf)
        }
        "save_workflow" => {
            let name = arg("name").ok_or("missing name")?;
            let steps: Vec<Step> =
                serde_json::from_value(input.get("steps").cloned().ok_or("missing steps")?)
                    .map_err(|e| format!("bad steps: {e}"))?;
            let trigger_phrases: Vec<String> = input
                .get("trigger_phrases")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let wf = Workflow {
                id: String::new(),
                name: name.clone(),
                trigger_phrases,
                // Model-authored workflows are saved DISABLED — the user must
                // review and enable them in Settings before they can run.
                enabled: false,
                steps,
            };
            workflows::upsert(app, wf)?;
            Ok(format!(
                "Saved '{name}' as a disabled draft. The user must review and enable it in Settings before it can run."
            ))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

// ---- primitives (shared by AI tools and the workflow engine) ----

pub fn run_shell(command: &str) -> Result<String, String> {
    if let Some(pat) = is_catastrophic(command) {
        return Err(format!(
            "Refused: command matches a blocked destructive pattern ({pat}). \
             Edit the workflow if this is intentional."
        ));
    }
    capture(Command::new("zsh").arg("-lc").arg(command))
}

/// Hard backstop denylist for the genuinely catastrophic — applies even to your
/// own workflows and Developer mode. Conservative: targets irreversible/system
/// damage, not ordinary `rm -rf ./node_modules`.
fn is_catastrophic(cmd: &str) -> Option<&'static str> {
    let c = cmd.to_lowercase();
    let c = c.replace(['"', '\''], ""); // ignore quoting around targets
    let patterns: [(&str, &str); 11] = [
        ("rm -rf /", "rm -rf / (root)"),
        ("rm -rf /*", "rm -rf /*"),
        ("rm -fr /", "rm -fr /"),
        ("rm -rf ~", "rm -rf home"),
        ("rm -rf $home", "rm -rf $HOME"),
        ("sudo ", "sudo (privilege escalation)"),
        ("mkfs", "mkfs (format)"),
        ("diskutil erase", "diskutil erase"),
        ("of=/dev/", "dd to a device"),
        (":(){:|:&};:", "fork bomb"),
        ("| sh", "pipe-to-shell"),
    ];
    // also catch `curl|wget ... | bash`
    if (c.contains("curl") || c.contains("wget")) && (c.contains("| sh") || c.contains("| bash")) {
        return Some("remote-pipe-to-shell");
    }
    patterns
        .iter()
        .find(|(p, _)| c.contains(p))
        .map(|(_, label)| *label)
}

pub fn open_app(name: &str) -> Result<String, String> {
    capture(Command::new("open").arg("-a").arg(name))
}

pub fn open_url(url: &str) -> Result<String, String> {
    capture(Command::new("open").arg(url))
}

pub fn run_applescript(script: &str) -> Result<String, String> {
    capture(Command::new("osascript").arg("-e").arg(script))
}

pub fn set_focus(enabled: bool) -> Result<String, String> {
    let shortcut = if enabled { "Bit DND On" } else { "Bit DND Off" };
    // `shortcuts run` exits 0 even when the shortcut is missing, so verify first.
    let list = Command::new("shortcuts")
        .arg("list")
        .output()
        .map_err(|e| e.to_string())?;
    let names = String::from_utf8_lossy(&list.stdout);
    if !names.lines().any(|l| l.trim() == shortcut) {
        return Err(format!(
            "Do Not Disturb isn't set up yet (missing the '{shortcut}' Shortcut). \
             Open Bit Settings and use 'Set up Do Not Disturb'."
        ));
    }
    capture(Command::new("shortcuts").arg("run").arg(shortcut))
}

fn capture(cmd: &mut Command) -> Result<String, String> {
    let output = cmd.output().map_err(|e| e.to_string())?;
    let mut s = String::from_utf8_lossy(&output.stdout).into_owned();
    let err = String::from_utf8_lossy(&output.stderr);
    if !err.trim().is_empty() {
        s.push_str("\n[stderr] ");
        s.push_str(&err);
    }
    if !output.status.success() {
        s.push_str(&format!(
            "\n[exit code: {}]",
            output.status.code().unwrap_or(-1)
        ));
        // Surface failures as errors so the agent answers honestly.
        return Err(s);
    }
    if s.len() > MAX_OUTPUT {
        s.truncate(MAX_OUTPUT);
        s.push_str("\n…[truncated]");
    }
    if s.trim().is_empty() {
        s = "(no output; succeeded)".into();
    }
    Ok(s)
}
