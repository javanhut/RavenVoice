//! User configuration, stored at `~/.config/ravenvoice/config.toml`.
//!
//! Every field has a default, so a missing file or a partial file is fine; a
//! file that does not parse is reported and the defaults are used instead.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub stt: SttConfig,
    pub audio: AudioConfig,
    pub hotkeys: HotkeyConfig,
    pub typing: TypingConfig,
    pub tts: TtsConfig,
    pub overlay: OverlayConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SttConfig {
    /// Whisper model name (tiny.en, base.en, small.en, medium.en, large-v3-turbo, ...).
    pub model: String,
    /// Explicit path to a ggml model file; overrides `model`.
    pub model_path: Option<PathBuf>,
    /// Download the model on first start if it is missing.
    pub auto_download: bool,
    /// Spoken language ("en", "de", ... or "auto"). English-only `.en` models ignore it.
    pub language: String,
    /// Inference threads; 0 picks a sensible number for this machine.
    pub threads: usize,
    /// Show a live, provisional transcript in the overlay while you speak.
    pub live_preview: bool,
    /// Feed the tail of what was already dictated back to Whisper so casing
    /// and punctuation stay consistent between phrases.
    pub use_context: bool,
    /// Extra vocabulary hint (names, jargon) given to Whisper as a prompt.
    pub vocabulary: String,
    /// Size Whisper's audio window to the phrase: 3-4x faster on short
    /// phrases. Turn off if you see words go missing.
    pub fast_encoder: bool,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            model: "base.en".into(),
            model_path: None,
            auto_download: true,
            language: "en".into(),
            threads: 0,
            live_preview: true,
            use_context: true,
            vocabulary: String::new(),
            fast_encoder: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Microphone to use, as printed by `ravenvoice devices`. Unset = system default.
    pub device: Option<String>,
    /// How long a pause (ms) ends a phrase and sends it for transcription.
    pub silence_ms: u32,
    /// Phrases shorter than this (ms of speech) are treated as noise.
    pub min_speech_ms: u32,
    /// A phrase is cut and transcribed after this many seconds even without a pause.
    pub max_phrase_secs: f32,
    /// While you speak, transcribe the phrase so far this often (ms) and type
    /// the words that have settled. Lower is snappier and uses more CPU.
    pub stream_interval_ms: u32,
    /// How far above the measured background noise speech must be (ratio).
    pub sensitivity: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device: None,
            silence_ms: 800,
            min_speech_ms: 300,
            max_phrase_secs: 20.0,
            stream_interval_ms: 600,
            sensitivity: 2.5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    /// Read keyboards directly (needs the `input` group) for global shortcuts.
    pub enabled: bool,
    /// Press once to start dictating, again to stop.
    pub toggle: String,
    /// Hold to dictate, release to stop. Empty disables it.
    pub push_to_talk: String,
    /// Read the last dictated text aloud.
    pub speak_last: String,
    /// Silence any speech in progress.
    pub stop_speaking: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            toggle: "Ctrl+Alt+D".into(),
            push_to_talk: String::new(),
            speak_last: "Ctrl+Alt+R".into(),
            stop_speaking: "Ctrl+Alt+X".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TypingConfig {
    /// Type recognised text into the focused app (via /dev/uinput).
    pub enabled: bool,
    /// Type words while you are still speaking, correcting them when the
    /// phrase ends if Whisper changes its mind. Off = type whole phrases.
    pub realtime: bool,
    /// Delay between synthetic key presses. Raise it if an app drops letters.
    pub key_delay_ms: u64,
    /// Understand "new line", "new paragraph", "scratch that", "stop listening", ...
    pub voice_commands: bool,
    /// Characters with no key on a US layout: "skip" them, or type them with
    /// GTK's "ctrl-shift-u" unicode entry (works in GTK apps only).
    pub unicode_fallback: String,
}

impl Default for TypingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            realtime: true,
            key_delay_ms: 4,
            voice_commands: true,
            unicode_fallback: "skip".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    /// "auto" (piper if installed with a voice, else espeak-ng), "piper" or "espeak-ng".
    pub engine: String,
    /// espeak-ng voice, e.g. "en-us", "en-gb", "de".
    pub espeak_voice: String,
    /// Speaking rate in words per minute (espeak-ng) — piper uses `piper_length_scale`.
    pub rate_wpm: u32,
    /// Piper executable; unset searches PATH for `piper-tts` and `piper`.
    pub piper_bin: Option<PathBuf>,
    /// Piper `.onnx` voice; its `.onnx.json` must sit next to it.
    pub piper_model: Option<PathBuf>,
    /// >1 speaks slower, <1 faster.
    pub piper_length_scale: f32,
    /// Read every dictated phrase back aloud (useful without sight of the screen).
    pub echo_dictation: bool,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            engine: "auto".into(),
            espeak_voice: "en-us".into(),
            rate_wpm: 175,
            piper_bin: None,
            piper_model: None,
            piper_length_scale: 1.0,
            echo_dictation: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    /// "top" or "bottom" of the screen.
    pub position: String,
    /// Distance from the screen edge in pixels.
    pub margin: i32,
    /// Base font size in points; raise it for low vision.
    pub font_size: u32,
    /// Start with the full bar; false starts shrunk to the microphone button.
    pub visible: bool,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            position: "bottom".into(),
            margin: 48,
            font_size: 13,
            visible: true,
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ravenvoice")
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ravenvoice")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

impl Config {
    /// Load the config, writing a commented default file on first run.
    pub fn load() -> Config {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    log::error!("{}: {e}; using defaults", path.display());
                    Config::default()
                }
            },
            Err(_) => {
                let cfg = Config::default();
                if let Err(e) = cfg.save() {
                    log::warn!("could not write {}: {e}", path.display());
                }
                cfg
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        std::fs::create_dir_all(config_dir())?;
        let body = toml::to_string_pretty(self)?;
        std::fs::write(
            &path,
            format!("# RavenVoice settings. See README.md for every option.\n\n{body}"),
        )
        .with_context(|| format!("writing {}", path.display()))
    }

    /// Where the Whisper model lives, explicit path first.
    pub fn model_path(&self) -> PathBuf {
        self.stt
            .model_path
            .clone()
            .unwrap_or_else(|| crate::model::path_for(&self.stt.model))
    }
}
