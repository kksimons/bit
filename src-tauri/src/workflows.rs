use crate::tools;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tauri::Manager;

/// One step of a workflow. Tagged by `type` so the model and the UI can author
/// these as plain JSON objects, e.g. {"type":"shell","command":"orb start"}.
#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Step {
    Shell { command: String },
    OpenApp { name: String },
    OpenUrl { url: String },
    AppleScript { script: String },
    /// Open one Ghostty window with these tabs.
    Ghostty { tabs: Vec<GhosttyTab> },
    /// Toggle Do Not Disturb.
    Focus { enabled: bool },
    /// Pause between steps (e.g. let OrbStack finish booting).
    Delay { ms: u64 },
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GhosttyTab {
    /// Working directory for the tab (supports a leading ~).
    pub dir: String,
    /// Optional command to run on open (stays in the shell, so dev servers live).
    #[serde(default)]
    pub command: Option<String>,
    /// Optional human label (UI only).
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Workflow {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub trigger_phrases: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub steps: Vec<Step>,
}

fn default_true() -> bool {
    true
}

fn step_kind(s: &Step) -> &'static str {
    match s {
        Step::Shell { .. } => "shell",
        Step::OpenApp { .. } => "open_app",
        Step::OpenUrl { .. } => "open_url",
        Step::AppleScript { .. } => "applescript",
        Step::Ghostty { .. } => "ghostty",
        Step::Focus { .. } => "focus",
        Step::Delay { .. } => "delay",
    }
}

// ---- storage (mirrors config.rs: JSON in the app config dir) ----

fn store_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    Some(app.path().app_config_dir().ok()?.join("workflows.json"))
}

pub fn load_all(app: &tauri::AppHandle) -> Vec<Workflow> {
    store_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_all(app: &tauri::AppHandle, all: &[Workflow]) -> Result<(), String> {
    let path = store_path(app).ok_or("no config dir")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(all).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

/// Generate a stable-ish id from the name plus a time suffix.
fn new_id(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", slug.trim_matches('-'), nanos)
}

/// Create or update a workflow, matched by case-insensitive name.
pub fn upsert(app: &tauri::AppHandle, mut wf: Workflow) -> Result<Workflow, String> {
    let mut all = load_all(app);
    if let Some(existing) = all.iter_mut().find(|w| w.name.eq_ignore_ascii_case(&wf.name)) {
        wf.id = existing.id.clone();
        *existing = wf.clone();
    } else {
        if wf.id.is_empty() {
            wf.id = new_id(&wf.name);
        }
        all.push(wf.clone());
    }
    save_all(app, &all)?;
    Ok(wf)
}

pub fn delete(app: &tauri::AppHandle, name: &str) -> Result<(), String> {
    let mut all = load_all(app);
    let before = all.len();
    all.retain(|w| !w.name.eq_ignore_ascii_case(name) && w.id != name);
    if all.len() == before {
        return Err(format!("no workflow named '{name}'"));
    }
    save_all(app, &all)
}

/// Find a workflow by name (case-insensitive, then loose contains match).
pub fn find(app: &tauri::AppHandle, name: &str) -> Option<Workflow> {
    let all = load_all(app);
    let q = name.to_lowercase();
    all.iter()
        .find(|w| w.name.eq_ignore_ascii_case(name))
        .or_else(|| all.iter().find(|w| w.name.to_lowercase().contains(&q)))
        .cloned()
}

// ---- execution ----

pub fn run(wf: &Workflow) -> Result<String, String> {
    for (i, step) in wf.steps.iter().enumerate() {
        let result = match step {
            Step::Shell { command } => tools::run_shell(command),
            Step::OpenApp { name } => tools::open_app(name),
            Step::OpenUrl { url } => tools::open_url(url),
            Step::AppleScript { script } => tools::run_applescript(script),
            Step::Focus { enabled } => tools::set_focus(*enabled),
            Step::Ghostty { tabs } => run_ghostty(tabs),
            Step::Delay { ms } => {
                std::thread::sleep(Duration::from_millis(*ms));
                Ok("waited".into())
            }
        };
        result.map_err(|e| format!("step {} ({}) failed: {e}", i + 1, step_kind(step)))?;
    }
    Ok(format!("ran workflow '{}' ({} steps)", wf.name, wf.steps.len()))
}

// ---- Ghostty multi-tab via native AppleScript (Ghostty >= 1.3) ----

fn ascript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn expand_tilde(dir: &str) -> String {
    if let Some(rest) = dir.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    } else if dir == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return home;
        }
    }
    dir.to_string()
}

/// Build an AppleScript that opens one Ghostty window with the given tabs, each
/// cd'd to its directory and (optionally) running a command in the live shell.
pub fn ghostty_script(tabs: &[GhosttyTab]) -> String {
    let mut s = String::from("tell application \"Ghostty\"\n    activate\n");
    for (i, tab) in tabs.iter().enumerate() {
        let cfg = format!("cfg{i}");
        s.push_str(&format!("    set {cfg} to new surface configuration\n"));
        s.push_str(&format!(
            "    set initial working directory of {cfg} to \"{}\"\n",
            ascript(&expand_tilde(&tab.dir))
        ));
        if let Some(cmd) = &tab.command {
            if !cmd.trim().is_empty() {
                s.push_str(&format!(
                    "    set initial input of {cfg} to \"{}\\n\"\n",
                    ascript(cmd)
                ));
            }
        }
        if i == 0 {
            s.push_str(&format!(
                "    set win to new window with configuration {cfg}\n"
            ));
        } else {
            s.push_str(&format!("    new tab in win with configuration {cfg}\n"));
        }
    }
    s.push_str("end tell\n");
    s
}

pub fn run_ghostty(tabs: &[GhosttyTab]) -> Result<String, String> {
    if tabs.is_empty() {
        return Err("no tabs specified".into());
    }
    let script = ghostty_script(tabs);
    let out = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "Ghostty AppleScript failed (is Ghostty 1.3+ installed and Automation allowed?): {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(format!("opened Ghostty with {} tab(s)", tabs.len()))
}
