//! RavenVoice — offline dictation and read-aloud for Raven Linux.

mod audio;
mod commands;
mod config;
mod engine;
mod hotkey;
mod ipc;
mod model;
mod stt;
mod tts;
mod typer;
mod ui;
mod vad;

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use config::Config;
use engine::{Cmd, Engine};
use hotkey::HotkeyEvent;

#[derive(Parser)]
#[command(
    name = "ravenvoice",
    version,
    about = "Offline dictation and read-aloud for Raven Linux"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the overlay and dictation engine (the default).
    Run,
    /// Start or stop dictation in the running instance.
    Toggle,
    /// Start dictation.
    Start,
    /// Stop dictation.
    Stop,
    /// Read text aloud (from the arguments, or stdin if none are given).
    Speak { text: Vec<String> },
    /// Read the last dictated phrase aloud.
    SpeakLast,
    /// Stop reading aloud.
    StopSpeaking,
    /// Show the full overlay bar.
    Show,
    /// Shrink the overlay to just the microphone button.
    Hide,
    /// Print whether dictation is on, as JSON.
    Status,
    /// Quit the running instance.
    Quit,
    /// List microphones.
    Devices,
    /// List known Whisper models.
    Models,
    /// Download a Whisper model (default: the configured one).
    DownloadModel { name: Option<String> },
    /// Transcribe a WAV file and print the text (for testing).
    Transcribe { wav: PathBuf },
    /// Check that everything RavenVoice needs is in place.
    Doctor,
    /// Render the overlay to a PNG (for design work).
    #[command(hide = true)]
    Preview {
        out: PathBuf,
        /// idle, listening, hearing, speaking, error, compact or panel
        #[arg(long, default_value = "listening")]
        state: String,
    },
    /// Type a sample sentence into a test window and report what arrived.
    #[command(hide = true)]
    TestTyping {
        /// Instead of typing, show a text field for this many seconds and
        /// print what dictation types into it.
        #[arg(long)]
        watch: Option<u64>,
    },
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("ravenvoice=info,whisper_rs=warn"),
    )
    .init();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run(),
        Command::Toggle => remote("toggle", None),
        Command::Start => remote("start", None),
        Command::Stop => remote("stop", None),
        Command::SpeakLast => remote("speak-last", None),
        Command::StopSpeaking => remote("stop-speaking", None),
        Command::Show => remote("show", None),
        Command::Hide => remote("hide", None),
        Command::Status => remote("status", None),
        Command::Quit => remote("quit", None),
        Command::Speak { text } => speak(text),
        Command::Devices => devices(),
        Command::Models => {
            let cfg = Config::load();
            for (name, about) in model::KNOWN_MODELS {
                let have = if model::path_for(name).exists() {
                    "installed"
                } else {
                    ""
                };
                let current = if *name == cfg.stt.model { "*" } else { " " };
                println!("{current} {name:<16} {about:<40} {have}");
            }
            Ok(())
        }
        Command::DownloadModel { name } => {
            let name = name.unwrap_or_else(|| Config::load().stt.model);
            let dest = model::path_for(&name);
            model::download(&name, &dest, |f| eprint!("\r{name}: {:3.0}%", f * 100.0))?;
            eprintln!("\nsaved {}", dest.display());
            Ok(())
        }
        Command::Transcribe { wav } => transcribe(&wav),
        Command::Doctor => doctor(),
        Command::TestTyping { watch } => {
            find_wayland_display()?;
            ui::test_typing(Config::load(), watch);
            Ok(())
        }
        Command::Preview { out, state } => {
            find_wayland_display()?;
            ui::preview(Config::load(), out, state);
            Ok(())
        }
    }
}

/// Session services under `raven-init --user` start before the compositor
/// hands out WAYLAND_DISPLAY, and GTK would then guess `wayland-0`. Find the
/// compositor's socket the way ravencanvasd does, waiting briefly for it.
fn find_wayland_display() -> Result<()> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return Ok(());
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    for _ in 0..150 {
        let mut sockets: Vec<_> = std::fs::read_dir(&runtime)?
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with("wayland-")
                    && !name.ends_with(".lock")
                    && e.file_type().is_ok_and(|t| {
                        use std::os::unix::fs::FileTypeExt;
                        t.is_socket()
                    })
            })
            .map(|e| e.file_name())
            .collect();
        sockets.sort();
        if let Some(socket) = sockets.first() {
            log::info!("using Wayland display {}", socket.to_string_lossy());
            // SAFETY: called from main before any other thread is started.
            unsafe { std::env::set_var("WAYLAND_DISPLAY", socket) };
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    bail!(
        "no Wayland compositor socket in {}",
        runtime.to_string_lossy()
    )
}

fn run() -> Result<()> {
    find_wayland_display()?;
    if ipc::already_running() {
        println!("RavenVoice is already running; showing its overlay.");
        return remote("show", None);
    }
    let cfg = Config::load();
    let (ui_tx, ui_rx) = async_channel::unbounded();
    let engine = Engine::start(cfg.clone(), ui_tx.clone());
    ipc::serve(engine.clone())?;

    if cfg.hotkeys.enabled {
        let e = engine.clone();
        let started = hotkey::spawn(&cfg.hotkeys, engine.held.clone(), move |ev| {
            e.send(match ev {
                HotkeyEvent::Toggle => Cmd::Toggle,
                HotkeyEvent::PushToTalkDown => Cmd::PushToTalkDown,
                HotkeyEvent::PushToTalkUp => Cmd::PushToTalkUp,
                HotkeyEvent::SpeakLast => Cmd::SpeakLast,
                HotkeyEvent::StopSpeaking => Cmd::StopSpeaking,
            })
        });
        if let Err(err) = started {
            let _ = ui_tx.send_blocking(engine::UiEvent::Error(format!(
                "Shortcuts disabled: {err:#}"
            )));
        }
    }

    let code = ui::run(cfg, engine, ui_rx);
    let _ = std::fs::remove_file(ipc::socket_path());
    if code != gtk4::glib::ExitCode::SUCCESS {
        bail!("overlay exited with an error");
    }
    Ok(())
}

