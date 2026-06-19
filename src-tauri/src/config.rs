use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::Manager;

const KEYRING_SERVICE: &str = "ca.magsolar.bit";
const KEYRING_USER: &str = "zai-api-key";

/// Non-secret settings, persisted as JSON in the app config dir.
#[derive(Serialize, Deserialize, Clone)]
pub struct Settings {
    pub base_url: String,
    pub model: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            base_url: "https://api.z.ai/api/anthropic".into(),
            model: "glm-5.2".into(),
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

/// API key lives in the OS keychain, never on disk.
pub fn get_key() -> Option<String> {
    let k = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .ok()?
        .get_password()
        .ok()?;
    (!k.is_empty()).then_some(k)
}

pub fn set_key(key: &str) -> Result<(), String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| e.to_string())?
        .set_password(key.trim())
        .map_err(|e| e.to_string())
}

/// Everything the agent needs to make a request. None if no key is configured.
pub struct AgentConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

pub fn load_agent_config(app: &tauri::AppHandle) -> Option<AgentConfig> {
    let s = load_settings(app);
    let api_key = get_key()?;
    Some(AgentConfig {
        base_url: s.base_url,
        model: s.model,
        api_key,
    })
}
