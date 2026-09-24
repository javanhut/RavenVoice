//! The dictation engine: microphone -> phrase detection -> Whisper -> typing,
//! plus text-to-speech. Runs on background threads and reports to the overlay
//! through `UiEvent`s.
//!
//! Threads:
//! * control – owns the microphone and phrase detector, handles commands, and
//!   watches for microphones being plugged in or pulled out
//! * stt – loads the model (downloading it if needed) and transcribes
//! * typer – types results through the virtual keyboard
//! * tts – speaks text

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, select, tick, unbounded};

use crate::audio::{self, Capture, MicInfo};
use crate::commands::{self, Action};
use crate::config::Config;
use crate::stt::Transcriber;
use crate::tts;
use crate::typer::{HeldModifiers, Typer};
use crate::vad::{Segmenter, VadEvent};

#[derive(Debug, Clone)]
pub enum Cmd {
    Toggle,
    Start,
    Stop,
    PushToTalkDown,
    PushToTalkUp,
    SelectDevice(Option<String>),
    Speak(String),
    SpeakLast,
    StopSpeaking,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    /// Model loaded; the engine is ready.
    Ready {
        model: String,
        tts: String,
    },
    ModelProgress(f32),
    Listening(bool),
    /// Audio is flowing from the microphone; speak now.
    MicLive,
    /// Speech is currently being heard.
    Hearing(bool),
    /// Phrases waiting for Whisper.
    Transcribing(bool),
    Speaking(bool),
    Level(f64),
    Partial(String),
    /// Text that was typed (or a command that ran).
    Final(String),
    Mics {
        list: Vec<MicInfo>,
        selected: Option<String>,
    },
    Notice(String),
    Error(String),
    SetVisible(Option<bool>),
}

/// State other parts of the program (the control socket) can query.
#[derive(Default)]
pub struct Shared {
    pub listening: AtomicBool,
    pub speaking: AtomicBool,
    pub last_text: Mutex<String>,
}

#[derive(Clone)]
pub struct Engine {
    pub cmd: Sender<Cmd>,
    pub ui: async_channel::Sender<UiEvent>,
    pub shared: Arc<Shared>,
    pub held: Arc<HeldModifiers>,
}

/// Phrases are numbered so live typing and the final transcript of the same
/// phrase can be matched up.
enum SttJob {
    Final(u64, Vec<f32>),
    Partial(u64, Vec<f32>),
}

enum TypeJob {
    /// Words of a phrase still being spoken that have stopped changing.
    Stream(u64, String),
    /// The finished phrase.
    Final(u64, Vec<Action>),
    Reset,
}

impl Engine {
    pub fn start(cfg: Config, ui: async_channel::Sender<UiEvent>) -> Engine {
        let (cmd_tx, cmd_rx) = unbounded();
        let shared = Arc::new(Shared::default());
        let held = Arc::new(HeldModifiers::default());
        let engine = Engine {
            cmd: cmd_tx,
            ui,
            shared,
            held,
        };

        let (final_tx, final_rx) = unbounded();
        let (partial_tx, partial_rx) = bounded(1);
        let (type_tx, type_rx) = unbounded();
        let (speak_tx, speak_rx) = unbounded::<String>();
        let interrupt = tts::Interrupt::default();
        let pending = Arc::new(AtomicUsize::new(0));

        {
            let (e, cfg) = (engine.clone(), cfg.clone());
            let type_tx = type_tx.clone();
            let pending = pending.clone();
            spawn("stt", move || {
                stt_thread(e, cfg, final_rx, partial_rx, type_tx, pending)
            });
        }
        {
            let (e, cfg) = (engine.clone(), cfg.clone());
            spawn("typer", move || typer_thread(e, cfg, type_rx));
        }
        {
            let (e, cfg, interrupt) = (engine.clone(), cfg.clone(), interrupt.clone());
            spawn("tts", move || tts_thread(e, cfg, speak_rx, interrupt));
        }
        {
            let e = engine.clone();
            spawn("control", move || {
                Control::new(
                    e, cfg, final_tx, partial_tx, type_tx, speak_tx, interrupt, pending,
                )
                .run(cmd_rx)
            });
        }
        engine
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd.send(cmd);
    }

