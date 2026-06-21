//! Smoke test: download Whisper base, load it via transcribe-rs, transcribe a
//! real speech clip. Proves the multilingual model path works end-to-end before
//! we build the model-picker UI around it.
//!
//! ```sh
//! cargo run --example whisper_smoke -- /path/to/a/speech.wav
//! ```
//!
//! Downloads `ggml-base.bin` (~141 MB) into the app's model dir on first run.

use std::path::PathBuf;

use transcribe_rs::whisper_cpp::{WhisperEngine, WhisperInferenceParams};

const WHISPER_BASE_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin";

fn app_config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let dir = PathBuf::from(home)
        .join("Library/Application Support")
        .join("ca.kylesimons.bit");
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn download_whisper_base(dir: &std::path::Path) -> Result<PathBuf, String> {
    let path = dir.join("models").join("whisper-base.bin");
    if path.exists() {
        println!("whisper-base.bin already present");
        return Ok(path);
    }
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    println!("downloading ggml-base.bin (~141 MB)…");
    let resp = ureq::get(WHISPER_BASE_URL)
        .call()
        .map_err(|e| format!("download: {e}"))?;
    let tmp = dir.join("models").join(".whisper-base.bin.part");
    let mut out = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    std::io::copy(&mut resp.into_reader(), &mut out).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    println!("downloaded.");
    Ok(path)
}

/// Decode a wav file to mono 16 kHz f32 (Whisper's expected input), reusing
/// Bit's hound-free approach: parse the canonical 16-bit PCM wav header by hand.
/// Bit's Handy recordings are 16-bit PCM; if not, we fall back to a hint.
fn read_wav_mono_f32(path: &std::path::Path) -> Result<(Vec<f32>, u32), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!(
            "not a canonical RIFF/WAVE file: {} (need 16-bit PCM wav)",
            path.display()
        ));
    }
    // Walk chunks to find fmt + data.
    let mut idx = 12usize;
    let mut sample_rate = 0u32;
    let mut channels = 1u16;
    let mut bits = 16u16;
    let mut audio: &[u8] = &[];
    while idx + 8 <= bytes.len() {
        let id = &bytes[idx..idx + 4];
        let size = u32::from_le_bytes([
            bytes[idx + 4],
            bytes[idx + 5],
            bytes[idx + 6],
            bytes[idx + 7],
        ]) as usize;
        let body_start = idx + 8;
        let body = &bytes[body_start..(body_start + size).min(bytes.len())];
        match id {
            b"fmt " => {
                // PCM fmt body: audio_format(2), channels(2), sample_rate(4), ...
                channels = u16::from_le_bytes([body[2], body[3]]);
                sample_rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                bits = u16::from_le_bytes([body[14], body[15]]);
            }
            b"data" => audio = body,
            _ => {}
        }
        idx = body_start + size + (size & 1); // pad to even
    }
    if bits != 16 {
        return Err(format!("expected 16-bit PCM, got {bits}-bit"));
    }
    let samples: Vec<f32> = audio
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect();
    // downmix to mono
    let mono = if channels > 1 {
        samples
            .chunks_exact(channels as usize)
            .map(|frame| frame.iter().copied().sum::<f32>() / channels as f32)
            .collect()
    } else {
        samples
    };
    // crude linear resample to 16k if needed
    let mono = if sample_rate != 16_000 {
        resample_linear(&mono, sample_rate, 16_000)
    } else {
        mono
    };
    Ok((mono, 16_000))
}

fn resample_linear(input: &[f32], src: u32, dst: u32) -> Vec<f32> {
    if src == dst || input.is_empty() {
        return input.to_vec();
    }
    let ratio = dst as f64 / src as f64;
    let out_len = (input.len() as f64 * ratio).round() as usize;
    let last = input.len() - 1;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 / ratio;
            let idx = pos.floor() as usize;
            let frac = pos - idx as f64;
            let a = input[idx.min(last)] as f64;
            let b = input[(idx + 1).min(last)] as f64;
            (a + (b - a) * frac) as f32
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let wav = args.first().cloned().unwrap_or_else(|| {
        // Default to the newest Handy recording if present.
        let dir = format!(
            "{}/Library/Application Support/com.pais.handy/recordings",
            std::env::var("HOME").unwrap_or_default()
        );
        newest_wav(&dir).unwrap_or_else(|| "speech.wav".into())
    });
    let dir = app_config_dir();

    println!("\n=== Whisper smoke test ===");
    println!("clip: {wav}");

    let model = match download_whisper_base(&dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("✗ model download failed: {e}");
            std::process::exit(1);
        }
    };

    println!("loading Whisper…");
    let mut engine = match WhisperEngine::load(&model) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("✗ load failed: {e}");
            std::process::exit(1);
        }
    };
    println!("loaded (multilingual={}).", engine_is_multilingual(&engine));

    let (samples, _rate) = match read_wav_mono_f32(std::path::Path::new(&wav)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("✗ couldn't read clip: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "clip: {} samples (~{:.1}s)",
        samples.len(),
        samples.len() as f32 / 16_000.0
    );

    let params = WhisperInferenceParams {
        language: Some("auto".into()),
        ..Default::default()
    };
    match engine.transcribe_with(&samples, &params) {
        Ok(result) => {
            println!("\n✓ transcription: {:?}", result.text);
            println!("\n✓ SUCCESS — Whisper loads + transcribes via transcribe-rs.");
        }
        Err(e) => {
            eprintln!("✗ transcribe failed: {e}");
            std::process::exit(1);
        }
    }
}

fn engine_is_multilingual(_e: &WhisperEngine) -> bool {
    // is_multilingual is a private field; tiny/base/large are multilingual,
    // tiny.en/base.en are not. We downloaded the multilingual base, so true.
    true
}

fn newest_wav(dir: &str) -> Option<String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    entries.sort_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()));
    entries
        .into_iter()
        .filter_map(|e| {
            let p = e.path();
            (p.extension()? == "wav").then(|| p.to_string_lossy().into_owned())
        })
        .last()
}
