mod agent;
mod audio;
mod config;
pub mod mcp;
mod motion;
mod stt;
mod tools;
mod workflows;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;

/// Default square window size for the Bit, in logical pixels. The on-screen
/// size multiplier resizes around this base.
const BIT_BASE_SIZE: f64 = 260.0;

/// Toggle dictation: press once to start, again to stop (Ctrl+Opt+Space).
fn talk_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space)
}

struct AppState {
    recorder: audio::Recorder,
    stt: Arc<stt::Stt>,
    /// Last time the talk shortcut toggled — time-debounces duplicate/auto-repeat
    /// shortcut events (macOS can double-fire), so one press = one toggle.
    last_toggle: Mutex<Instant>,
    /// Set while the user is dragging the Bit (pauses the click-through poller).
    dragging: Arc<AtomicBool>,
    /// Live MCP server connections, keyed by server name.
    mcp: mcp::Registry,
}

#[derive(serde::Serialize)]
struct SettingsView {
    provider: String,
    base_url: String,
    model: String,
    has_key: bool,
    developer_mode: bool,
    size: f64,
}

#[tauri::command]
fn get_settings(app: tauri::AppHandle) -> SettingsView {
    let s = config::load_settings(&app);
    SettingsView {
        provider: s.provider,
        base_url: s.base_url,
        model: s.model,
        has_key: config::get_key(&app).is_some(),
        developer_mode: s.developer_mode,
        size: s.size,
    }
}

#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    settings: config::Settings,
    api_key: Option<String>,
) -> Result<(), String> {
    config::save_settings(&app, &settings)?;
    if let Some(k) = api_key {
        if !k.is_empty() {
            config::set_key(&app, &k)?;
        }
    }
    Ok(())
}

// ---- workflows ----

#[tauri::command]
fn get_workflows(app: tauri::AppHandle) -> Vec<workflows::Workflow> {
    workflows::load_all(&app)
}

#[tauri::command]
fn save_workflow(
    app: tauri::AppHandle,
    workflow: workflows::Workflow,
) -> Result<workflows::Workflow, String> {
    workflows::upsert(&app, workflow)
}

#[tauri::command]
fn delete_workflow(app: tauri::AppHandle, name: String) -> Result<(), String> {
    workflows::delete(&app, &name)
}

#[tauri::command]
fn run_workflow(app: tauri::AppHandle, name: String) -> Result<String, String> {
    let wf = workflows::find(&app, &name).ok_or(format!("no workflow named '{name}'"))?;
    workflows::run(&wf)
}

// ---- Bit size ----

/// Resize the Bit overlay around its base size. `scale` is the user's size
/// multiplier (clamped to the slider range). The passthrough poller reads the
/// window size live, so the click-through hit disc follows automatically.
fn apply_bit_size(app: &tauri::AppHandle, scale: f64) {
    let Some(win) = app.get_webview_window("bit") else {
        return;
    };
    let px = BIT_BASE_SIZE * scale.clamp(0.5, 2.0);
    let _ = win.set_size(tauri::LogicalSize::new(px, px));
}

#[tauri::command]
fn set_bit_size(app: tauri::AppHandle, scale: f64) {
    apply_bit_size(&app, scale);
}

// ---- Do Not Disturb ----

#[tauri::command]
fn dnd_status() -> bool {
    let Ok(out) = std::process::Command::new("shortcuts").arg("list").output() else {
        return false;
    };
    let names = String::from_utf8_lossy(&out.stdout);
    let has = |n: &str| names.lines().any(|l| l.trim() == n);
    has("Bit DND On") && has("Bit DND Off")
}

#[tauri::command]
fn set_dnd(enabled: bool) -> Result<String, String> {
    tools::set_focus(enabled)
}