    fn ui(&self, ev: UiEvent) {
        let _ = self.ui.send_blocking(ev);
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .expect("spawning engine thread");
}

struct Control {
    e: Engine,
    cfg: Config,
    host: cpal::Host,
    /// The open microphone stream. It stays open (paused) between dictations
    /// because opening one costs ~2 s while resuming costs ~0.5 s.
    capture: Option<Capture>,
    /// What `capture` was opened for: a configured id, or None for "system default".
    capture_request: Option<String>,
    capture_failed: Arc<AtomicBool>,
    audio_tx: Sender<Vec<f32>>,
    audio_rx: Receiver<Vec<f32>>,
    listening: bool,
    /// Listening, but no audio has arrived yet (the microphone is waking up).
    awaiting_audio: bool,
    started_at: Instant,
    seg: Segmenter,
    phrase_id: u64,
    push_to_talk: bool,
    mics: Vec<MicInfo>,
    final_tx: Sender<SttJob>,
    partial_tx: Sender<SttJob>,
    type_tx: Sender<TypeJob>,
    speak_tx: Sender<String>,
    interrupt: tts::Interrupt,
    pending: Arc<AtomicUsize>,
    last_level: Instant,
}

impl Control {
    #[allow(clippy::too_many_arguments)]
    fn new(
        e: Engine,
        cfg: Config,
        final_tx: Sender<SttJob>,
        partial_tx: Sender<SttJob>,
        type_tx: Sender<TypeJob>,
        speak_tx: Sender<String>,
        interrupt: tts::Interrupt,
        pending: Arc<AtomicUsize>,
    ) -> Self {
        let (audio_tx, audio_rx) = bounded(256);
        Self {
            seg: Segmenter::new(&cfg.audio),
            phrase_id: 0,
            host: cpal::default_host(),
            e,
            cfg,
            capture: None,
            capture_request: None,
            capture_failed: Arc::new(AtomicBool::new(false)),
            audio_tx,
            audio_rx,
            listening: false,
            awaiting_audio: false,
            started_at: Instant::now(),
            push_to_talk: false,
            mics: Vec::new(),
            final_tx,
            partial_tx,
            type_tx,
            speak_tx,
            interrupt,
            pending,
            last_level: Instant::now(),
        }
    }

    fn run(mut self, cmd_rx: Receiver<Cmd>) {
        log::info!("audio host: {:?}", self.host.id());
        self.refresh_mics(false);
        self.prewarm(self.cfg.audio.device.clone());
        let ticker = tick(Duration::from_secs(2));
        loop {
            select! {
                recv(cmd_rx) -> cmd => match cmd {
                    Ok(cmd) => self.command(cmd),
                    Err(_) => return,
                },
                recv(self.audio_rx) -> chunk => {
                    if let Ok(chunk) = chunk {
                        self.audio(chunk);
                    }
                },
                recv(ticker) -> _ => {
                    self.check_stream();
                    self.refresh_mics(true);
                },
            }
        }
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Toggle => {
                if self.listening {
                    self.stop()
                } else {
                    self.start()
                }
            }
            Cmd::Start => self.start(),
            Cmd::Stop => self.stop(),
            Cmd::PushToTalkDown => {
                if !self.listening {
                    self.push_to_talk = true;
                    self.start();
                }
            }
            Cmd::PushToTalkUp => {
                if self.push_to_talk {
                    self.stop();
                }
            }
            Cmd::SelectDevice(id) => {
                self.cfg.audio.device = id.clone();
                if let Err(e) = self.cfg.save() {
                    log::warn!("saving microphone choice: {e}");
                }
                self.switch_mic(id);
                self.send_mics();
            }
            Cmd::Speak(text) => {
                self.interrupt.stop();
                let _ = self.speak_tx.send(text);
            }
            Cmd::SpeakLast => {
                let last = self.e.shared.last_text.lock().unwrap().clone();
                self.interrupt.stop();
                if last.is_empty() {
                    let _ = self.speak_tx.send("Nothing has been dictated yet.".into());
                } else {
                    let _ = self.speak_tx.send(last);
                }
            }
            Cmd::StopSpeaking => self.interrupt.stop(),
        }
    }

