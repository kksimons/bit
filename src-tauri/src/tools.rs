use serde_json::{json, Value};
use std::process::Command;

const MAX_OUTPUT: usize = 4000;

/// Anthropic-format tool definitions advertised to the model.
pub fn definitions() -> Value {
    json!([
        {
            "name": "run_shell",
            "description": "Run a command on the user's Mac through their login shell (zsh -lc), with their normal PATH and environment. Use this to launch apps and CLIs (e.g. open Ghostty, start OrbStack, bring up dev services) and for any terminal task. Returns combined stdout/stderr and a non-zero exit note on failure.",
            "input_schema": {
                "type": "object",
                "properties": { "command": { "type": "string", "description": "The shell command to run." } },
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
            "description": "Turn macOS Do Not Disturb / Focus on (true) or off (false). Requires the user to have created Shortcuts named exactly 'Bit DND On' and 'Bit DND Off'.",
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
        }
    ])
}

/// Execute a tool call and return text output (or an error string).
pub fn execute(name: &str, input: &Value) -> Result<String, String> {
    let arg = |key: &str| input.get(key).and_then(|v| v.as_str()).map(str::to_owned);

    match name {
        "run_shell" => {
            let cmd = arg("command").ok_or("missing command")?;
            run(Command::new("zsh").arg("-lc").arg(cmd))
        }
        "open_app" => {
            let app = arg("name").ok_or("missing name")?;
            run(Command::new("open").arg("-a").arg(app))
        }
        "open_url" => {
            let url = arg("url").ok_or("missing url")?;
            run(Command::new("open").arg(url))
        }
        "set_focus" => {
            let enabled = input
                .get("enabled")
                .and_then(|v| v.as_bool())
                .ok_or("missing enabled")?;
            let shortcut = if enabled { "Bit DND On" } else { "Bit DND Off" };
            // The shortcut must exist; `shortcuts run` exits 0 even when missing,
            // so verify presence first and report a clear, actionable error.
            let list = Command::new("shortcuts")
                .arg("list")
                .output()
                .map_err(|e| e.to_string())?;
            let names = String::from_utf8_lossy(&list.stdout);
            if !names.lines().any(|l| l.trim() == shortcut) {
                return Err(format!(
                    "Shortcut '{shortcut}' not found. The user must create Shortcuts named \
                     'Bit DND On' and 'Bit DND Off' (each a 'Set Focus' action) in the Shortcuts app."
                ));
            }
            run(Command::new("shortcuts").arg("run").arg(shortcut))
        }
        "run_applescript" => {
            let script = arg("script").ok_or("missing script")?;
            run(Command::new("osascript").arg("-e").arg(script))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn run(cmd: &mut Command) -> Result<String, String> {
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
