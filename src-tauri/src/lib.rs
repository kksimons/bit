mod agent;
mod audio;
mod config;
mod stt;
mod tools;
mod workflows;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;

/// Toggle dictation: press once to start, again to stop (Ctrl+Opt+Space).
fn talk_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space)
}

struct AppState {
    recorder: audio::Recorder,
    stt: Arc<stt::Stt>,
    /// Whether the shortcut key is physically held — debounces auto-repeat so a
    /// held key toggles exactly once.
    key_held: AtomicBool,
}

#[derive(serde::Serialize)]
struct SettingsView {
    base_url: String,
    model: String,
    has_key: bool,
}

#[tauri::command]
fn get_settings(app: tauri::AppHandle) -> SettingsView {
    let s = config::load_settings(&app);
    SettingsView {
        base_url: s.base_url,
        model: s.model,
        has_key: config::get_key().is_some(),
    }
}

#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    base_url: String,
    model: String,
    api_key: Option<String>,
) -> Result<(), String> {
    config::save_settings(&app, &config::Settings { base_url, model })?;
    if let Some(k) = api_key {
        if !k.is_empty() {
            config::set_key(&k)?;
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

/// Tell the Bit overlay to change form/state.
fn set_bit_state(app: &tauri::AppHandle, state: &str) {
    if let Some(win) = app.get_webview_window("bit") {
        let _ = win.emit("bit-state", state);
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

    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(ActivationPolicy::Regular);

    let _ = WebviewWindowBuilder::new(app, "config", WebviewUrl::App("config.html".into()))
        .title("Bit Settings")
        .inner_size(440.0, 540.0)
        .resizable(true)
        .build();
}

/// Background poll: keep the overlay click-through everywhere except over the
/// Bit itself, so empty pixels pass clicks to whatever is behind it. We poll the
/// global cursor against the window's center so it works even while the window is
/// ignoring cursor events (and thus receiving no DOM mouse events).
fn spawn_passthrough(app: tauri::AppHandle, window: tauri::WebviewWindow) {
    std::thread::spawn(move || {
        let mut last: Option<bool> = None;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(70));
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

/// Toggle dictation on a fresh key press: start recording if idle, otherwise
/// stop and run transcription → agent → yes/no.
fn on_toggle(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    if !state.recorder.is_recording() {
        state.recorder.start();
        set_bit_state(app, "listening");
    } else {
        let (samples, rate) = state.recorder.stop();
        set_bit_state(app, "thinking");
        let stt = state.stt.clone();
        let app = app.clone();
        std::thread::spawn(move || {
                let samples_16k = audio::resample_to_16k(&samples, rate);
                let text = match stt
                    .ensure_loaded()
                    .and_then(|_| stt.transcribe(&samples_16k))
                {
                    Ok(t) => t.trim().to_string(),
                    Err(e) => {
                        eprintln!("[bit] stt error: {e}");
                        String::new()
                    }
                };
                println!("[bit] heard: {text:?}");
                let _ = app.emit("transcript", text.clone());
                if text.is_empty() {
                    set_bit_state(&app, "neutral");
                    return;
                }
                match config::load_agent_config(&app) {
                    None => {
                        eprintln!("[bit] no Z.AI key set — open Settings to add it");
                        set_bit_state(&app, "neutral");
                    }
                    Some(cfg) => match agent::ask(&app, &cfg, &text) {
                        Ok(true) => {
                            println!("[bit] → yes");
                            set_bit_state(&app, "yes");
                        }
                        Ok(false) => {
                            println!("[bit] → no");
                            set_bit_state(&app, "no");
                        }
                        Err(e) => {
                            eprintln!("[bit] agent error: {e}");
                            set_bit_state(&app, "no");
                        }
                    },
                }
            });
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if shortcut != &talk_shortcut() {
                        return;
                    }
                    let state = app.state::<AppState>();
                    match event.state() {
                        // Fresh press toggles; ignore auto-repeat while held.
                        ShortcutState::Pressed => {
                            if !state.key_held.swap(true, Ordering::Relaxed) {
                                on_toggle(app);
                            }
                        }
                        ShortcutState::Released => {
                            state.key_held.store(false, Ordering::Relaxed);
                        }
                    }
                })
                .build(),
        )
        .setup(|app| {
            // Pure floating pet: no Dock icon, no app menu.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(ActivationPolicy::Accessory);

            // Speech-to-text state (model downloads/loads lazily on first use).
            let app_data = app.path().app_data_dir()?;
            let stt = Arc::new(stt::Stt::new(stt::model_dir(&app_data)));
            app.manage(AppState {
                recorder: audio::Recorder::new(),
                stt,
                key_held: AtomicBool::new(false),
            });

            // Push-to-talk global shortcut.
            app.global_shortcut().register(talk_shortcut())?;

            // Tray icon is the only chrome: open settings / quit.
            let settings_i =
                MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit Bit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&settings_i, &quit_i])?;

            TrayIconBuilder::with_id("bit-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Bit")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "settings" => open_settings(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            if let Some(win) = app.get_webview_window("bit") {
                spawn_passthrough(app.handle().clone(), win);
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
            get_workflows,
            save_workflow,
            delete_workflow,
            run_workflow,
            dnd_status,
            set_dnd,
            setup_dnd
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