    /// Make sure a stream is open for `request` (paused unless listening).
    fn ensure_stream(&mut self, request: Option<String>) -> anyhow::Result<()> {
        let healthy = !self.capture_failed.load(Ordering::SeqCst);
        if self.capture.is_some() && healthy && self.capture_request == request {
            return Ok(());
        }
        self.capture = None;
        self.capture_failed.store(false, Ordering::SeqCst);
        let cap = Capture::open(
            &self.host,
            request.as_deref(),
            self.audio_tx.clone(),
            self.capture_failed.clone(),
        )?;
        self.capture_request = request;
        self.capture = Some(cap);
        Ok(())
    }

    /// Open the microphone in the background once, so the first dictation
    /// starts quickly. Audio that arrives meanwhile is discarded.
    fn prewarm(&mut self, request: Option<String>) {
        if self.ensure_stream(request).is_ok()
            && let Some(cap) = &self.capture
            && cap.resume().is_ok()
        {
            cap.pause();
        }
        while self.audio_rx.try_recv().is_ok() {}
    }

    fn start(&mut self) {
        self.start_with(self.cfg.audio.device.clone());
    }

    fn start_with(&mut self, request: Option<String>) {
        if self.listening {
            return;
        }
        let _ = self.type_tx.send(TypeJob::Reset);
        self.seg = Segmenter::new(&self.cfg.audio);
        while self.audio_rx.try_recv().is_ok() {}
        let opened = self
            .ensure_stream(request)
            .and_then(|_| self.capture.as_ref().expect("just opened").resume());
        match opened {
            Ok(()) => {
                let name = self
                    .capture
                    .as_ref()
                    .map(|c| c.device_name.clone())
                    .unwrap_or_default();
                log::info!("listening on {name}");
                self.listening = true;
                self.awaiting_audio = true;
                self.started_at = Instant::now();
                self.e.shared.listening.store(true, Ordering::SeqCst);
                self.e.ui(UiEvent::Listening(true));
            }
            Err(err) => {
                self.push_to_talk = false;
                self.e.ui(UiEvent::Error(format!(
                    "Could not open the microphone: {err:#}"
                )));
            }
        }
    }

    fn stop(&mut self) {
        if self.listening {
            if let Some(cap) = &self.capture {
                cap.pause();
            }
            // Take what the microphone already delivered, then close the
            // phrase so the last words spoken are not lost.
            while let Ok(chunk) = self.audio_rx.try_recv() {
                self.feed(chunk);
            }
            let mut events = Vec::new();
            self.seg.flush(&mut events);
            self.handle_vad(events);
        }
        self.listening = false;
        self.awaiting_audio = false;
        self.push_to_talk = false;
        self.e.shared.listening.store(false, Ordering::SeqCst);
        self.e.ui(UiEvent::Listening(false));
        self.e.ui(UiEvent::Hearing(false));
        self.e.ui(UiEvent::Level(0.0));
    }

    /// Move to another microphone, carrying on dictation if it was running.
    fn switch_mic(&mut self, request: Option<String>) {
        let was_listening = self.listening;
        let ptt = self.push_to_talk;
        if was_listening {
            self.stop();
        }
        self.capture = None;
        if was_listening {
            self.push_to_talk = ptt;
            self.start_with(request);
        } else {
            self.prewarm(request);
        }
    }

    fn audio(&mut self, chunk: Vec<f32>) {
        if !self.listening {
            return;
        }
        if self.awaiting_audio {
            self.awaiting_audio = false;
            log::debug!(
                "microphone live {:?} after start",
                self.started_at.elapsed()
            );
            self.e.ui(UiEvent::MicLive);
        }
        // Don't transcribe our own voice coming out of the speakers.
        if self.e.shared.speaking.load(Ordering::SeqCst) {
            return;
        }
        self.feed(chunk);
        if self.last_level.elapsed() >= Duration::from_millis(50) {
            self.last_level = Instant::now();
            self.e
                .ui(UiEvent::Level(audio::meter_level(self.seg.last_rms)));
        }
    }

