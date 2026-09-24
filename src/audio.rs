//! Microphone discovery and capture through cpal.
//!
//! On Raven the PulseAudio host (served by pipewire-pulse) is used, which gives
//! human-readable names ("Blue Yeti", "Built-in Audio Analog Stereo") and lets
//! "System default" follow whatever microphone PipeWire routes to. ALSA is the
//! fallback when no sound server is running.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SampleRate, StreamConfig, SupportedBufferSize};
use crossbeam_channel::Sender;

/// Whisper wants 16 kHz mono f32.
pub const SAMPLE_RATE: u32 = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicKind {
    BuiltIn,
    Usb,
    Bluetooth,
    Other,
}

impl MicKind {
    pub fn label(self) -> &'static str {
        match self {
            MicKind::BuiltIn => "built-in",
            MicKind::Usb => "USB",
            MicKind::Bluetooth => "Bluetooth",
            MicKind::Other => "external",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MicInfo {
    /// Stable id (`pulseaudio:alsa_input.usb-...`), what the config stores.
    pub id: String,
    pub name: String,
    pub kind: MicKind,
    pub is_default: bool,
}

impl MicInfo {
    pub fn display(&self) -> String {
        format!("{} ({})", self.name, self.kind.label())
    }
}

/// Guess what kind of microphone this is from its sound-server name.
fn classify(id: &str, name: &str) -> MicKind {
    let id = id.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    if id.contains("bluez") || name.contains("bluetooth") {
        MicKind::Bluetooth
    } else if id.contains(".usb-") || id.contains("usb") {
        MicKind::Usb
    } else if id.contains(".pci-") || id.contains("platform-") || name.contains("built-in") {
        MicKind::BuiltIn
    } else {
        MicKind::Other
    }
}

/// Every capture source except loopback "Monitor of ..." sources.
pub fn list_inputs(host: &cpal::Host) -> Vec<MicInfo> {
    let default_id = host
        .default_input_device()
        .and_then(|d| d.id().ok())
        .map(|id| id.to_string());
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut mics: Vec<MicInfo> = devices
        .filter_map(|d| {
            let id = d.id().ok()?.to_string();
            let name = d.description().ok()?.name().to_string();
            if id.ends_with(".monitor") || name.starts_with("Monitor of") {
                return None;
            }
            // Plain ALSA lists a PCM per plugin; keep the ones people pick.
            if id.starts_with("alsa:")
                && !(id.contains("sysdefault")
                    || id.ends_with(":default")
                    || id.contains("pipewire"))
            {
                return None;
            }
            Some(MicInfo {
                kind: classify(&id, &name),
                is_default: default_id.as_deref() == Some(id.as_str()),
                id,
                name,
            })
        })
        .collect();
    mics.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.name.cmp(&b.name)));
    mics
}

pub fn find_device(host: &cpal::Host, id: Option<&str>) -> Result<cpal::Device> {
    if let Some(id) = id {
        let parsed = id
            .parse()
            .map_err(|e| anyhow!("bad device id {id:?}: {e:?}"))?;
        if let Some(dev) = host.device_by_id(&parsed) {
            return Ok(dev);
        }
        log::warn!("microphone {id} not present, using the system default");
    }
    host.default_input_device()
        .ok_or_else(|| anyhow!("no microphone found — is one plugged in and unmuted?"))
}

/// Streaming linear resampler with a one-pole low-pass in front, which is
/// plenty for speech going into Whisper.
pub struct Resampler {
    step: f64,
    t: f64,
    prev: f32,
    lp: f32,
    alpha: f32,
    bypass: bool,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let cutoff = 0.45 * to as f32;
        let alpha = 1.0 - (-2.0 * std::f32::consts::PI * cutoff / from as f32).exp();
        Self {
            step: from as f64 / to as f64,
            t: 0.0,
            prev: 0.0,
            lp: 0.0,
            alpha,
            bypass: from == to,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.bypass {
            out.extend_from_slice(input);
            return;
        }
        for &x in input {
            self.lp += self.alpha * (x - self.lp);
            let x = self.lp;
            while self.t < 1.0 {
                out.push(self.prev + (x - self.prev) * self.t as f32);
                self.t += self.step;
            }
            self.t -= 1.0;
            self.prev = x;
        }
    }
}

/// A running microphone stream. Dropping it closes the microphone.
pub struct Capture {
    stream: cpal::Stream,
    /// The concrete device opened (the default resolved to a real id).
    pub id: String,
    pub device_name: String,
}