/// Open the Shortcuts app so the user can create the two required Shortcuts.
#[tauri::command]
fn setup_dnd() -> Result<(), String> {
    std::process::Command::new("open")
        .arg("-a")
        .arg("Shortcuts")
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---- drag / fling physics ----

/// Begin a custom drag: a background thread moves the Bit window to follow the
/// cursor and tracks release velocity, so letting go can fling it with momentum.
#[tauri::command]
fn start_drag(app: tauri::AppHandle) {
    let state = app.state::<AppState>();
    if state.dragging.swap(true, Ordering::Relaxed) {
        return; // already dragging
    }
    let dragging = state.dragging.clone();
    let Some(win) = app.get_webview_window("bit") else {
        return;
    };
    std::thread::spawn(move || {
        let start_cur = app.cursor_position().unwrap_or_default();
        let start_win = win.outer_position().unwrap_or_default();
        let off_x = start_cur.x - start_win.x as f64;
        let off_y = start_cur.y - start_win.y as f64;
        let mut last = start_cur;
        let mut vel = (0.0_f64, 0.0_f64); // px per ms, smoothed
        while dragging.load(Ordering::Relaxed) {
            let cur = match app.cursor_position() {
                Ok(c) => c,
                Err(_) => break,
            };
            let nx = (cur.x - off_x).round() as i32;
            let ny = (cur.y - off_y).round() as i32;
            let _ = win.set_position(tauri::PhysicalPosition::new(nx, ny));
            // EMA of cursor velocity (tick ≈ 8ms)
            let vx = (cur.x - last.x) / 8.0;
            let vy = (cur.y - last.y) / 8.0;
            vel.0 = vel.0 * 0.6 + vx * 0.4;
            vel.1 = vel.1 * 0.6 + vy * 0.4;
            last = cur;
            std::thread::sleep(Duration::from_millis(8));
        }
        motion::fling(&win, vel);
    });
}

#[tauri::command]
fn end_drag(app: tauri::AppHandle) {
    app.state::<AppState>()
        .dragging
        .store(false, Ordering::Relaxed);
}

// ---- MCP (external tool servers) ----

/// All configured MCP servers + their live connection status, for the UI.
#[derive(serde::Serialize)]
struct McpServerView {
    name: String,
    transport: String,
    command: String,
    args: Vec<String>,
    env: std::collections::BTreeMap<String, String>,
    url: String,
    enabled: bool,
    preset: String,
    connected: bool,
    tool_count: usize,
    error: Option<String>,
}

#[tauri::command]
fn get_mcp_servers(app: tauri::AppHandle) -> Vec<McpServerView> {
    let registry = app.state::<AppState>().mcp.clone();
    mcp::load_all(&app)
        .into_iter()
        .map(|s| {
            let connected = registry.is_connected(&s.name);
            McpServerView {
                tool_count: if connected {
                    registry.tool_count(&s)
                } else {
                    0
                },
                connected,
                error: registry.last_error(&s.name),
                name: s.name.clone(),
                transport: s.transport.clone(),
                command: s.command,
                args: s.args,
                env: s.env,
                url: s.url,
                enabled: s.enabled,
                preset: s.preset,
            }
        })
        .collect()
}

#[tauri::command]
fn save_mcp_server(
    app: tauri::AppHandle,
    server: mcp::McpServer,
) -> Result<mcp::McpServer, String> {
    app.state::<AppState>().mcp.invalidate(&server.name);
    let saved = mcp::upsert(&app, server)?;
    // If enabled, start warming the (new) connection now.
    if saved.enabled {
        spawn_mcp_prewarm(app.clone());
    }
    Ok(saved)
}

#[tauri::command]
fn delete_mcp_server(app: tauri::AppHandle, name: String) -> Result<(), String> {
    app.state::<AppState>().mcp.invalidate(&name);
    // Also remove any stored OAuth token for an http server so it’s fully gone.
    if let Some(path) = mcp::oauth::FileCredentialStore::path_for(&app, &name) {
        let _ = std::fs::remove_file(path);
    }
    mcp::delete(&app, &name)
}

/// Add a remote (HTTP) MCP server via the OAuth flow: discover → register →
/// open browser for sign-in → capture the loopback callback → persist token.
/// On success the server is saved (enabled) and pre-warmed. The caller in the
/// UI just supplies a name + URL — this is the one-line “Add a service” UX.
#[tauri::command]
fn add_http_server(app: tauri::AppHandle, name: String, url: String) -> Result<String, String> {
    // Run OAuth first; if the user bails or it fails, we save nothing.
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("no config dir: {e}"))?;
    // run_oauth_flow normalizes the URL (e.g. /sse → /mcp) and returns the one
    // it actually used; save THAT so the token's audience matches connect.
    let resolved_url = mcp::http::run_oauth_flow(&dir, &name, &url)?;
    let server = mcp::McpServer {
        name,
        transport: "http".into(),
        command: String::new(),
        args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        url: resolved_url.clone(),
        enabled: true,
        preset: String::new(),
        disabled_tools: Vec::new(),
    };
    mcp::upsert(&app, server)?;
    spawn_mcp_prewarm(app);
    Ok(resolved_url)
}