    fn feed(&mut self, chunk: Vec<f32>) {
        let mut events = Vec::new();
        self.seg.push(&chunk, &mut events);
        self.handle_vad(events);
    }

    fn handle_vad(&mut self, events: Vec<VadEvent>) {
        for ev in events {
            match ev {
                VadEvent::SpeechStarted => {
                    self.phrase_id += 1;
                    self.e.ui(UiEvent::Hearing(true));
                }
                VadEvent::Partial(clip) => {
                    let typing_live = self.cfg.typing.enabled && self.cfg.typing.realtime;
                    if self.cfg.stt.live_preview || typing_live {
                        let _ = self
                            .partial_tx
                            .try_send(SttJob::Partial(self.phrase_id, clip));
                    }
                }
                VadEvent::Phrase(clip) => {
                    self.e.ui(UiEvent::Hearing(false));
                    self.pending.fetch_add(1, Ordering::SeqCst);
                    self.e.ui(UiEvent::Transcribing(true));
                    let _ = self.final_tx.send(SttJob::Final(self.phrase_id, clip));
                }
            }
        }
    }

    /// The microphone vanished mid-stream (usually unplugged).
    fn check_stream(&mut self) {
        if self.capture.is_none() || !self.capture_failed.load(Ordering::SeqCst) {
            return;
        }
        let lost = self
            .capture
            .as_ref()
            .map(|c| c.device_name.clone())
            .unwrap_or_default();
        self.capture = None;
        if self.listening {
            self.e.ui(UiEvent::Notice(format!(
                "{lost} disconnected — switching to the system default microphone"
            )));
            self.switch_mic(None);
        }
    }

    fn refresh_mics(&mut self, announce: bool) {
        let list = audio::list_inputs(&self.host);
        if list == self.mics {
            return;
        }
        if announce {
            for mic in list
                .iter()
                .filter(|m| !self.mics.iter().any(|o| o.id == m.id))
            {
                self.e.ui(UiEvent::Notice(format!(
                    "Microphone connected: {}",
                    mic.display()
                )));
            }
        }
        let preferred = self.cfg.audio.device.clone();
        let preferred_present = preferred
            .as_ref()
            .is_some_and(|p| list.iter().any(|m| &m.id == p));
        let default_id = list.iter().find(|m| m.is_default).map(|m| m.id.clone());
        let current_id = self.capture.as_ref().map(|c| c.id.clone());
        self.mics = list;
        self.send_mics();

        if preferred_present && self.capture_request != preferred {
            // The chosen microphone came back after being unplugged.
            self.switch_mic(preferred);
        } else if !preferred_present && current_id.is_some() && current_id != default_id {
            // Following "System default" and the system switched microphones
            // (e.g. a headset was plugged in).
            self.switch_mic(None);
        }
    }

    fn send_mics(&self) {
        self.e.ui(UiEvent::Mics {
            list: self.mics.clone(),
            selected: self.cfg.audio.device.clone(),
        });
    }
}

