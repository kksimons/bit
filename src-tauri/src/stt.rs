use std::path::{Path, PathBuf};
use std::sync::Mutex;

use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;
use transcribe_rs::whisper_cpp::{WhisperEngine, WhisperInferenceParams};

// ============================ model registry ============================
//
// Bit supports two local transcription models, picked by the user in Settings:
//   - Parakeet V2: English-only, ~700 MB, best English accuracy (the default).
//   - Whisper Base: 99 languages, ~141 MB, multilingual.
// Both ship via `transcribe-rs`. Each `ModelSpec` knows its id, display info,
// download source, and the on-disk files that mean "fully downloaded". The
// loaded model is held as a `LoadedModel` enum so `Stt` can swap backends.

/// Catalog entry for one transcription model.
pub struct ModelSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub languages: &'static str,
    /// Approximate total download size in MB (shown in the picker UI).
    pub size_mb: u64,
    /// (url, relative_filename) pairs. Downloaded into the model's dir.
    pub files: &'static [(&'static str, &'static str)],
}

const PARAKEET_FILES: &[(&str, &str)] = &[
    (
        "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v2-onnx/resolve/main/encoder-model.int8.onnx",
        "encoder-model.int8.onnx",
    ),
    (
        "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v2-onnx/resolve/main/decoder_joint-model.int8.onnx",
        "decoder_joint-model.int8.onnx",
    ),
    (
        "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v2-onnx/resolve/main/nemo128.onnx",
        "nemo128.onnx",
    ),
    (
        "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v2-onnx/resolve/main/vocab.txt",
        "vocab.txt",
    ),
];

const WHISPER_FILES: &[(&str, &str)] = &[(
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
    "ggml-base.bin",
)];

/// The full menu. Order = display order; the first entry is the default for new
/// users (matched by `DEFAULT_MODEL_ID`).
pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "parakeet-v2",
        name: "Parakeet V2",
        description: "Fast and accurate. English only.",
        languages: "English",
        size_mb: 700,
        files: PARAKEET_FILES,
    },
    ModelSpec {
        id: "whisper-base",
        name: "Whisper Base",
        description: "Multilingual. Great for non-English speakers.",
        languages: "99 languages",
        size_mb: 141,
        files: WHISPER_FILES,
    },
];

/// The active model for users who haven't picked one yet. Existing users with
/// the Parakeet files already on disk are migrated to this on first run.
pub const DEFAULT_MODEL_ID: &str = "parakeet-v2";

/// Look up a spec by id. Falls back to the default if unknown (defensive — a
/// stale `active_model` setting naming a removed model shouldn't break STT).
pub fn spec(id: &str) -> &'static ModelSpec {
    MODELS
        .iter()
        .find(|m| m.id == id)
        .unwrap_or_else(|| MODELS.iter().find(|m| m.id == DEFAULT_MODEL_ID).unwrap())
}

/// Where a model's files live on disk: `<app_data>/models/<id>`.
pub fn model_dir(app_data: &Path, model_id: &str) -> PathBuf {
    app_data.join("models").join(model_id)
}

/// One-time migration: pre-0.2.0 Bit stored Parakeet under the full HuggingFace
/// repo name (`parakeet-tdt-0.6b-v2`) rather than the model id (`parakeet-v2`).
/// Rename it into place so existing users keep working instead of being told
/// to redownload. Idempotent — no-op if already migrated or absent. Best-effort;
/// a failure just means the user re-downloads via the picker.
pub fn migrate_legacy_dirs(app_data: &Path) {
    let old = app_data.join("models").join("parakeet-tdt-0.6b-v2");
    let new = model_dir(app_data, "parakeet-v2");
    if old.is_dir() && !new.is_dir() {
        if let Err(e) = std::fs::rename(&old, &new) {
            eprintln!("[bit] couldn't migrate parakeet model dir: {e}");
        } else {
            println!("[bit] migrated parakeet model dir → parakeet-v2/");
        }
    }
}

/// Are all of this model's files present?
pub fn model_ready(app_data: &Path, model_id: &str) -> bool {
    let spec = spec(model_id);
    let dir = model_dir(app_data, model_id);
    spec.files.iter().all(|(_, name)| dir.join(name).exists())
}