fn remote(verb: &str, arg: Option<&str>) -> Result<()> {
    let reply = ipc::request(verb, arg)?;
    if reply != "ok" {
        println!("{reply}");
    }
    Ok(())
}

fn speak(words: Vec<String>) -> Result<()> {
    let text = if words.is_empty() {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    } else {
        words.join(" ")
    };
    if ipc::already_running() {
        return remote("speak", Some(&text));
    }
    // No running instance: speak directly.
    tts::speak(&Config::load().tts, &text, &tts::Interrupt::default())
}

fn devices() -> Result<()> {
    let cfg = Config::load();
    let host = cpal::default_host();
    let mics = audio::list_inputs(&host);
    if mics.is_empty() {
        println!("No microphones found.");
    }
    for m in mics {
        let chosen = if cfg.audio.device.as_deref() == Some(m.id.as_str()) {
            "*"
        } else {
            " "
        };
        let default = if m.is_default {
            " [system default]"
        } else {
            ""
        };
        println!("{chosen} {}{default}\n    id: {}", m.display(), m.id);
    }
    println!(
        "\nTo pin one, set `device = \"<id>\"` under [audio] in {}",
        config::config_path().display()
    );
    Ok(())
}

fn transcribe(wav: &PathBuf) -> Result<()> {
    let cfg = Config::load();
    let mut reader =
        hound::WavReader::open(wav).with_context(|| format!("opening {}", wav.display()))?;
    let spec = reader.spec();
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|s| s as f32 / scale))
                .collect::<Result<_, _>>()?
        }
    };
    let mono: Vec<f32> = raw
        .chunks(spec.channels as usize)
        .map(|f| f.iter().sum::<f32>() / f.len() as f32)
        .collect();
    let mut audio16 = Vec::new();
    audio::Resampler::new(spec.sample_rate, audio::SAMPLE_RATE).process(&mono, &mut audio16);
    let mut stt = stt::Transcriber::load(&cfg.model_path(), &cfg.stt)?;
    let started = std::time::Instant::now();
    let text = stt.transcribe(&audio16, "", false)?;
    eprintln!(
        "({:.1}s of audio in {:.2}s)",
        audio16.len() as f32 / audio::SAMPLE_RATE as f32,
        started.elapsed().as_secs_f32()
    );
    println!("{text}");
    Ok(())
}

fn doctor() -> Result<()> {
    let cfg = Config::load();
    let ok = |good: bool| if good { "ok  " } else { "FIX " };
    println!("config:  {}", config::config_path().display());

    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    println!("[{}] Wayland session", ok(wayland));
    println!(
        "[{}] floating overlay (layer-shell){}",
        ok(cfg!(feature = "layer-shell")),
        if cfg!(feature = "layer-shell") {
            ""
        } else {
            " — built without the layer-shell feature"
        }
    );

    let uinput = std::fs::OpenOptions::new().write(true).open("/dev/uinput");
    println!(
        "[{}] typing into apps (/dev/uinput){}",
        ok(uinput.is_ok()),
        match &uinput {
            Ok(_) => String::new(),
            Err(e) => format!(" — {e}; run `imlazy setup` in the RavenVoice source folder"),
        }
    );

    let keyboards = evdev::enumerate()
        .filter(|(_, d)| {
            d.supported_keys()
                .is_some_and(|k| k.contains(evdev::KeyCode::KEY_A))
        })
        .count();
    println!(
        "[{}] global shortcuts: {keyboards} keyboard(s) readable{}",
        ok(keyboards > 0),
        if keyboards > 0 {
            ""
        } else {
            " — add yourself to the `input` group"
        }
    );

    let host = cpal::default_host();
    let mics = audio::list_inputs(&host);
    println!(
        "[{}] audio host {:?}, {} microphone(s)",
        ok(!mics.is_empty()),
        host.id(),
        mics.len()
    );
    for m in &mics {
        println!(
            "         - {}{}",
            m.display(),
            if m.is_default { " [default]" } else { "" }
        );
    }

    let model = cfg.model_path();
    println!(
        "[{}] Whisper model {}{}",
        ok(model.exists()),
        model.display(),
        if model.exists() {
            ""
        } else {
            " — `ravenvoice download-model` (or it downloads on first run)"
        }
    );
    let tts_name = tts::engine_name(&cfg.tts);
    println!(
        "[{}] read aloud: {tts_name}",
        ok(!tts_name.starts_with("unavailable"))
    );

    for (label, spec) in [
        ("toggle", &cfg.hotkeys.toggle),
        ("push to talk", &cfg.hotkeys.push_to_talk),
        ("speak last", &cfg.hotkeys.speak_last),
        ("stop speaking", &cfg.hotkeys.stop_speaking),
    ] {
        if let Err(e) = hotkey::parse(spec) {
            println!("[FIX ] {label} shortcut: {e}");
        }
    }
    Ok(())
}
