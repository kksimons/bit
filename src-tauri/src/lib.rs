mod agent;
mod audio;
mod config;
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
    /// Last time the talk shortcut toggled — time-debounces duplicate/auto-repeat
    /// shortcut events (macOS can double-fire), so one press = one toggle.
    last_toggle: Mutex<Instant>,
    /// Set while the user is dragging the Bit (pauses the click-through poller).
    dragging: Arc<AtomicBool>,
}

#[derive(serde::Serialize)]
struct SettingsView {
    provider: String,
    base_url: String,
    model: String,
    has_key: bool,
    developer_mode: bool,
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
    }
}

#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    provider: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    developer_mode: bool,
) -> Result<(), String> {
    config::save_settings(
        &app,
        &config::Settings {
            provider,
            base_url,
            model,
            developer_mode,
        },
    )?;
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
                Ok((yes, times)) => {
                    println!("[bit] → {} x{times}", if yes { "yes" } else { "no" });
                    emit_verdict(&app, yes, times);
                }
                Err(e) => {
                    eprintln!("[bit] agent error: {e}");
                    emit_verdict(&app, false, 1);
                }
            },
        }
    });
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

            // Speech-to-text state (model downloads/loads lazily on first use).
            let app_data = app.path().app_data_dir()?;
            let stt = Arc::new(stt::Stt::new(stt::model_dir(&app_data)));
            let dragging = Arc::new(AtomicBool::new(false));
            app.manage(AppState {
                recorder: audio::Recorder::new(),
                stt,
                last_toggle: Mutex::new(Instant::now()),
                dragging: dragging.clone(),
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
            get_workflows,
            save_workflow,
            delete_workflow,
            run_workflow,
            dnd_status,
            set_dnd,
            setup_dnd,
            start_drag,
            end_drag
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
