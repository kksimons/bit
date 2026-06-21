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
    /// Set false if the audio stream couldn't start (no device, no mic permission,
    /// unsupported format…). Lets the toggle path refuse cleanly instead of
    /// silently hanging in a state that captures nothing.
    available: Arc<AtomicBool>,
    /// The reason `available` is false, captured at setup time for the UI.
    error: Arc<Mutex<Option<String>>>,
}

impl Recorder {
    pub fn new() -> Self {
        let recording = Arc::new(AtomicBool::new(false));
        let buffer = Arc::new(Mutex::new(Vec::<f32>::new()));
        let sample_rate = Arc::new(AtomicU32::new(16_000));
        let available = Arc::new(AtomicBool::new(true));
        let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let rec = recording.clone();
        let buf = buffer.clone();
        let sr = sample_rate.clone();
        let avail = available.clone();
        let err = error.clone();
        // Helper: record why the stream failed, then leave `available` false.
        let fail = move |reason: String| {
            eprintln!("[bit] audio unavailable: {reason}");
            *err.lock().unwrap() = Some(reason);
            avail.store(false, Ordering::Relaxed);
        };
        std::thread::spawn(move || {
            let host = cpal::default_host();
            let Some(device) = host.default_input_device() else {
                fail("no microphone input device found".into());
                return;
            };
            let config = match device.default_input_config() {
                Ok(c) => c,
                Err(e) => {
                    fail(format!("microphone not available ({e}). If macOS never prompted, grant Bit Microphone permission in System Settings → Privacy."));
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
                    fail(format!("unsupported sample format: {other:?}"));
                    return;
                }
            };
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    fail(format!("couldn't open microphone ({e}). Grant Bit Microphone permission in System Settings → Privacy if you haven't."));
                    return;
                }
            };
            if let Err(e) = stream.play() {
                fail(format!("couldn't start microphone ({e})"));
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
            available,
            error,
        }
    }

    /// Did the mic stream come up at all? False means permission denied / no
    /// device / unsupported — `last_error()` has the reason.
    pub fn available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }

    pub fn last_error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Relaxed)
    }

    pub fn start(&self) {
        // Idempotent: key auto-repeat fires Pressed repeatedly while held, but we
        // only clear the buffer on the transition into recording.
        if !self.recording.swap(true, Ordering::Relaxed) {
            self.buffer.lock().unwrap().clear();
        }
    }

    /// Stop recording and return the captured mono samples + sample rate.
    /// Returns None if recording was already stopped — so a manual press and an
    /// automatic silence-stop can race and only one wins (no double-processing).
    pub fn stop(&self) -> Option<(Vec<f32>, u32)> {
        if self.recording.swap(false, Ordering::Relaxed) {
            let samples = self.buffer.lock().unwrap().clone();
            Some((samples, self.sample_rate.load(Ordering::Relaxed)))
        } else {
            None
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Relaxed)
    }

    /// RMS energy over the most recent `window` samples (for silence detection).
    pub fn recent_rms(&self, window: usize) -> f32 {
        let b = self.buffer.lock().unwrap();
        let n = b.len().min(window);
        if n == 0 {
            return 0.0;
        }
        let sum: f32 = b[b.len() - n..].iter().map(|x| x * x).sum();
        (sum / n as f32).sqrt()
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
