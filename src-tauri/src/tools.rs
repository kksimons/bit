use crate::workflows::{self, GhosttyTab, Step, Workflow};
use serde_json::{json, Value};
use std::process::Command;
use tauri::Manager;

const MAX_OUTPUT: usize = 4000;

/// Anthropic-format tool definitions advertised to the model.
pub fn definitions() -> Value {
    json!([
        {
            "name": "run_shell",
            "description": "Run a command on the user's Mac through their login shell (zsh -lc), with their normal PATH and environment. Use for launching CLIs, starting dev services, OrbStack/Docker containers (e.g. `orb start`, `docker start <name>`), file operations, etc. Returns combined stdout/stderr and notes non-zero exit codes.",
            "input_schema": {
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"]
            }
        },
        {
            "name": "open_app",
            "description": "Open a macOS application by name, e.g. \"Ghostty\", \"OrbStack\", \"Safari\".",
            "input_schema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "open_url",
            "description": "Open a URL in the user's default browser.",
            "input_schema": {
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"]
            }
        },
        {
            "name": "set_focus",
            "description": "Turn macOS Do Not Disturb on (true) or off (false).",
            "input_schema": {
                "type": "object",
                "properties": { "enabled": { "type": "boolean" } },
                "required": ["enabled"]
            }
        },
        {
            "name": "run_applescript",
            "description": "Run an AppleScript snippet via osascript to control macOS apps and system (volume, windows, System Events, etc.).",
            "input_schema": {
                "type": "object",
                "properties": { "script": { "type": "string" } },
                "required": ["script"]
            }
        },
        {
            "name": "open_terminal_tabs",
            "description": "Open one Ghostty terminal window with multiple tabs. Each tab opens in a directory and can run a command (commands run in the live shell, so dev servers keep running). Use this for ad-hoc multi-tab setups.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "tabs": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "dir": { "type": "string", "description": "Working directory (a leading ~ is allowed)." },
                                "command": { "type": "string", "description": "Optional command to run on open." },
                                "title": { "type": "string", "description": "Optional label." }
                            },
                            "required": ["dir"]
                        }
                    }
                },
                "required": ["tabs"]
            }
        },
        {
            "name": "move_bit",
            "description": "Move the Bit pet to the other screen (or opposite corner on a single screen). Use when the user shoos it away, e.g. 'get out of here', 'go away', 'move over there', 'shoo'.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "list_workflows",
            "description": "List the user's saved workflows (named multi-step routines) with their trigger phrases and steps.",
            "input_schema": { "type": "object", "properties": {} }
        },
        {
            "name": "run_workflow",
            "description": "Run a saved workflow by name. Use this when the user asks for something matching a workflow's name or trigger phrases (e.g. 'let's work on Heatsink').",
            "input_schema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "save_workflow",
            "description": "Create or update a named workflow (matched by name). Use this when the user asks you to set up / save / change a routine. A workflow is an ordered list of steps. Each step is an object with a \"type\": \
\"shell\" {command}, \
\"open_app\" {name}, \
\"open_url\" {url}, \
\"applescript\" {script}, \
\"focus\" {enabled:bool}, \
\"delay\" {ms:number}, or \
\"ghostty\" {tabs:[{dir, command?, title?}]} to open one terminal window with multiple tabs.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "trigger_phrases": { "type": "array", "items": { "type": "string" }, "description": "Phrases that should trigger this workflow." },
                    "steps": { "type": "array", "items": { "type": "object" }, "description": "Ordered steps; each has a 'type' field as described." }
                },
                "required": ["name", "steps"]
            }
        },
        {
            "name": "delete_workflow",
            "description": "Delete a saved workflow by name.",
            "input_schema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        }
    ])
}

/// Execute a tool call and return text output (or an error string).
pub fn execute(app: &tauri::AppHandle, name: &str, input: &Value) -> Result<String, String> {
    let arg = |key: &str| input.get(key).and_then(|v| v.as_str()).map(str::to_owned);

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
            let tabs: Vec<GhosttyTab> = serde_json::from_value(
                input.get("tabs").cloned().ok_or("missing tabs")?,
            )
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
            let steps: Vec<Step> = serde_json::from_value(
                input.get("steps").cloned().ok_or("missing steps")?,
            )
            .map_err(|e| format!("bad steps: {e}"))?;
            let trigger_phrases: Vec<String> = input
                .get("trigger_phrases")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let wf = Workflow {
                id: String::new(),
                name: name.clone(),
                trigger_phrases,
                enabled: true,
                steps,
            };
            workflows::upsert(app, wf)?;
            Ok(format!("saved workflow '{name}'"))
        }
        "delete_workflow" => {
            let n = arg("name").ok_or("missing name")?;
            workflows::delete(app, &n)?;
            Ok(format!("deleted workflow '{n}'"))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

// ---- primitives (shared by AI tools and the workflow engine) ----

pub fn run_shell(command: &str) -> Result<String, String> {
    capture(Command::new("zsh").arg("-lc").arg(command))
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
