use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// Captures microphone audio into a mono f32 buffer while `recording` is set.
///
/// A dedicated thread owns the (non-Send) cpal stream for the app's lifetime and
/// only appends samples while recording, so start/stop is just a flag flip.
#[derive(Clone)]
pub struct Recorder {
    recording: Arc<AtomicBool>,
    buffer: Arc<Mutex<Vec<f32>>>,
    sample_rate: Arc<AtomicU32>,
}

impl Recorder {
    pub fn new() -> Self {
        let recording = Arc::new(AtomicBool::new(false));
        let buffer = Arc::new(Mutex::new(Vec::<f32>::new()));
        let sample_rate = Arc::new(AtomicU32::new(16_000));

        let rec = recording.clone();
        let buf = buffer.clone();
        let sr = sample_rate.clone();
        std::thread::spawn(move || {
            let host = cpal::default_host();
            let Some(device) = host.default_input_device() else {
                eprintln!("[bit] no input device");
                return;
            };
            let config = match device.default_input_config() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[bit] input config error: {e}");
                    return;
                }
            };
            sr.store(config.sample_rate().0, Ordering::Relaxed);
            let channels = config.channels() as usize;
            let fmt = config.sample_format();
            let stream_config: cpal::StreamConfig = config.into();
            let err_fn = |e| eprintln!("[bit] audio stream error: {e}");

            let stream = match fmt {
                cpal::SampleFormat::F32 => device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        if rec.load(Ordering::Relaxed) {
                            let mut b = buf.lock().unwrap();
                            for frame in data.chunks(channels) {
                                b.push(frame.iter().copied().sum::<f32>() / channels as f32);
                            }
                        }
                    },
                    err_fn,
                    None,
                ),
                cpal::SampleFormat::I16 => device.build_input_stream(
                    &stream_config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        if rec.load(Ordering::Relaxed) {
                            let mut b = buf.lock().unwrap();
                            for frame in data.chunks(channels) {
                                let m = frame.iter().map(|&s| s as f32 / 32768.0).sum::<f32>()
                                    / channels as f32;
                                b.push(m);
                            }
                        }
                    },
                    err_fn,
                    None,
                ),
                other => {
                    eprintln!("[bit] unsupported sample format: {other:?}");
                    return;
                }
            };
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[bit] build input stream error: {e}");
                    return;
                }
            };
            if let Err(e) = stream.play() {
                eprintln!("[bit] stream play error: {e}");
                return;
            }
            // Keep the stream alive for the lifetime of the app.
            loop {
                std::thread::park();
            }
        });

        Recorder {
            recording,
            buffer,
            sample_rate,
        }
    }

    pub fn start(&self) {
        // Idempotent: key auto-repeat fires Pressed repeatedly while held, but we
        // only clear the buffer on the transition into recording.
        if !self.recording.swap(true, Ordering::Relaxed) {
            self.buffer.lock().unwrap().clear();
        }
    }

    /// Stop recording and return the captured mono samples + their sample rate.
    pub fn stop(&self) -> (Vec<f32>, u32) {
        self.recording.store(false, Ordering::Relaxed);
        let samples = self.buffer.lock().unwrap().clone();
        (samples, self.sample_rate.load(Ordering::Relaxed))
    }
}

/// Linear resample mono f32 to 16 kHz (what the STT model expects). Linear is
/// fine for speech ASR and keeps us dependency-light.
pub fn resample_to_16k(input: &[f32], src_rate: u32) -> Vec<f32> {
    if input.is_empty() || src_rate == 16_000 {
        return input.to_vec();
    }
    let ratio = 16_000.0 / src_rate as f64;
    let out_len = (input.len() as f64 * ratio).round() as usize;
    let last = input.len() - 1;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 / ratio;
        let idx = pos.floor() as usize;
        let frac = pos - idx as f64;
        let a = input[idx.min(last)] as f64;
        let b = input[(idx + 1).min(last)] as f64;
        out.push((a + (b - a) * frac) as f32);
    }
    out
}
