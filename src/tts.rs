//! Text-to-speech.
//!
//! Speech is synthesised by an external engine — Piper (natural neural
//! voices) when one is installed with a voice, espeak-ng otherwise — and
//! played through cpal, so it can be stopped instantly mid-sentence.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use cpal::SampleFormat;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::audio::Resampler;
use crate::config::TtsConfig;

enum Backend {
    Piper {
        bin: PathBuf,
        model: PathBuf,
        rate: u32,
    },
    Espeak {
        bin: PathBuf,
    },
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.is_file())
}

fn piper_sample_rate(model: &Path) -> Result<u32> {
    let json_path = PathBuf::from(format!("{}.json", model.display()));
    let text = std::fs::read_to_string(&json_path)
        .with_context(|| format!("reading {}", json_path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    v["audio"]["sample_rate"]
        .as_u64()
        .map(|r| r as u32)
        .ok_or_else(|| anyhow!("{} has no audio.sample_rate", json_path.display()))
}

fn resolve(cfg: &TtsConfig) -> Result<Backend> {
    let want = cfg.engine.as_str();
    if want == "auto" || want == "piper" {
        let bin = cfg
            .piper_bin
            .clone()
            .or_else(|| which("piper-tts"))
            .or_else(|| which("piper"));
        match (bin, &cfg.piper_model) {
            (Some(bin), Some(model)) if model.exists() => {
                let rate = piper_sample_rate(model)?;
                return Ok(Backend::Piper {
                    bin,
                    model: model.clone(),
                    rate,
                });
            }
            _ if want == "piper" => {
                bail!(
                    "Piper needs both the piper binary and tts.piper_model (an .onnx voice) configured"
                )
            }
            _ => {}
        }
    }
    if let Some(bin) = which("espeak-ng") {
        return Ok(Backend::Espeak { bin });
    }
    bail!("no text-to-speech engine found; install espeak-ng (`imlazy setup` does it for you)")
}

pub fn engine_name(cfg: &TtsConfig) -> String {
    match resolve(cfg) {
        Ok(Backend::Piper { model, .. }) => format!(
            "Piper ({})",
            model.file_stem().unwrap_or_default().to_string_lossy()
        ),
        Ok(Backend::Espeak { .. }) => format!("espeak-ng ({})", cfg.espeak_voice),
        Err(e) => format!("unavailable: {e}"),
    }
}

/// Shared between the thread that speaks and whoever wants to interrupt it.
#[derive(Clone, Default)]
pub struct Interrupt {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Interrupt {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}

/// Speak `text` and return when it has been heard (or was interrupted).
pub fn speak(cfg: &TtsConfig, text: &str, interrupt: &Interrupt) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(());
    }
    interrupt.stop.store(false, Ordering::SeqCst);
    let backend = resolve(cfg)?;
    let mut cmd = match &backend {
        Backend::Piper { bin, model, .. } => {
            let mut c = Command::new(bin);
            c.arg("--model")
                .arg(model)
                .arg("--output_raw")
                .arg("--length_scale")
                .arg(cfg.piper_length_scale.to_string());
            c
        }
        Backend::Espeak { bin } => {
            let mut c = Command::new(bin);
            c.args(["--stdout", "--stdin", "-v", &cfg.espeak_voice, "-s"])
                .arg(cfg.rate_wpm.clamp(80, 450).to_string());
            c
        }
    };
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting the speech engine")?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let errors = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let owned_text = format!("{text}\n");
    std::thread::spawn(move || {
        let _ = stdin.write_all(owned_text.as_bytes());
    });
    *interrupt.child.lock().unwrap() = Some(child);

    let result = match backend {
        Backend::Piper { rate, .. } => Ok((rate, 1)),
        Backend::Espeak { .. } => read_wav_header(&mut stdout),
    }
    .and_then(|(rate, channels)| play_pcm(&mut stdout, rate, channels, &interrupt.stop));
    if let Some(mut child) = interrupt.child.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Surface the engine's own complaint (missing voice, library, ...).
    result.map_err(|e| match errors.join().unwrap_or_default().trim() {
        "" => e,
        msg => e.context(msg.lines().last().unwrap_or(msg).to_string()),
    })
}

/// Stream 16-bit PCM from `src` to the default speakers.
fn play_pcm(
    src: &mut impl Read,
    src_rate: u32,
    src_channels: u16,
    stop: &AtomicBool,
) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no speakers or headphones found")?;
    let def = device.default_output_config()?;
    // Sinks often run natively in 32-bit int; ask for float, which the sound
    // server converts, and fall back to the native format.
    let chosen = device
        .supported_output_configs()
        .ok()
        .and_then(|mut all| {
            all.find(|c| {
                c.sample_format() == SampleFormat::F32
                    && c.channels() == def.channels()
                    && c.min_sample_rate() <= def.sample_rate()
                    && c.max_sample_rate() >= def.sample_rate()
            })
        })
        .map(|c| c.with_sample_rate(def.sample_rate()))
        .unwrap_or(def);
    let format = chosen.sample_format();
    let config = crate::audio::low_latency(&chosen);
    let out_channels = config.channels as usize;

    let queue: Arc<Mutex<VecDeque<f32>>> = Default::default();
    let q = queue.clone();
    let fill = move |out: &mut dyn FnMut(usize, f32), frames: usize| {
        let mut q = q.lock().unwrap();
        for f in 0..frames {
            let s = q.pop_front().unwrap_or(0.0);
            for c in 0..out_channels {
                out(f * out_channels + c, s);
            }
        }
    };
    let err = |e| log::error!("speaker stream error: {e}");
    let stream = match format {
        SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _: &_| {
                let frames = data.len() / out_channels;
                fill(&mut |i, s| data[i] = s, frames)
            },
            err,
            None,
        )?,
        SampleFormat::I16 => device.build_output_stream(
            config,
            move |data: &mut [i16], _: &_| {
                let frames = data.len() / out_channels;
                fill(
                    &mut |i, s| data[i] = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16,
                    frames,
                )
            },
            err,
            None,
        )?,
        SampleFormat::I32 => device.build_output_stream(
            config,
            move |data: &mut [i32], _: &_| {
                let frames = data.len() / out_channels;
                fill(
                    &mut |i, s| data[i] = (s.clamp(-1.0, 1.0) as f64 * i32::MAX as f64) as i32,
                    frames,
                )
            },
            err,
            None,
        )?,
        other => bail!("unsupported speaker sample format {other:?}"),
    };
    let t0 = std::time::Instant::now();
    stream.play()?;
    log::debug!("speaker stream started in {:?}", t0.elapsed());
    let mut queued = 0usize;

    let mut resampler = Resampler::new(src_rate, config.sample_rate);
    let mut buf = [0u8; 8192];
    let mut mono = Vec::new();
    let mut resampled = Vec::new();
    let frame_bytes = 2 * src_channels as usize;
    let mut pending: Vec<u8> = Vec::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            queue.lock().unwrap().clear();
            return Ok(());
        }
        let n = match src.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        let whole = pending.len() / frame_bytes * frame_bytes;
        mono.clear();
        for frame in pending[..whole].chunks_exact(frame_bytes) {
            let sum: f32 = frame
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                .sum();
            mono.push(sum / src_channels as f32);
        }
        pending.drain(..whole);
        resampled.clear();
        resampler.process(&mono, &mut resampled);
        queued += resampled.len();
        queue.lock().unwrap().extend(resampled.iter().copied());
    }
    log::debug!(
        "synthesised {:.2}s of speech by {:?}; {} samples still queued",
        queued as f32 / config.sample_rate as f32,
        t0.elapsed(),
        queue.lock().unwrap().len()
    );
    // Let the queued audio drain, plus a little for the device buffer.
    while !queue.lock().unwrap().is_empty() {
        if stop.load(Ordering::SeqCst) {
            queue.lock().unwrap().clear();
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(150));
    log::debug!("finished playing after {:?}", t0.elapsed());
    Ok(())
}

/// Parse a (possibly streaming) WAV header, leaving `r` at the sample data.
fn read_wav_header(r: &mut impl Read) -> Result<(u32, u16)> {
    let mut riff = [0u8; 12];
    r.read_exact(&mut riff)
        .context("speech engine produced no audio")?;
    if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        bail!("speech engine output is not WAV");
    }
    let mut format = None;
    loop {
        let mut head = [0u8; 8];
        r.read_exact(&mut head)?;
        let len = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
        match &head[0..4] {
            b"fmt " => {
                let mut body = vec![0u8; len];
                r.read_exact(&mut body)?;
                let channels = u16::from_le_bytes([body[2], body[3]]);
                let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                let bits = u16::from_le_bytes([body[14], body[15]]);
                if bits != 16 {
                    bail!("expected 16-bit speech audio, got {bits}-bit");
                }
                format = Some((rate, channels));
            }
            b"data" => return format.ok_or_else(|| anyhow!("WAV data before format")),
            _ => {
                std::io::copy(&mut r.by_ref().take(len as u64), &mut std::io::sink())?;
            }
        }
    }
}
