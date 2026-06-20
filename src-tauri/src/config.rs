use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::Manager;

/// Non-secret settings, persisted as JSON in the app config dir.
#[derive(Serialize, Deserialize, Clone)]
pub struct Settings {
    pub base_url: String,
    pub model: String,
    /// When false (default), the model cannot run raw shell/AppleScript — only
    /// your enabled workflows + safe built-ins. When true, raw execution tools
    /// are re-enabled for power users who accept the risk.
    #[serde(default)]
    pub developer_mode: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            base_url: "https://api.z.ai/api/anthropic".into(),
            model: "glm-5.2".into(),
            developer_mode: false,
        }
    }
}

fn settings_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    Some(app.path().app_config_dir().ok()?.join("settings.json"))
}

pub fn load_settings(app: &tauri::AppHandle) -> Settings {
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_settings(app: &tauri::AppHandle, s: &Settings) -> Result<(), String> {
    let path = settings_path(app).ok_or("no config dir")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

fn key_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    Some(app.path().app_config_dir().ok()?.join("api_key"))
}

/// API key stored in a 0600 file in the app's private config dir. (Dev builds
/// get a fresh ad-hoc code signature each rebuild, which breaks Keychain ACLs
/// and forced re-entry every launch; a file avoids that.)
pub fn get_key(app: &tauri::AppHandle) -> Option<String> {
    let s = std::fs::read_to_string(key_path(app)?).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

pub fn set_key(app: &tauri::AppHandle, key: &str) -> Result<(), String> {
    let path = key_path(app).ok_or("no config dir")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, key.trim()).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Everything the agent needs to make a request. None if no key is configured.
pub struct AgentConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

pub fn load_agent_config(app: &tauri::AppHandle) -> Option<AgentConfig> {
    let s = load_settings(app);
    let api_key = get_key(app)?;
    Some(AgentConfig {
        base_url: s.base_url,
        model: s.model,
        api_key,
    })
}
