//! Global shortcuts, read straight from the keyboards.
//!
//! Huginn has no global-shortcut protocol, but the Raven session user is in
//! the `input` group, so keyboards can be watched (never grabbed: keys still
//! reach the focused app as usual). New keyboards are picked up as they are
//! plugged in.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use evdev::{EventSummary, KeyCode};

use crate::config::HotkeyConfig;
use crate::typer::{DEVICE_NAME, HeldModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: Mods,
    pub key: KeyCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Toggle,
    PushToTalkDown,
    PushToTalkUp,
    SpeakLast,
    StopSpeaking,
}

fn modifier_of(code: KeyCode) -> Option<fn(&mut Mods)> {
    Some(match code {
        KeyCode::KEY_LEFTCTRL | KeyCode::KEY_RIGHTCTRL => |m| m.ctrl = true,
        KeyCode::KEY_LEFTALT | KeyCode::KEY_RIGHTALT => |m| m.alt = true,
        KeyCode::KEY_LEFTSHIFT | KeyCode::KEY_RIGHTSHIFT => |m| m.shift = true,
        KeyCode::KEY_LEFTMETA | KeyCode::KEY_RIGHTMETA => |m| m.logo = true,
        _ => return None,
    })
}

/// Parse "Ctrl+Alt+D", "Super+Space", "F9", "RightCtrl". Empty means unset.
pub fn parse(spec: &str) -> Result<Option<Hotkey>> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(None);
    }
    let mut mods = Mods::default();
    let parts: Vec<&str> = spec.split('+').map(str::trim).collect();
    let (key_name, mod_names) = parts.split_last().ok_or_else(|| anyhow!("empty hotkey"))?;
    for m in mod_names {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods.ctrl = true,
            "alt" => mods.alt = true,
            "shift" => mods.shift = true,
            "super" | "logo" | "meta" | "win" => mods.logo = true,
            other => bail!("unknown modifier {other:?} in hotkey {spec:?}"),
        }
    }
    let lower = key_name.to_ascii_lowercase();
    let evdev_name = match lower.as_str() {
        "esc" | "escape" => "KEY_ESC".to_string(),
        "return" => "KEY_ENTER".to_string(),
        "del" => "KEY_DELETE".to_string(),
        "pgup" => "KEY_PAGEUP".to_string(),
        "pgdn" => "KEY_PAGEDOWN".to_string(),
        "menu" => "KEY_COMPOSE".to_string(),
        "rightctrl" | "rctrl" => "KEY_RIGHTCTRL".to_string(),
        "rightalt" | "altgr" => "KEY_RIGHTALT".to_string(),
        "rightshift" => "KEY_RIGHTSHIFT".to_string(),
        "rightsuper" => "KEY_RIGHTMETA".to_string(),
        s if s.starts_with("key_") => s.to_ascii_uppercase(),
        s => format!("KEY_{}", s.to_ascii_uppercase()),
    };
    let key: KeyCode = evdev_name
        .parse()
        .map_err(|_| anyhow!("unknown key {key_name:?} in hotkey {spec:?}"))?;
    Ok(Some(Hotkey { mods, key }))
}

struct Bindings {
    toggle: Option<Hotkey>,
    ptt: Option<Hotkey>,
    speak_last: Option<Hotkey>,
    stop_speaking: Option<Hotkey>,
}

impl Bindings {
    fn from_config(cfg: &HotkeyConfig) -> Result<Self> {
        Ok(Self {
            toggle: parse(&cfg.toggle)?,
            ptt: parse(&cfg.push_to_talk)?,
            speak_last: parse(&cfg.speak_last)?,
            stop_speaking: parse(&cfg.stop_speaking)?,
        })
    }

    fn keys(&self) -> impl Iterator<Item = KeyCode> + '_ {
        [self.toggle, self.ptt, self.speak_last, self.stop_speaking]
            .into_iter()
            .flatten()
            .map(|h| h.key)
    }
}

/// Current modifiers across every keyboard, not counting `except` itself
/// (so a bare "RightCtrl" push-to-talk key matches with no modifiers).
fn current_mods(held: &HeldModifiers, except: KeyCode) -> Mods {
    let mut mods = Mods::default();
    for code in held.codes() {
        if code != except.0
            && let Some(set) = modifier_of(KeyCode::new(code))
        {
            set(&mut mods);
        }
    }
    mods
}

