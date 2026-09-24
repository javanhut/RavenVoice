//! Whisper speech-to-text.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

use crate::audio::SAMPLE_RATE;
use crate::config::SttConfig;

pub struct Transcriber {
    state: WhisperState,
    threads: i32,
    language: Option<String>,
    vocabulary: String,
    fast_encoder: bool,
}

impl Transcriber {
    pub fn load(model: &Path, cfg: &SttConfig) -> Result<Self> {
        whisper_rs::install_logging_hooks();
        if !model.exists() {
            return Err(anyhow!(
                "Whisper model not found at {}; run `ravenvoice download-model {}`",
                model.display(),
                cfg.model
            ));
        }
        let path = model.to_str().context("model path is not UTF-8")?;
        let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .with_context(|| format!("loading {}", model.display()))?;
        let english_only = !ctx.is_multilingual();
        let state = ctx.create_state().context("creating Whisper state")?;
        let threads = if cfg.threads > 0 {
            cfg.threads
        } else {
            std::thread::available_parallelism()
                .map_or(4, |n| n.get())
                .clamp(1, 8)
        } as i32;
        let language = match cfg.language.as_str() {
            _ if english_only => Some("en".to_string()),
            "" | "auto" => None,
            l => Some(l.to_string()),
        };
        Ok(Self {
            state,
            threads,
            language,
            vocabulary: cfg.vocabulary.clone(),
            fast_encoder: cfg.fast_encoder,
        })
    }

    /// Transcribe one phrase. `context` is recently dictated text, used as a
    /// style hint so capitalisation and punctuation carry across phrases.
    pub fn transcribe(&mut self, audio: &[f32], context: &str, fast: bool) -> Result<String> {
        // Whisper refuses clips under a second; pad with silence.
        let min = SAMPLE_RATE as usize + SAMPLE_RATE as usize / 10;
        let padded;
        let audio = if audio.len() < min {
            padded = {
                let mut v = audio.to_vec();
                v.resize(min, 0.0);
                v
            };
            &padded[..]
        } else {
            audio
        };

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(self.threads);
        params.set_language(self.language.as_deref());
        params.set_translate(false);
        params.set_no_context(true);
        params.set_no_timestamps(true);
        params.set_single_segment(fast);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_temperature(0.0);
        if self.fast_encoder {
            params.set_audio_ctx(encoder_window(audio.len(), fast));
        }
        let prompt = build_prompt(&self.vocabulary, context);
        if !prompt.is_empty() {
            params.set_initial_prompt(&prompt);
        }

        self.state
            .full(params, audio)
            .context("Whisper inference failed")?;

        let mut text = String::new();
        for i in 0..self.state.full_n_segments() {
            let Some(seg) = self.state.get_segment(i) else {
                continue;
            };
            if seg.no_speech_probability() > 0.6 {
                continue;
            }
            if let Ok(s) = seg.to_str_lossy() {
                text.push_str(&s);
            }
        }
        Ok(clean(&text))
    }
}

/// Whisper always encodes a 30 s window (1500 frames, 50 per second). Short
/// dictation phrases need far less, and shrinking the window makes them 3-4x
/// faster. It must comfortably cover the clip, or Whisper loops and invents
/// text, hence the generous margin and floor for final transcripts.
fn encoder_window(samples: usize, preview: bool) -> i32 {
    let needed = samples as f32 / SAMPLE_RATE as f32 * 50.0;
    let ctx = if preview {
        needed + 128.0
    } else {
        needed * 1.5 + 256.0
    };
    let floor = if preview { 512.0 } else { 768.0 };
    ctx.max(floor).min(1500.0).ceil() as i32
}

fn build_prompt(vocabulary: &str, context: &str) -> String {
    let tail: String = {
        let chars: Vec<char> = context.chars().collect();
        chars[chars.len().saturating_sub(200)..].iter().collect()
    };
    match (vocabulary.trim(), tail.trim()) {
        ("", t) => t.to_string(),
        (v, "") => v.to_string(),
        (v, t) => format!("{v} {t}"),
    }
}

/// Drop Whisper's non-speech annotations and the phrases it tends to invent
/// for silence, and tidy whitespace.
pub fn clean(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0u32;
    let mut in_stars = false;
    for c in raw.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' if depth > 0 => depth -= 1,
            '*' => in_stars = !in_stars,
            _ if depth == 0 && !in_stars => out.push(c),
            _ => {}
        }
    }
    let text = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = text.to_lowercase();
    const HALLUCINATIONS: &[&str] = &[
        "thank you.",
        "thanks for watching!",
        "thank you for watching.",
        "you",
        ".",
        "bye.",
    ];
    if HALLUCINATIONS.contains(&lower.as_str()) {
        return String::new();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{SAMPLE_RATE, clean};

    #[test]
    fn encoder_window_covers_the_clip() {
        let secs = |s: f32| (s * SAMPLE_RATE as f32) as usize;
        assert_eq!(super::encoder_window(secs(2.0), false), 768);
        assert!(super::encoder_window(secs(11.0), false) >= 1000);
        assert_eq!(super::encoder_window(secs(25.0), false), 1500);
        for s in [1.0, 5.0, 10.0, 20.0, 30.0] {
            assert!(super::encoder_window(secs(s), true) as f32 >= s * 50.0);
        }
    }

    #[test]
    fn strips_annotations() {
        assert_eq!(clean(" [BLANK_AUDIO]"), "");
        assert_eq!(clean(" Hello (coughs) world. *music*"), "Hello world.");
        assert_eq!(clean("  Thank you."), "");
        assert_eq!(
            clean(" Thank you for the report."),
            "Thank you for the report."
        );
    }
}