impl Capture {
    /// Open the microphone, paused. Once resumed it sends 16 kHz mono chunks
    /// to `tx`. `failed` is set if the device disappears (unplugged).
    pub fn open(
        host: &cpal::Host,
        device_id: Option<&str>,
        tx: Sender<Vec<f32>>,
        failed: Arc<AtomicBool>,
    ) -> Result<Capture> {
        let device = find_device(host, device_id)?;
        let id = device.id().map(|i| i.to_string()).unwrap_or_default();
        let device_name = device
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "microphone".into());

        let (config, format) = pick_config(&device)?;
        let channels = config.channels as usize;
        log::info!(
            "opened {device_name}: {} Hz, {channels} ch, {format:?}",
            config.sample_rate
        );

        let mut resampler = Resampler::new(config.sample_rate, SAMPLE_RATE);
        let mut mono = Vec::new();
        let mut out = Vec::new();
        let mut deliver = move |frames: &mut dyn Iterator<Item = f32>| {
            mono.clear();
            let mut acc = 0.0;
            for (i, s) in frames.enumerate() {
                acc += s;
                if i % channels == channels - 1 {
                    mono.push(acc / channels as f32);
                    acc = 0.0;
                }
            }
            out.clear();
            resampler.process(&mono, &mut out);
            // If the engine falls behind, dropping audio beats unbounded memory.
            let _ = tx.try_send(std::mem::take(&mut out));
        };

        let err_flag = failed.clone();
        let on_err = move |e: cpal::Error| {
            log::error!("microphone stream error: {e}");
            err_flag.store(true, Ordering::SeqCst);
        };

        let stream = match format {
            SampleFormat::F32 => device.build_input_stream(
                config,
                move |data: &[f32], _: &_| deliver(&mut data.iter().copied()),
                on_err,
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                config,
                move |data: &[i16], _: &_| {
                    deliver(&mut data.iter().map(|&s| s as f32 / i16::MAX as f32))
                },
                on_err,
                None,
            ),
            SampleFormat::I32 => device.build_input_stream(
                config,
                move |data: &[i32], _: &_| {
                    deliver(&mut data.iter().map(|&s| s as f32 / i32::MAX as f32))
                },
                on_err,
                None,
            ),
            SampleFormat::U16 => device.build_input_stream(
                config,
                move |data: &[u16], _: &_| {
                    deliver(&mut data.iter().map(|&s| (s as f32 - 32768.0) / 32768.0))
                },
                on_err,
                None,
            ),
            other => return Err(anyhow!("unsupported microphone sample format {other:?}")),
        }
        .with_context(|| format!("opening {device_name}"))?;
        let _ = stream.pause();
        Ok(Capture {
            stream,
            id,
            device_name,
        })
    }

    pub fn resume(&self) -> Result<()> {
        self.stream.play().context("starting the microphone")
    }

    pub fn pause(&self) {
        let _ = self.stream.pause();
    }
}

/// Prefer 16 kHz mono straight from the sound server (no resampling on our
/// side); otherwise take the device default and resample.
fn pick_config(device: &cpal::Device) -> Result<(StreamConfig, SampleFormat)> {
    const PREFERRED: [SampleFormat; 2] = [SampleFormat::F32, SampleFormat::I16];
    if let Ok(configs) = device.supported_input_configs() {
        let configs: Vec<_> = configs.collect();
        for fmt in PREFERRED {
            for channels in [1, 2] {
                if let Some(c) = configs.iter().find(|c| {
                    c.sample_format() == fmt
                        && c.channels() == channels
                        && c.min_sample_rate() <= SAMPLE_RATE
                        && c.max_sample_rate() >= SAMPLE_RATE
                }) {
                    let cfg = c.with_sample_rate(SAMPLE_RATE as SampleRate);
                    return Ok((low_latency(&cfg), fmt));
                }
            }
        }
    }
    let def = device
        .default_input_config()
        .context("microphone reports no usable format")?;
    Ok((low_latency(&def), def.sample_format()))
}

/// Ask for ~30 ms buffers. Left at the default, PulseAudio buffers about two
/// seconds, which delays the start of dictation and clips the end of speech.
pub fn low_latency(supported: &cpal::SupportedStreamConfig) -> StreamConfig {
    let mut config = supported.config();
    let frames = config.sample_rate * 30 / 1000;
    if let SupportedBufferSize::Range { min, max } = supported.buffer_size() {
        config.buffer_size = BufferSize::Fixed(frames.clamp(*min, *max));
    }
    config
}

/// Root-mean-square of a block, the loudness measure used for the level meter and VAD.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// Map an RMS value onto 0..1 for a meter (-60 dBFS .. -10 dBFS).
pub fn meter_level(rms: f32) -> f64 {
    let db = 20.0 * rms.max(1e-6).log10();
    ((db + 60.0) / 50.0).clamp(0.0, 1.0) as f64
}
