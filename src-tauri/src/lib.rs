mod audio;
mod stt;

use std::sync::Arc;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;

/// Push-to-talk: hold to record, release to transcribe (Cmd+Shift+Space).
fn talk_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Space)
}

struct AppState {
    recorder: audio::Recorder,
    stt: Arc<stt::Stt>,
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

/// Fired on hold (start recording) and release (stop + transcribe) of the
/// push-to-talk shortcut.
fn on_talk(app: &tauri::AppHandle, event_state: ShortcutState) {
    let state = app.state::<AppState>();
    match event_state {
        ShortcutState::Pressed => {
            state.recorder.start();
            set_bit_state(app, "listening");
        }
        ShortcutState::Released => {
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
                let _ = app.emit("transcript", text);
                set_bit_state(&app, "neutral");
            });
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if shortcut == &talk_shortcut() {
                        on_talk(app, event.state());
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
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
