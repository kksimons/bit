use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WebviewUrl, WebviewWindowBuilder,
};

#[cfg(target_os = "macos")]
use tauri::ActivationPolicy;

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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // Pure floating pet: no Dock icon, no app menu.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(ActivationPolicy::Accessory);

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
