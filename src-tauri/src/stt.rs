use std::path::{Path, PathBuf};
use std::sync::Mutex;

use transcribe_rs::onnx::parakeet::{ParakeetModel, ParakeetParams};
use transcribe_rs::onnx::Quantization;

const HF_BASE: &str =
    "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v2-onnx/resolve/main";
const FILES: [&str; 4] = [
    "encoder-model.int8.onnx",
    "decoder_joint-model.int8.onnx",
    "nemo128.onnx",
    "vocab.txt",
];

pub fn model_dir(app_data: &Path) -> PathBuf {
    app_data.join("models").join("parakeet-tdt-0.6b-v2")
}

fn model_ready(dir: &Path) -> bool {
    FILES.iter().all(|f| dir.join(f).exists())
}

/// Download any missing model files (~700 MB on first run) into `dir`.
fn download_model(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for f in FILES {
        let dest = dir.join(f);
        if dest.exists() {
            continue;
        }
        let url = format!("{HF_BASE}/{f}");
        println!("[bit] downloading {f} …");
        let resp = ureq::get(&url)
            .call()
            .map_err(|e| format!("download {f}: {e}"))?;
        let tmp = dir.join(format!("{f}.part"));
        let mut out = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
        std::io::copy(&mut resp.into_reader(), &mut out).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        println!("[bit] downloaded {f}");
    }
    Ok(())
}

/// Holds the lazily-loaded Parakeet model. The model is heavy to load and
/// inference is serialized behind a mutex.
pub struct Stt {
    model: Mutex<Option<ParakeetModel>>,
    dir: PathBuf,
}

impl Stt {
    pub fn new(dir: PathBuf) -> Self {
        Stt {
            model: Mutex::new(None),
            dir,
        }
    }

    /// Download (if needed) and load the model. Heavy — call off the UI thread.
    pub fn ensure_loaded(&self) -> Result<(), String> {
        if !model_ready(&self.dir) {
            download_model(&self.dir)?;
        }
        let mut guard = self.model.lock().unwrap();
        if guard.is_none() {
            println!("[bit] loading Parakeet model …");
            let model = ParakeetModel::load(&self.dir, &Quantization::Int8)
                .map_err(|e| e.to_string())?;
            *guard = Some(model);
            println!("[bit] model loaded");
        }
        Ok(())
    }

    /// Transcribe 16 kHz mono f32 samples. `ensure_loaded` must have succeeded.
    pub fn transcribe(&self, samples_16k: &[f32]) -> Result<String, String> {
        let mut guard = self.model.lock().unwrap();
        let model = guard.as_mut().ok_or("model not loaded")?;
        let result = model
            .transcribe_with(samples_16k, &ParakeetParams::default())
            .map_err(|e| e.to_string())?;
        Ok(result.text)
    }
}
