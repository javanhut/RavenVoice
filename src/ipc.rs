//! Control socket at `$XDG_RUNTIME_DIR/ravenvoice.sock`, so other programs,
//! scripts and compositor key bindings can drive a running instance:
//! `ravenvoice toggle`, `ravenvoice speak "hello"`, ...
//!
//! Protocol: the client writes one request and shuts down its write side;
//! the server replies with one line. A request is a verb, optionally
//! followed by a space and an argument (which may span lines).

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};

use crate::engine::{Cmd, Engine, UiEvent};

pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ravenvoice.sock")
}

/// Send a request to the running instance and return its reply.
pub fn request(verb: &str, arg: Option<&str>) -> Result<String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .with_context(|| "RavenVoice is not running (start it with `ravenvoice`)".to_string())?;
    let mut msg = verb.to_string();
    if let Some(arg) = arg {
        msg.push(' ');
        msg.push_str(arg);
    }
    stream.write_all(msg.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    let reply = reply.trim_end().to_string();
    if let Some(err) = reply.strip_prefix("error: ") {
        bail!("{err}");
    }
    Ok(reply)
}

/// True if another instance already owns the socket.
pub fn already_running() -> bool {
    UnixStream::connect(socket_path()).is_ok()
}

pub fn serve(engine: Engine) -> Result<()> {
    let path = socket_path();
    // A socket file nobody answers on is left over from a crash.
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    std::thread::Builder::new()
        .name("ipc".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                if let Err(e) = handle(stream, &engine) {
                    log::warn!("control socket: {e:#}");
                }
            }
        })?;
    Ok(())
}

fn handle(mut stream: UnixStream, engine: &Engine) -> Result<()> {
    let mut msg = String::new();
    (&stream).take(1 << 20).read_to_string(&mut msg)?;
    if msg.is_empty() {
        // `already_running()` probes by connecting and hanging up.
        return Ok(());
    }
    let (verb, arg) = match msg.split_once(' ') {
        Some((v, a)) => (v.trim(), a.to_string()),
        None => (msg.trim(), String::new()),
    };
    let reply = match verb {
        "toggle" => ok(engine, Cmd::Toggle),
        "start" => ok(engine, Cmd::Start),
        "stop" => ok(engine, Cmd::Stop),
        "speak" => ok(engine, Cmd::Speak(arg)),
        "speak-last" => ok(engine, Cmd::SpeakLast),
        "stop-speaking" => ok(engine, Cmd::StopSpeaking),
        "show" | "hide" | "toggle-overlay" => {
            let visible = match verb {
                "show" => Some(true),
                "hide" => Some(false),
                _ => None,
            };
            let _ = engine.ui.send_blocking(UiEvent::SetVisible(visible));
            "ok".to_string()
        }
        "status" => {
            let s = &engine.shared;
            serde_json::json!({
                "listening": s.listening.load(Ordering::SeqCst),
                "speaking": s.speaking.load(Ordering::SeqCst),
                "last_text": *s.last_text.lock().unwrap(),
            })
            .to_string()
        }
        "quit" => {
            stream.write_all(b"ok\n")?;
            std::process::exit(0);
        }
        other => format!("error: unknown request {other:?}"),
    };
    stream.write_all(reply.as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(())
}

fn ok(engine: &Engine, cmd: Cmd) -> String {
    engine.send(cmd);
    "ok".to_string()
}