/// Start watching keyboards. Returns immediately; `on_event` is called from
/// background threads.
pub fn spawn(
    cfg: &HotkeyConfig,
    held: Arc<HeldModifiers>,
    on_event: impl Fn(HotkeyEvent) + Send + Sync + 'static,
) -> Result<()> {
    let bindings = Arc::new(Bindings::from_config(cfg)?);
    let on_event = Arc::new(on_event);
    let open: Arc<Mutex<HashSet<PathBuf>>> = Default::default();

    std::thread::Builder::new()
        .name("hotkey-scan".into())
        .spawn(move || {
            let mut next_index = 0usize;
            let mut warned = false;
            loop {
                let mut found_any = false;
                for (path, device) in evdev::enumerate() {
                    let Some(keys) = device.supported_keys() else { continue };
                    let is_keyboard = keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_ENTER);
                    let has_binding = bindings.keys().any(|k| keys.contains(k));
                    if device.name() == Some(DEVICE_NAME) || !(is_keyboard || has_binding) {
                        continue;
                    }
                    found_any = true;
                    if !open.lock().unwrap().insert(path.clone()) {
                        continue;
                    }
                    let index = next_index;
                    next_index += 1;
                    log::info!("watching {} ({}) for shortcuts", device.name().unwrap_or("keyboard"), path.display());
                    let (bindings, on_event, held, open) =
                        (bindings.clone(), on_event.clone(), held.clone(), open.clone());
                    std::thread::spawn(move || {
                        watch(device, index, &bindings, &*on_event, &held);
                        held.forget_device(index);
                        open.lock().unwrap().remove(&path);
                    });
                }
                if !found_any && !warned {
                    warned = true;
                    log::warn!(
                        "no keyboards readable in /dev/input; global shortcuts need the `input` group"
                    );
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        })?;
    Ok(())
}

fn watch(
    mut device: evdev::Device,
    index: usize,
    b: &Bindings,
    on_event: &(dyn Fn(HotkeyEvent) + Send + Sync),
    held: &HeldModifiers,
) {
    loop {
        let events = match device.fetch_events() {
            Ok(events) => events,
            Err(e) => {
                log::info!("keyboard {index} went away: {e}");
                return;
            }
        };
        for ev in events {
            let EventSummary::Key(_, code, value) = ev.destructure() else {
                continue;
            };
            // value: 1 press, 0 release, 2 autorepeat (ignored).
            if value == 2 {
                continue;
            }
            let down = value == 1;
            if modifier_of(code).is_some() {
                held.set(index, code.0, down);
            }
            let matches = |h: Option<Hotkey>| {
                h.is_some_and(|h| h.key == code && h.mods == current_mods(held, code))
            };
            if down {
                if matches(b.toggle) {
                    on_event(HotkeyEvent::Toggle);
                } else if matches(b.ptt) {
                    on_event(HotkeyEvent::PushToTalkDown);
                } else if matches(b.speak_last) {
                    on_event(HotkeyEvent::SpeakLast);
                } else if matches(b.stop_speaking) {
                    on_event(HotkeyEvent::StopSpeaking);
                }
            } else if b.ptt.is_some_and(|h| h.key == code) {
                // Release of the talk key ends push-to-talk whatever the modifiers now are.
                on_event(HotkeyEvent::PushToTalkUp);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_specs() {
        let h = parse("Ctrl+Alt+D").unwrap().unwrap();
        assert_eq!(h.key, KeyCode::KEY_D);
        assert!(h.mods.ctrl && h.mods.alt && !h.mods.shift && !h.mods.logo);
        assert_eq!(parse("F9").unwrap().unwrap().key, KeyCode::KEY_F9);
        assert_eq!(
            parse("super+space").unwrap().unwrap().key,
            KeyCode::KEY_SPACE
        );
        assert_eq!(
            parse("RightCtrl").unwrap().unwrap().key,
            KeyCode::KEY_RIGHTCTRL
        );
        assert!(parse("").unwrap().is_none());
        assert!(parse("Hyper+Q").is_err());
        assert!(parse("Ctrl+Nope").is_err());
    }
}