/// Probe one server: connect now, run tools/list, and report status. Lets the
/// UI show “connected · N tools” on demand (and after the user pastes a token).
#[tauri::command]
fn test_mcp_server(app: tauri::AppHandle, name: String) -> Result<usize, String> {
    let server = mcp::load_all(&app)
        .into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(&name))
        .ok_or_else(|| format!("no MCP server named '{name}'"))?;
    if !server.enabled {
        return Err("server is disabled".into());
    }
    // Force a fresh connect attempt (drops any cached error).
    app.state::<AppState>().mcp.invalidate(&name);
    app.state::<AppState>()
        .mcp
        .ensure(&app, &server)
        .map(|t| t.len())
}

/// List one server's tools with their enabled + destructive state, for the
/// “Manage tools” UI. Connects if needed.
#[tauri::command]
fn get_mcp_tools(app: tauri::AppHandle, name: String) -> Result<Vec<mcp::ToolView>, String> {
    mcp::tools_view(&app, &name)
}

/// Toggle a single tool on/off, OR bulk-replace the whole denylist. `disabled`
/// is the FULL new denylist (server-side tool names) — the UI computes it
/// locally and sends the complete set, which keeps this command simple and
/// idempotent. Cheap: rewrites `mcp.json` only, no reconnect.
#[tauri::command]
fn set_mcp_disabled_tools(
    app: tauri::AppHandle,
    name: String,
    disabled: Vec<String>,
) -> Result<(), String> {
    mcp::set_disabled_tools(&app, &name, disabled)
}

// ---- Transcription models ----

/// A model's UI-facing metadata + on-disk state.
#[derive(serde::Serialize)]
struct ModelView {
    id: String,
    name: String,
    description: String,
    languages: String,
    size_mb: u64,
    downloaded: bool,
    active: bool,
}

#[tauri::command]
fn get_stt_models(app: tauri::AppHandle) -> Vec<ModelView> {
    let app_data = app.path().app_data_dir().unwrap_or_default();
    let active = config::load_settings(&app).stt_model;
    stt::MODELS
        .iter()
        .map(|m| ModelView {
            downloaded: stt::model_ready(&app_data, m.id),
            active: m.id == active,
            id: m.id.into(),
            name: m.name.into(),
            description: m.description.into(),
            languages: m.languages.into(),
            size_mb: m.size_mb,
        })
        .collect()
}

/// Download a model's files. Emits `stt-download-progress` events as each
/// file completes so the UI can show a progress bar. Runs synchronously — the
/// UI calls this from a background task and awaits completion.
#[tauri::command]
fn download_stt_model(app: tauri::AppHandle, model_id: String) -> Result<(), String> {
    let app_data = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let app_for_progress = app.clone();
    stt::download_model(&app_data, &model_id, |done, total, name| {
        let _ = app_for_progress.emit(
            "stt-download-progress",
            serde_json::json!({ "model": model_id, "done": done, "total": total, "file": name }),
        );
    })?;
    let _ = app.emit(
        "stt-download-progress",
        serde_json::json!({ "model": model_id, "done": 0, "total": 0, "file": "__complete__" }),
    );
    Ok(())
}