/// Download any missing files for a model. `on_progress` is called with
/// (file_index, total_files, filename) as each completes — used to drive the
/// download progress in the picker UI.
pub fn download_model<F: FnMut(usize, usize, &str)>(
    app_data: &Path,
    model_id: &str,
    mut on_progress: F,
) -> Result<(), String> {
    let spec = spec(model_id);
    let dir = model_dir(app_data, model_id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let total = spec.files.len();
    for (i, (url, name)) in spec.files.iter().enumerate() {
        let dest = dir.join(name);
        if dest.exists() {
            on_progress(i + 1, total, name);
            continue;
        }
        println!("[bit] downloading {name} …");
        let resp = ureq::get(url)
            .call()
            .map_err(|e| format!("download {name}: {e}"))?;
        let tmp = dir.join(format!("{name}.part"));
        let mut out = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        std::io::copy(&mut resp.into_reader(), &mut out).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        println!("[bit] downloaded {name}");
        on_progress(i + 1, total, name);
    }
    Ok(())
}

/// Delete a model's files from disk (used when the user removes a downloaded
/// model they no longer want). Best-effort.
pub fn delete_model(app_data: &Path, model_id: &str) -> Result<(), String> {
    let dir = model_dir(app_data, model_id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ============================ the loaded model ============================

/// A model loaded into memory, regardless of backend. Each backend's
/// `transcribe_with` is called the same way; we just route by variant.
enum LoadedModel {
    Parakeet(ParakeetModel),
    Whisper(WhisperEngine),
}

impl LoadedModel {
    fn transcribe(&mut self, samples_16k: &[f32]) -> Result<String, String> {
        match self {
            LoadedModel::Parakeet(m) => m
                .transcribe_with(samples_16k, &ParakeetParams::default())
                .map(|r| r.text)
                .map_err(|e| e.to_string()),
            LoadedModel::Whisper(m) => {
                let params = WhisperInferenceParams {
                    // Auto-detect language (Whisper's strength); suppress the
                    // chatty C++ realtime/progress logs that polluted earlier runs.
                    language: Some("auto".into()),
                    print_progress: false,
                    print_realtime: false,
                    print_special: false,
                    print_timestamps: false,
                    ..Default::default()
                };
                m.transcribe_with(samples_16k, &params)
                    .map(|r| r.text)
                    .map_err(|e| e.to_string())
            }
        }
    }
}

/// Holds the lazily-loaded active model. Loading is heavy and inference is
/// serialized behind a mutex. The active model id is read from settings at
/// load time, so a user switching models in Settings takes effect on the next
/// `ensure_loaded` (which reloads if the id changed).
pub struct Stt {
    model: Mutex<Option<LoadedModel>>,
    /// The model id currently loaded (None = not loaded yet). Compared against
    /// the configured active model to decide whether to reload on `ensure_loaded`.
    loaded_id: Mutex<Option<String>>,
    app_data: PathBuf,
}

impl Stt {
    pub fn new(app_data: PathBuf) -> Self {
        Stt {
            model: Mutex::new(None),
            loaded_id: Mutex::new(None),
            app_data,
        }
    }

    /// Download (if needed) and load the configured active model. Heavy — call
    /// off the UI thread. Reloads if the active model changed since last load.
    /// `active_model_id` comes from settings (the caller reads it).
    pub fn ensure_loaded(&self, active_model_id: &str) -> Result<(), String> {
        // Already loaded and still the active one, with a live model? Nothing to do.
        if let Some(id) = self.loaded_id.lock().unwrap().as_ref() {
            if id == active_model_id && self.model.lock().unwrap().is_some() {
                return Ok(());
            }
        }

        if !model_ready(&self.app_data, active_model_id) {
            download_model(&self.app_data, active_model_id, |_, _, _| {})?;
        }
        let dir = model_dir(&self.app_data, active_model_id);
        println!("[bit] loading model {active_model_id} …");
        let loaded = load_model(active_model_id, &dir)?;
        *self.model.lock().unwrap() = Some(loaded);
        *self.loaded_id.lock().unwrap() = Some(active_model_id.to_string());
        println!("[bit] model loaded");
        Ok(())
    }

    /// Transcribe 16 kHz mono f32 samples. `ensure_loaded` must have succeeded.
    pub fn transcribe(&self, samples_16k: &[f32]) -> Result<String, String> {
        let mut guard = self.model.lock().unwrap();
        let model = guard.as_mut().ok_or("model not loaded")?;
        model.transcribe(samples_16k)
    }
}

/// Instantiate a model backend from its on-disk files.
fn load_model(model_id: &str, dir: &Path) -> Result<LoadedModel, String> {
    match model_id {
        "whisper-base" => {
            let path = dir.join("ggml-base.bin");
            WhisperEngine::load(&path)
                .map(LoadedModel::Whisper)
                .map_err(|e| e.to_string())
        }
        // Default / "parakeet-v2" / any unknown id → Parakeet (matches `spec`).
        _ => ParakeetModel::load(dir, &Quantization::Int8)
            .map(LoadedModel::Parakeet)
            .map_err(|e| e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_have_unique_ids() {
        let ids: Vec<_> = MODELS.iter().map(|m| m.id).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len(), "duplicate model ids");
    }

    #[test]
    fn default_model_exists_in_registry() {
        assert!(MODELS.iter().any(|m| m.id == DEFAULT_MODEL_ID));
    }

    #[test]
    fn spec_falls_back_to_default_for_unknown_id() {
        // A stale setting naming a removed model shouldn't panic — it falls
        // back to the default, which is always present.
        let s = spec("nonexistent-future-model");
        assert_eq!(s.id, DEFAULT_MODEL_ID);
    }

    #[test]
    fn parakeet_and_whisper_have_disjoint_files() {
        // Sanity: the two models must not share on-disk filenames (they live in
        // separate dirs anyway, but this guards against copy-paste mistakes).
        let p_names: Vec<&str> = PARAKEET_FILES.iter().map(|(_, n)| *n).collect();
        let w_names: Vec<&str> = WHISPER_FILES.iter().map(|(_, n)| *n).collect();
        for name in &p_names {
            assert!(!w_names.contains(name), "{name} shared between models");
        }
    }
}
