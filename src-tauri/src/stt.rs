use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;
use transcribe_rs::whisper_cpp::{WhisperEngine, WhisperInferenceParams};

// ============================ model registry ============================
//
// Two local transcription models, picked by the user in Settings:
//   - Parakeet V2: English-only, ~700 MB, best English accuracy (the default).
//   - Whisper Base: 99 languages, ~141 MB, multilingual.
// Each `ModelSpec` knows its id, display info, download source, and the on-disk
// files that mean "fully downloaded". The loaded model is held as a
// `LoadedModel` enum so `Stt` can swap backends.

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

/// The full menu. Order = display order; the first entry is the default.
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

pub const DEFAULT_MODEL_ID: &str = "parakeet-v2";

/// Look up a spec by id. Falls back to the default if unknown so a stale
/// `active_model` setting can't break STT.
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

/// Are all of this model's files present?
pub fn model_ready(app_data: &Path, model_id: &str) -> bool {
    let spec = spec(model_id);
    let dir = model_dir(app_data, model_id);
    spec.files.iter().all(|(_, name)| dir.join(name).exists())
}

/// Download any missing files for a model. `on_progress` is called with
/// (file_index, total_files, filename) as each completes.
///
/// Uses reqwest (shared with the MCP transport) via `block_on`, streaming each
/// file to disk chunk-by-chunk so a 700 MB model never sits whole in memory.
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
        download_file(url, name, &dir)?;
        on_progress(i + 1, total, name);
    }
    Ok(())
}

/// Fetch one model file and write it atomically: stream to `<name>.part`, then
/// rename on success so a partial download never looks complete.
fn download_file(url: &str, name: &str, dir: &Path) -> Result<(), String> {
    let url = url.to_string();
    let tmp = dir.join(format!("{name}.part"));
    let dest = dir.join(name);
    tauri::async_runtime::handle().block_on(async move {
        use futures_util::StreamExt;
        let resp = http_client()
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("download {name}: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("download {name}: HTTP {status}"));
        }
        let mut out = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| format!("download {name}: {e}"))?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("download {name}: {e}"))?;
            tokio::io::AsyncWriteExt::write_all(&mut out, &chunk)
                .await
                .map_err(|e| format!("download {name}: {e}"))?;
        }
        tokio::io::AsyncWriteExt::flush(&mut out)
            .await
            .map_err(|e| format!("download {name}: {e}"))?;
        drop(out);
        tokio::fs::rename(&tmp, &dest)
            .await
            .map_err(|e| format!("download {name}: {e}"))
    })
}

/// Shared reqwest client for model downloads (connection pooling).
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// Delete a model's files from disk. Best-effort.
pub fn delete_model(app_data: &Path, model_id: &str) -> Result<(), String> {
    let dir = model_dir(app_data, model_id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ============================ the loaded model ============================

/// A model loaded into memory, regardless of backend.
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

/// Lazily-loaded active model. Inference is serialized behind a mutex. The
/// active model id is read from settings at load time; switching models in
/// Settings takes effect on the next `ensure_loaded`.
pub struct Stt {
    model: Mutex<Option<LoadedModel>>,
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

    /// Download (if needed) and load the active model. Reloads if the id changed.
    pub fn ensure_loaded(&self, active_model_id: &str) -> Result<(), String> {
        if let Some(id) = self.loaded_id.lock().unwrap().as_ref() {
            if id == active_model_id && self.model.lock().unwrap().is_some() {
                return Ok(());
            }
        }

        if !model_ready(&self.app_data, active_model_id) {
            download_model(&self.app_data, active_model_id, |_, _, _| {})?;
        }
        let dir = model_dir(&self.app_data, active_model_id);
        let loaded = load_model(active_model_id, &dir)?;
        *self.model.lock().unwrap() = Some(loaded);
        *self.loaded_id.lock().unwrap() = Some(active_model_id.to_string());
        Ok(())
    }

    /// Transcribe 16 kHz mono f32 samples. `ensure_loaded` must have succeeded.
    pub fn transcribe(&self, samples_16k: &[f32]) -> Result<String, String> {
        let mut guard = self.model.lock().unwrap();
        let model = guard.as_mut().ok_or("model not loaded")?;
        model.transcribe(samples_16k)
    }
}

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
        let s = spec("nonexistent-future-model");
        assert_eq!(s.id, DEFAULT_MODEL_ID);
    }

    #[test]
    fn parakeet_and_whisper_have_disjoint_files() {
        let p_names: Vec<&str> = PARAKEET_FILES.iter().map(|(_, n)| *n).collect();
        let w_names: Vec<&str> = WHISPER_FILES.iter().map(|(_, n)| *n).collect();
        for name in &p_names {
            assert!(!w_names.contains(name), "{name} shared between models");
        }
    }
}