/// Set the active transcription model. Takes effect on the next recording —
/// `Stt::ensure_loaded` reloads when the id changes.
#[tauri::command]
fn set_stt_model(app: tauri::AppHandle, model_id: String) -> Result<(), String> {
    // Validate the id is a known model so a bad UI call can't break STT.
    if !stt::MODELS.iter().any(|m| m.id == model_id) {
        return Err(format!("unknown model: {model_id}"));
    }
    let mut s = config::load_settings(&app);
    s.stt_model = model_id;
    config::save_settings(&app, &s)
}

/// Remove a downloaded model from disk (the user picked a different one and
/// wants the space back). Refuses if it's the active model.
#[tauri::command]
fn delete_stt_model(app: tauri::AppHandle, model_id: String) -> Result<(), String> {
    let active = config::load_settings(&app).stt_model;
    if active == model_id {
        return Err("can’t delete the active model — switch first".into());
    }
    let app_data = app.path().app_data_dir().map_err(|e| e.to_string())?;
    stt::delete_model(&app_data, &model_id)
}

/// One credential field a preset asks the UI to collect.
#[derive(serde::Serialize)]
struct PresetFieldView {
    env_key: String,
    label: String,
    placeholder: String,
    secret: bool,
}

/// Gallery presets the UI renders as one-click “Add X” buttons.
#[derive(serde::Serialize)]
struct PresetView {
    id: String,
    label: String,
    description: String,
    command: String,
    args: Vec<String>,
    fields: Vec<PresetFieldView>,
}

#[tauri::command]
fn get_mcp_presets() -> Vec<PresetView> {
    mcp::presets()
        .iter()
        .map(|p| PresetView {
            id: p.id.into(),
            label: p.label.into(),
            description: p.description.into(),
            command: p.command.into(),
            args: p.args.iter().map(|s| (*s).into()).collect(),
            fields: p
                .fields
                .iter()
                .map(|f| PresetFieldView {
                    env_key: f.env_key.into(),
                    label: f.label.into(),
                    placeholder: f.placeholder.into(),
                    secret: f.secret,
                })
                .collect(),
        })
        .collect()
}

/// Background-warm every enabled MCP server. Safe to call repeatedly; already-
/// connected servers are no-ops. Runs off the calling thread so it never blocks
/// the UI or the agent loop.
fn spawn_mcp_prewarm(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let registry = app.state::<AppState>().mcp.clone();
        for server in mcp::load_all(&app) {
            if !server.enabled {
                continue;
            }
            if let Err(e) = registry.ensure(&app, &server) {
                eprintln!("[bit] mcp '{}': {e}", server.name);
            }
        }
    });
}

// ---- Launch on startup ----
//
// We wrap the autostart plugin in our own commands rather than letting the
// frontend call it directly, so we can refuse in development builds. Registering
// a login item that points at `target/debug/bit` is a footgun: that binary loads
// its UI from the Vite dev server, which isn't running after a reboot — so the
// app starts to a blank screen. Force users onto a real `tauri build` install.

/// Read whether launch-on-login is currently registered with the OS.
#[tauri::command]
fn autostart_state(app: tauri::AppHandle) -> bool {
    // Reading is always safe — it just inspects the existing login item.
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Register or unregister Bit as a login item. Refuses in debug builds.
#[tauri::command]
fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    // `cfg!(debug_assertions)` is a const bool: true under `tauri dev` (debug
    // profile), false under `tauri build` (release). It compiles out cleanly and
    // is far more reliable than sniffing current_exe().
    if cfg!(debug_assertions) {
        return Err(
            "Launch on startup is disabled in development builds — the dev binary \
             can’t find its UI after a reboot. Build and install the app first: \
             `bun run tauri build`, then open the resulting .app and re-enable this."
                .into(),
        );
    }
    let mgr = app.autolaunch();
    if enabled {
        mgr.enable().map_err(|e| e.to_string())
    } else {
        mgr.disable().map_err(|e| e.to_string())
    }
}

