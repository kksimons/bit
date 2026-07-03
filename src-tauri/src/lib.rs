mod agent;
mod audio;
mod config;
pub mod mcp;
mod memory;
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

/// Base square window size for the Bit overlay, in logical pixels.
const BIT_BASE_SIZE: f64 = 260.0;

/// Push-to-talk: press once to start, again to stop (Ctrl+Opt+Space).
fn talk_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space)
}

struct AppState {
    recorder: audio::Recorder,
    stt: Arc<stt::Stt>,
    /// Debounces duplicate/auto-repeat shortcut events (macOS can double-fire).
    last_toggle: Mutex<Instant>,
    /// Set while the user is dragging the Bit (pauses the click-through poller).
    dragging: Arc<AtomicBool>,
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

/// Resize the overlay around the base size; the passthrough poller follows live.
fn apply_bit_size(app: &tauri::AppHandle, scale: f64) {
    let Some(win) = bit_window(app) else {
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

/// A background thread follows the cursor while dragging and tracks release
/// velocity, so letting go can fling the Bit with momentum.
#[tauri::command]
fn start_drag(app: tauri::AppHandle) {
    let state = app.state::<AppState>();
    if state.dragging.swap(true, Ordering::Relaxed) {
        return; // already dragging
    }
    let dragging = state.dragging.clone();
    let Some(win) = bit_window(&app) else {
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
            // EMA of cursor velocity (tick ≈ 8ms).
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

#[derive(serde::Serialize)]
struct McpServerView {
    name: String,
    transport: mcp::Transport,
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
                    registry.tool_count(&s.name)
                } else {
                    0
                },
                connected,
                error: registry.last_error(&s.name),
                name: s.name.clone(),
                transport: s.transport,
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
    if saved.enabled {
        spawn_mcp_prewarm(app.clone());
    }
    Ok(saved)
}

#[tauri::command]
fn delete_mcp_server(app: tauri::AppHandle, name: String) -> Result<(), String> {
    app.state::<AppState>().mcp.invalidate(&name);
    // Drop any stored OAuth token for an http server so it's fully gone.
    if let Some(path) = mcp::oauth::FileCredentialStore::path_for(&app, &name) {
        let _ = std::fs::remove_file(path);
    }
    mcp::delete(&app, &name)
}

/// Add a remote (HTTP) MCP server via OAuth: discover → register → browser
/// sign-in → loopback callback → persist token. The UI just supplies name + URL.
#[tauri::command]
fn add_http_server(app: tauri::AppHandle, name: String, url: String) -> Result<String, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("no config dir: {e}"))?;
    // run_oauth_flow normalizes the URL (e.g. /sse → /mcp) and returns the one
    // it used; save THAT so the token's audience matches connect.
    let resolved_url = mcp::http::run_oauth_flow(&dir, &name, &url)?;
    let server = mcp::McpServer {
        name,
        transport: mcp::Transport::Http,
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

#[tauri::command]
fn test_mcp_server(app: tauri::AppHandle, name: String) -> Result<usize, String> {
    let server = mcp::load_all(&app)
        .into_iter()
        .find(|s| s.name.eq_ignore_ascii_case(&name))
        .ok_or_else(|| format!("no MCP server named '{name}'"))?;
    if !server.enabled {
        return Err("server is disabled".into());
    }
    app.state::<AppState>().mcp.invalidate(&name);
    app.state::<AppState>()
        .mcp
        .ensure(&app, &server)
        .map(|t| t.len())
}

#[tauri::command]
fn get_mcp_tools(app: tauri::AppHandle, name: String) -> Result<Vec<mcp::ToolView>, String> {
    mcp::tools_view(&app, &name)
}

/// `disabled` is the FULL new denylist (server-side tool names). The UI sends
/// the complete set, keeping this idempotent. Cheap: rewrites `mcp.json` only.
#[tauri::command]
fn set_mcp_disabled_tools(
    app: tauri::AppHandle,
    name: String,
    disabled: Vec<String>,
) -> Result<(), String> {
    mcp::set_disabled_tools(&app, &name, disabled)
}

// ---- Transcription models ----

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

/// Download a model's files, emitting `stt-download-progress` per file.
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

/// Set the active transcription model; takes effect on the next recording.
#[tauri::command]
fn set_stt_model(app: tauri::AppHandle, model_id: String) -> Result<(), String> {
    if !stt::MODELS.iter().any(|m| m.id == model_id) {
        return Err(format!("unknown model: {model_id}"));
    }
    let mut s = config::load_settings(&app);
    s.stt_model = model_id;
    config::save_settings(&app, &s)
}

#[tauri::command]
fn delete_stt_model(app: tauri::AppHandle, model_id: String) -> Result<(), String> {
    let active = config::load_settings(&app).stt_model;
    if active == model_id {
        return Err("can't delete the active model — switch first".into());
    }
    let app_data = app.path().app_data_dir().map_err(|e| e.to_string())?;
    stt::delete_model(&app_data, &model_id)
}

#[derive(serde::Serialize)]
struct PresetFieldView {
    env_key: String,
    label: String,
    placeholder: String,
    secret: bool,
}

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

/// Background-warm every enabled MCP server (already-connected ones are no-ops).
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

/// Pre-load the STT model after launch so the first voice command doesn't pay
/// the load cost. Failures are non-fatal (retried on first use).
fn spawn_stt_prewarm(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let stt = app.state::<AppState>().stt.clone();
        let active_model = config::load_settings(&app).stt_model;
        if let Err(e) = stt.ensure_loaded(&active_model) {
            eprintln!("[bit] stt pre-warm deferred (will retry on first use): {e}");
        }
    });
}

// ---- Launch on startup ----
//
// Wrapped in our own commands so we can refuse in dev builds: the debug binary
// loads its UI from the Vite dev server, which isn't running after a reboot.

#[tauri::command]
fn autostart_state(app: tauri::AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Register/unregister Bit as a login item. Refuses in debug builds (see above).
#[tauri::command]
fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    // cfg!(debug_assertions) is true under `tauri dev`, false under `tauri build`.
    if cfg!(debug_assertions) {
        return Err(
            "Launch on startup is disabled in development builds — the dev binary \
             can't find its UI after a reboot. Build and install the app first: \
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

/// Max chars of tool output kept (in memory + shown to the model).
pub(crate) const MAX_OUTPUT: usize = 4000;

/// Truncate `s` to [`MAX_OUTPUT`] bytes on a UTF-8 char boundary, with ellipsis.
pub(crate) fn truncate_output(s: &mut String) {
    if s.len() <= MAX_OUTPUT {
        return;
    }
    let cut = s
        .char_indices()
        .take_while(|(i, _)| *i <= MAX_OUTPUT)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(MAX_OUTPUT);
    s.truncate(cut);
    s.push_str("\n…[truncated]");
}

/// The Bit overlay window, if it exists.
fn bit_window(app: &tauri::AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window("bit")
}

fn set_bit_state(app: &tauri::AppHandle, state: &str) {
    if let Some(win) = bit_window(app) {
        let _ = win.emit("bit-state", state);
    }
}

/// Emit the final yes/no verdict and how many times to say it (1..=3).
fn emit_verdict(app: &tauri::AppHandle, yes: bool, times: u8) {
    if let Some(win) = bit_window(app) {
        let _ = win.emit(
            "bit-verdict",
            serde_json::json!({ "kind": if yes { "yes" } else { "no" }, "times": times }),
        );
    }
}

/// Heartbeat while the agent works (one per LLM turn + tool call) so the Bit's
/// thinking animation can advance instead of freezing through the latency.
pub fn nudge_thinking(app: &tauri::AppHandle) {
    if let Some(win) = bit_window(app) {
        let _ = win.emit("bit-step", ());
    }
}

#[tauri::command]
fn open_memory_folder(app: tauri::AppHandle) -> Result<(), String> {
    let dir = memory::folder(&app)?;
    std::process::Command::new("open")
        .arg(&dir)
        .status()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Open (or focus) the settings window. Briefly switches to a regular activation
/// policy so the window can take focus, then drops back to accessory on close.
fn open_settings(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("config") {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }

    // Become a regular app so the window can take focus (in accessory mode it
    // would open behind and need a second click).
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

/// Keep the overlay click-through except over the Bit itself, so empty pixels
/// pass clicks through. Polls the global cursor vs. the window center because
/// the window receives no DOM mouse events while ignoring cursor events.
fn spawn_passthrough(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    dragging: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut last: Option<bool> = None;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(70));
            // While dragging/flinging, leave the window interactive.
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
            // Interactive within a centered disc covering the Bit.
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
        // Refuse visibly if the mic never came up.
        if !state.recorder.available() {
            let reason = state
                .recorder
                .last_error()
                .unwrap_or_else(|| "microphone unavailable".into());
            eprintln!("[bit] push-to-talk refused: {reason}");
            emit_verdict(app, false, 1);
            return;
        }
        // If no transcription model is downloaded yet, open Settings so the user
        // can pick one rather than silently starting a 700 MB download mid-recording.
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
/// ~1.1s lull (or a 30s cap) finishes the recording — one press is enough.
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
        let samples_16k = audio::resample_to_16k(&samples, rate);
        let active_model = config::load_settings(&app).stt_model;
        let text = match stt
            .ensure_loaded(&active_model)
            .and_then(|()| stt.transcribe(&samples_16k))
        {
            Ok(t) => t.trim().to_string(),
            Err(e) => {
                eprintln!("[bit] stt error: {e}");
                String::new()
            }
        };
        let _ = app.emit("transcript", text.clone());
        if text.is_empty() {
            set_bit_state(&app, "neutral");
            return;
        }
        match config::load_agent_config(&app) {
            None => {
                eprintln!("[bit] no API key set — open Settings to add it");
                set_bit_state(&app, "neutral");
            }
            Some(cfg) => {
                let ask_t0 = Instant::now();
                match agent::ask(&app, &cfg, &text) {
                    Ok((yes, times, n_tools)) => {
                        let ms = ask_t0.elapsed().as_millis();
                        memory::log_ask(
                            &app,
                            &text,
                            if yes { "yes" } else { "no" },
                            times,
                            n_tools,
                            ms,
                            None,
                        );
                        println!("[bit] → {} x{times}", if yes { "yes" } else { "no" });
                        emit_verdict(&app, yes, times);
                    }
                    Err(e) => {
                        let ms = ask_t0.elapsed().as_millis();
                        memory::log_ask(&app, &text, "no", 1, 0, ms, Some(&e));
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
                    // Act on key-down only, time-debounced against auto-repeat.
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

            let app_data = app.path().app_data_dir()?;
            let stt = Arc::new(stt::Stt::new(app_data));
            let dragging = Arc::new(AtomicBool::new(false));
            // The MCP registry is registered twice: nested in AppState (for the
            // commands) and as a top-level managed type (so mcp.rs/agent.rs can
            // reach it via app.state::<Registry>()). Both point at the same
            // Arc-backed instance.
            let mcp = mcp::Registry::new();
            app.manage(mcp.clone());
            app.manage(AppState {
                recorder: audio::Recorder::new(),
                stt,
                last_toggle: Mutex::new(Instant::now()),
                dragging: dragging.clone(),
                mcp,
            });

            spawn_mcp_prewarm(app.handle().clone());
            spawn_stt_prewarm(app.handle().clone());

            app.global_shortcut().register(talk_shortcut())?;

            // Tray icon is the only chrome. Prefer a monochrome template image
            // (adaptive to the menu bar); fall back to the colored app icon.
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
            if let Some(icon) = tray_icon {
                tray = tray.icon(icon);
            } else if let Some(icon) = app.default_window_icon().cloned() {
                tray = tray.icon(icon).icon_as_template(false);
            }
            tray.build(app)?;

            if let Some(win) = bit_window(app.handle()) {
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
            set_autostart,
            open_memory_folder
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