fn stt_thread(
    e: Engine,
    cfg: Config,
    final_rx: Receiver<SttJob>,
    partial_rx: Receiver<SttJob>,
    type_tx: Sender<TypeJob>,
    pending: Arc<AtomicUsize>,
) {
    let path = cfg.model_path();
    if !path.exists() && cfg.stt.model_path.is_none() && cfg.stt.auto_download {
        e.ui(UiEvent::Notice(format!(
            "Downloading the {} speech model (one time)…",
            cfg.stt.model
        )));
        let ui = e.ui.clone();
        if let Err(err) = crate::model::download(&cfg.stt.model, &path, |f| {
            let _ = ui.send_blocking(UiEvent::ModelProgress(f));
        }) {
            e.ui(UiEvent::Error(format!("Model download failed: {err:#}")));
            return;
        }
    }
    let mut stt = match Transcriber::load(&path, &cfg.stt) {
        Ok(t) => t,
        Err(err) => {
            e.ui(UiEvent::Error(format!("{err:#}")));
            return;
        }
    };
    e.ui(UiEvent::Ready {
        model: cfg.stt.model.clone(),
        tts: tts::engine_name(&cfg.tts),
    });

    let typing_live = cfg.typing.enabled && cfg.typing.realtime;
    let mut live = LiveWords::default();
    let mut last_final_id = 0u64;
    let mut context = String::new();
    loop {
        // Finished phrases always win over live previews.
        let job = match final_rx.try_recv() {
            Ok(job) => job,
            Err(_) => select! {
                recv(final_rx) -> j => match j { Ok(j) => j, Err(_) => return },
                recv(partial_rx) -> j => match j { Ok(j) => j, Err(_) => return },
            },
        };
        let ctx = if cfg.stt.use_context {
            context.as_str()
        } else {
            ""
        };
        match job {
            SttJob::Partial(id, clip) => {
                // Stale once that phrase is finished or another is waiting.
                if id <= last_final_id || !final_rx.is_empty() {
                    continue;
                }
                let Ok(text) = stt.transcribe(&clip, ctx, true) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                if typing_live && let Some(settled) = live.update(id, &text, &cfg) {
                    let _ = type_tx.send(TypeJob::Stream(id, settled));
                }
                if cfg.stt.live_preview {
                    e.ui(UiEvent::Partial(text));
                }
            }
            SttJob::Final(id, clip) => {
                last_final_id = id;
                let started = Instant::now();
                let result = stt.transcribe(&clip, ctx, false);
                let left = pending.fetch_sub(1, Ordering::SeqCst) - 1;
                if left == 0 {
                    e.ui(UiEvent::Transcribing(false));
                }
                let text = match result {
                    Ok(t) => t,
                    Err(err) => {
                        e.ui(UiEvent::Error(format!("{err:#}")));
                        continue;
                    }
                };
                log::info!(
                    "{:.1}s of audio -> {:?} in {} ms",
                    clip.len() as f32 / audio::SAMPLE_RATE as f32,
                    text,
                    started.elapsed().as_millis()
                );
                if text.is_empty() {
                    // Take back anything typed live for a phrase that turned out to be nothing.
                    let _ = type_tx.send(TypeJob::Final(id, Vec::new()));
                    e.ui(UiEvent::Partial(String::new()));
                    continue;
                }
                let actions = commands::interpret(&text, cfg.typing.voice_commands);
                if actions.contains(&Action::StopListening) {
                    let _ = type_tx.send(TypeJob::Final(id, Vec::new()));
                    e.send(Cmd::Stop);
                    e.ui(UiEvent::Final("Stopped listening".into()));
                    continue;
                }
                let typed_text: String = actions
                    .iter()
                    .filter_map(|a| match a {
                        Action::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                if !typed_text.is_empty() {
                    context.push(' ');
                    context.push_str(&typed_text);
                    *e.shared.last_text.lock().unwrap() = typed_text.clone();
                    if cfg.tts.echo_dictation {
                        e.send(Cmd::Speak(typed_text));
                    }
                }
                e.ui(UiEvent::Final(describe(&actions)));
                let _ = type_tx.send(TypeJob::Final(id, actions));
            }
        }
    }
}

/// Decides which words of a phrase still being spoken are safe to type.
///
/// Whisper revises its guess as more audio arrives, so a word is only typed
/// once two successive transcripts agree on it, and never the last word
/// heard (it may be cut off mid-syllable). Anything that could still become
/// a spoken command waits for the final transcript.
#[derive(Default)]
struct LiveWords {
    id: u64,
    previous: Vec<String>,
    settled: String,
}

impl LiveWords {
    /// Returns the settled text for the phrase when it has grown.
    fn update(&mut self, id: u64, text: &str, cfg: &Config) -> Option<String> {
        if id != self.id {
            *self = LiveWords {
                id,
                ..Default::default()
            };
        }
        let words: Vec<String> = text.split_whitespace().map(str::to_string).collect();
        let key = |w: &str| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        };
        let agreed = self
            .previous
            .iter()
            .zip(&words)
            .take_while(|(a, b)| key(a) == key(b))
            .count();
        let mut n = agreed.min(words.len().saturating_sub(1));
        if cfg.typing.voice_commands {
            // Stop before a possible "new line" / "new paragraph".
            if let Some(i) = words[..n]
                .iter()
                .position(|w| commands::may_start_inline_command(w))
            {
                n = i;
            }
        }
        self.previous = words.clone();
        let candidate = words[..n].join(" ");
        if cfg.typing.voice_commands && commands::could_be_command(&candidate) {
            return None;
        }
        if candidate.len() > self.settled.len() && candidate.starts_with(&self.settled) {
            self.settled = candidate.clone();
            return Some(candidate);
        }
        None
    }
}

/// What the overlay shows for a phrase: the text, or the command that ran.
fn describe(actions: &[Action]) -> String {
    use commands::NamedKey;
    actions
        .iter()
        .map(|a| match a {
            Action::Text(t) => t.clone(),
            Action::DeleteLast => "⌫ deleted last phrase".into(),
            Action::StopListening => "stopped".into(),
            Action::Key(k) => match k.key {
                NamedKey::Enter if !k.ctrl => "↵".into(),
                NamedKey::Char(c) if k.ctrl => format!("Ctrl+{}", c.to_ascii_uppercase()),
                other => format!("{other:?}"),
            },
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn typer_thread(e: Engine, cfg: Config, rx: Receiver<TypeJob>) {
    if !cfg.typing.enabled {
        log::info!("typing disabled; transcripts are only shown in the overlay");
    }
    let mut typer: Option<Typer> = None;
    let mut warned = false;
    for job in rx {
        if !cfg.typing.enabled {
            continue;
        }
        if typer.is_none() {
            match Typer::new(&cfg.typing, e.held.clone()) {
                Ok(t) => typer = Some(t),
                Err(err) => {
                    if !warned {
                        warned = true;
                        e.ui(UiEvent::Error(format!("{err:#}")));
                    }
                    continue;
                }
            }
        }
        let t = typer.as_mut().expect("created above");
        let result = match job {
            TypeJob::Reset => {
                t.reset();
                Ok(())
            }
            TypeJob::Stream(id, text) => t.stream(id, &text),
            TypeJob::Final(id, actions) => t.finish(id, &actions),
        };
        if let Err(err) = result {
            e.ui(UiEvent::Error(format!("Typing failed: {err:#}")));
            typer = None;
        }
    }
}

fn tts_thread(e: Engine, cfg: Config, rx: Receiver<String>, interrupt: tts::Interrupt) {
    for text in rx {
        e.shared.speaking.store(true, Ordering::SeqCst);
        e.ui(UiEvent::Speaking(true));
        if let Err(err) = tts::speak(&cfg.tts, &text, &interrupt) {
            e.ui(UiEvent::Error(format!("Speech failed: {err:#}")));
        }
        e.shared.speaking.store(false, Ordering::SeqCst);
        e.ui(UiEvent::Speaking(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed successive partial transcripts; collect what would be typed.
    fn stream(partials: &[&str]) -> Vec<String> {
        let cfg = Config::default();
        let mut live = LiveWords::default();
        partials
            .iter()
            .filter_map(|p| live.update(1, p, &cfg))
            .collect()
    }

    #[test]
    fn types_words_once_two_transcripts_agree() {
        assert_eq!(
            stream(&[
                "And so",
                "And so my fellow",
                "And so my fellow Americans ask"
            ]),
            vec!["And so".to_string(), "And so my fellow".to_string()]
        );
    }

    #[test]
    fn never_types_the_newest_word() {
        assert!(stream(&["Hello", "Hello"]).is_empty());
    }

    #[test]
    fn waits_on_possible_commands() {
        assert!(stream(&["Scratch that", "Scratch that"]).is_empty());
        assert_eq!(
            stream(&["Dear Sam, new line", "Dear Sam, new line thanks"]),
            vec!["Dear Sam,".to_string()]
        );
    }

    #[test]
    fn ignores_revisions_of_settled_words() {
        // "fellow" became "yellow": nothing already typed is retracted live.
        assert_eq!(
            stream(&[
                "my fellow Americans",
                "my fellow Americans ask",
                "my yellow Americans ask not"
            ]),
            vec!["my fellow Americans".to_string()]
        );
    }
}