/// Tell the Bit overlay to change form/state.
fn set_bit_state(app: &tauri::AppHandle, state: &str) {
    if let Some(win) = app.get_webview_window("bit") {
        let _ = win.emit("bit-state", state);
    }
}

/// Emit the final yes/no verdict and how many times to say it (1..=3).
fn emit_verdict(app: &tauri::AppHandle, yes: bool, times: u8) {
    if let Some(win) = app.get_webview_window("bit") {
        let _ = win.emit(
            "bit-verdict",
            serde_json::json!({ "kind": if yes { "yes" } else { "no" }, "times": times }),
        );
    }
}

/// Open (or focus) the settings window. On macOS we temporarily switch to a
/// regular activation policy so the window can take focus, then drop back to
/// accessory (no Dock icon) when it closes.
fn open_settings(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("config") {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }

    // Become a regular app so the new window can take focus, then build and
    // explicitly focus it — otherwise (from accessory mode) it opens behind and
    // needs a second click to come forward.
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(ActivationPolicy::Regular);

    match WebviewWindowBuilder::new(app, "config", WebviewUrl::App("config.html".into()))
        .title("Bit Settings")
        .inner_size(440.0, 540.0)
        .resizable(true)
        .focused(true)
        .build()
    {
        Ok(win) => {
            let _ = win.show();
            let _ = win.set_focus();
        }
        Err(e) => eprintln!("[bit] failed to open settings: {e}"),
    }
}

/// Background poll: keep the overlay click-through everywhere except over the
/// Bit itself, so empty pixels pass clicks to whatever is behind it. We poll the
/// global cursor against the window's center so it works even while the window is
/// ignoring cursor events (and thus receiving no DOM mouse events).
fn spawn_passthrough(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    dragging: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut last: Option<bool> = None;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(70));
            // While dragging/flinging, leave the window interactive and let the
            // motion code own positioning.
            if dragging.load(Ordering::Relaxed) {
                if last != Some(true) {
                    let _ = window.set_ignore_cursor_events(false);
                    last = Some(true);
                }
                continue;
            }
            let (pos, size) = match (window.outer_position(), window.outer_size()) {
                (Ok(p), Ok(s)) => (p, s),
                _ => continue,
            };
            let cursor = match app.cursor_position() {
                Ok(c) => c,
                Err(_) => continue,
            };
            let cx = pos.x as f64 + size.width as f64 / 2.0;
            let cy = pos.y as f64 + size.height as f64 / 2.0;
            let dx = cursor.x - cx;
            let dy = cursor.y - cy;
            // interactive within a centered disc covering the Bit (+ margin)
            let r = size.width.min(size.height) as f64 * 0.5 * 0.8;
            let over = dx * dx + dy * dy <= r * r;
            if last != Some(over) {
                last = Some(over);
                let _ = window.set_ignore_cursor_events(!over);
            }
        }
    });
}

/// A press starts listening (if idle) or stops early (if already listening).
/// Listening normally ends on its own when you go quiet (see the silence watcher).
fn on_toggle(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    if !state.recorder.is_recording() {
        // Refuse visibly if the mic never came up (no permission / no device).
        // Previously this silently started a recording that captured nothing,
        // then hung in "thinking" for ~30s — exactly the stuck-on-thinking bug.
        if !state.recorder.available() {
            let reason = state
                .recorder
                .last_error()
                .unwrap_or_else(|| "microphone unavailable".into());
            eprintln!("[bit] push-to-talk refused: {reason}");
            // Flash “no” once so the user sees the press registered but failed,
            // rather than wondering if the shortcut is dead.
            emit_verdict(app, false, 1);
            return;
        }
        // First-run guard: if the user hasn't downloaded any transcription
        // model yet, don't silently start a 700 MB download mid-recording
        // (the old behavior, which looked like a hang). Open Settings to the
        // Transcription section so they can pick one. We can't deep-link to a
        // section, so we just open the window — the empty state there explains
        // what's needed.
        let app_data = app.path().app_data_dir().unwrap_or_default();
        let active = config::load_settings(app).stt_model;
        if !stt::model_ready(&app_data, &active) {
            eprintln!("[bit] no transcription model downloaded — opening Settings");
            open_settings(app);
            return;
        }
        println!("[bit] REC start");
        state.recorder.start();
        set_bit_state(app, "listening");
        spawn_silence_watcher(app.clone());
    } else {
        finish_recording(app);
    }
}

