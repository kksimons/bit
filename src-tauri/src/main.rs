// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // The bundled macOS app has no terminal, so println!/eprintln! go nowhere —
    // which made the push-to-talk hang impossible to diagnose. Redirect stdout
    // + stderr to a log file so every [bit] line (and any panic, and any
    // third-party lib output) is captured. Appends, so a session's worth of
    // runs stays readable. Best-effort: if it fails, we still run normally.
    redirect_stdio_to_log();
    eprintln!("\n=== bit started ===");
    bit_lib::run()
}

/// Append stdout+stderr to ~/Library/Logs/ca.kylesimons.bit/bit.log by dup2'ing
/// the log file's fd onto fd 1 and 2. This catches everything that writes to the
/// process's standard streams, including panics (which print to stderr) and
/// libraries that log via eprintln/println (cpal, onnxruntime, etc.).
#[cfg(unix)]
fn redirect_stdio_to_log() {
    use std::os::unix::io::AsRawFd;
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = std::path::Path::new(&home)
        .join("Library/Logs")
        .join("ca.kylesimons.bit");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("bit.log");
    let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(false)
        .open(&path)
    else {
        return;
    };
    let fd = f.as_raw_fd();
    // dup2 onto stdout (1) and stderr (2). `f` stays open for the process
    // lifetime (leaked intentionally — it holds the fd the dups point at).
    unsafe {
        libc::dup2(fd, 1);
        libc::dup2(fd, 2);
    }
    std::mem::forget(f);
}

#[cfg(not(unix))]
fn redirect_stdio_to_log() {}