/// Auto-end listening when the user stops talking: once speech is detected, a
/// ~1.1s lull (or a 30s cap) finishes the recording. So one press is enough —
/// no second press needed.
fn spawn_silence_watcher(app: tauri::AppHandle) {
    let recorder = app.state::<AppState>().recorder.clone();
    std::thread::spawn(move || {
        let rate = recorder.sample_rate().max(1) as usize;
        let window = rate / 8; // ~125ms RMS window
        const SPEECH_RMS: f32 = 0.012;
        let silence_ticks_needed = 11; // ~1.1s at 100ms ticks
        let max_ticks = 300; // 30s safety cap
        let mut spoke = false;
        let mut silent_ticks = 0;
        let mut ticks = 0;
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if !recorder.is_recording() {
                return; // ended elsewhere (manual press)
            }
            ticks += 1;
            let rms = recorder.recent_rms(window);
            if rms > SPEECH_RMS {
                spoke = true;
                silent_ticks = 0;
            } else if spoke {
                silent_ticks += 1;
            }
            if (spoke && silent_ticks >= silence_ticks_needed) || ticks >= max_ticks {
                finish_recording(&app);
                return;
            }
        }
    });
}

/// Stop recording (if still active) and run transcription → agent → yes/no.
fn finish_recording(app: &tauri::AppHandle) {
    let Some((samples, rate)) = app.state::<AppState>().recorder.stop() else {
        return; // already finished by the other path
    };
    let secs = samples.len() as f64 / rate.max(1) as f64;
    println!("[bit] REC stop: {} samples (~{secs:.1}s)", samples.len());
    set_bit_state(app, "thinking");
    let stt = app.state::<AppState>().stt.clone();
    let app = app.clone();
    std::thread::spawn(move || {
        println!("[bit] worker: resampling + loading STT model…");
        let samples_16k = audio::resample_to_16k(&samples, rate);
        let active_model = config::load_settings(&app).stt_model;
        let text = match stt.ensure_loaded(&active_model).and_then(|()| {
            println!("[bit] worker: STT model loaded, transcribing…");
            stt.transcribe(&samples_16k)
        }) {
            Ok(t) => t.trim().to_string(),
            Err(e) => {
                eprintln!("[bit] stt error: {e}");
                String::new()
            }
        };
        println!("[bit] heard: {text:?}");
        let _ = app.emit("transcript", text.clone());
        if text.is_empty() {
            println!("[bit] worker: empty transcript → neutral");
            set_bit_state(&app, "neutral");
            return;
        }
        match config::load_agent_config(&app) {
            None => {
                eprintln!("[bit] no Z.AI key set — open Settings to add it");
                set_bit_state(&app, "neutral");
            }
            Some(cfg) => {
                println!(
                    "[bit] worker: calling agent (provider={}, model={})…",
                    cfg.provider, cfg.model
                );
                match agent::ask(&app, &cfg, &text) {
                    Ok((yes, times)) => {
                        println!("[bit] → {} x{times}", if yes { "yes" } else { "no" });
                        emit_verdict(&app, yes, times);
                    }
                    Err(e) => {
                        eprintln!("[bit] agent error: {e}");
                        emit_verdict(&app, false, 1);
                    }
                }
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if shortcut != &talk_shortcut() {
                        return;
                    }
                    // Act on key-down only, time-debounced so one press = one
                    // toggle even if macOS repeats the event while held.
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let state = app.state::<AppState>();
                    let mut last = state.last_toggle.lock().unwrap();
                    if last.elapsed() < Duration::from_millis(350) {
                        return;
                    }
                    *last = Instant::now();
                    drop(last);
                    on_toggle(app);
                })
                .build(),
        )
        .setup(|app| {
            // Pure floating pet: no Dock icon, no app menu.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(ActivationPolicy::Accessory);

            // Speech-to-text state. The model dir is decided per-active-model
            // (see stt::model_dir); Stt just needs the app-data root. Migrate
            // any pre-0.2.0 model dir name first so existing users keep working.
            let app_data = app.path().app_data_dir()?;
            stt::migrate_legacy_dirs(&app_data);
            let stt = Arc::new(stt::Stt::new(app_data));
            let dragging = Arc::new(AtomicBool::new(false));
            // The MCP registry is registered TWICE: nested in AppState (for the
            // commands that reach via `app.state::<AppState>().mcp`) AND as a
            // top-level managed type (so `app.state::<Registry>()` works from
            // mcp.rs/agent.rs, which can't name AppState). Both point at the
            // SAME Arc-backed instance, so this is free and never diverges.
            let mcp = mcp::Registry::new();
            app.manage(mcp.clone());
            app.manage(AppState {
                recorder: audio::Recorder::new(),
                stt,
                last_toggle: Mutex::new(Instant::now()),
                dragging: dragging.clone(),
                mcp,
            });

            // Pre-warm enabled MCP servers in the background so the first voice
            // request doesn't stall on npx cold-start. Failures are non-fatal —
            // the server just contributes no tools until it connects.
            spawn_mcp_prewarm(app.handle().clone());

            // Defense-in-depth: verify the Registry is reachable via the
            // standalone `app.state::<Registry>()` path that mcp.rs/agent.rs
            // use. If a future refactor un-manges it, this panics loudly at
            // startup instead of silently breaking push-to-talk mid-request
            // (which is the exact bug this guards against).
            let _: tauri::State<'_, mcp::Registry> = app.state::<mcp::Registry>();
            eprintln!("[bit] setup ok: MCP registry registered");

            // Push-to-talk global shortcut.
            app.global_shortcut().register(talk_shortcut())?;

            // Tray icon is the only chrome: open settings / quit. Use a dedicated
            // monochrome template image (adaptive to the menu bar) built from the
            // Bit geometry; fall back to the app icon if it can't be decoded.
            let tray_icon =
                tauri::image::Image::from_bytes(include_bytes!("../icons/bit-tray.png"))
                    .map_err(|e| eprintln!("[bit] tray icon decode failed: {e}"))
                    .ok();
            let settings_i = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit Bit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&settings_i, &quit_i])?;

            let mut tray = TrayIconBuilder::with_id("bit-tray")
                .tooltip("Bit")
                .icon_as_template(true)
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "settings" => open_settings(app),
                    "quit" => app.exit(0),
                    _ => {}
                });
            // Prefer the template silhouette; otherwise the colored app icon.
            if let Some(icon) = tray_icon {
                tray = tray.icon(icon);
            } else if let Some(icon) = app.default_window_icon().cloned() {
                tray = tray.icon(icon).icon_as_template(false);
            }
            tray.build(app)?;

            if let Some(win) = app.get_webview_window("bit") {
                // Apply the saved on-screen size before the passthrough poller starts.
                apply_bit_size(app.handle(), config::load_settings(app.handle()).size);
                spawn_passthrough(app.handle().clone(), win, dragging.clone());
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // Drop back to accessory mode once settings is closed.
            #[cfg(target_os = "macos")]
            if window.label() == "config" {
                if let tauri::WindowEvent::Destroyed = event {
                    let _ = window
                        .app_handle()
                        .set_activation_policy(ActivationPolicy::Accessory);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            set_bit_size,
            get_workflows,
            save_workflow,
            delete_workflow,
            run_workflow,
            dnd_status,
            set_dnd,
            setup_dnd,
            start_drag,
            end_drag,
            get_mcp_servers,
            save_mcp_server,
            delete_mcp_server,
            test_mcp_server,
            add_http_server,
            get_mcp_tools,
            set_mcp_disabled_tools,
            get_stt_models,
            download_stt_model,
            set_stt_model,
            delete_stt_model,
            get_mcp_presets,
            autostart_state,
            set_autostart
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
