//! Types text into whichever app has focus through a virtual keyboard
//! (/dev/uinput).
//!
//! Huginn offers no virtual-keyboard or input-method protocol, so a kernel
//! virtual keyboard is the one route that reaches every app: native Wayland,
//! XWayland and terminals alike. The compositor applies its own keymap to our
//! key codes, so text is translated for a US layout.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};

use crate::commands::{Action, KeyCombo, NamedKey};
use crate::config::TypingConfig;

pub const DEVICE_NAME: &str = "RavenVoice virtual keyboard";

/// Modifier keys physically held right now (device index, key code), kept up
/// to date by the hotkey listener. We must not type while the user still
/// holds Ctrl from a shortcut, or "hello" would become Ctrl+H, Ctrl+E, ...
#[derive(Default)]
pub struct HeldModifiers(Mutex<HashSet<(usize, u16)>>);

impl HeldModifiers {
    pub fn set(&self, dev: usize, code: u16, down: bool) {
        let mut held = self.0.lock().unwrap();
        if down {
            held.insert((dev, code));
        } else {
            held.remove(&(dev, code));
        }
    }
    pub fn forget_device(&self, dev: usize) {
        self.0.lock().unwrap().retain(|(d, _)| *d != dev);
    }
    pub fn codes(&self) -> Vec<u16> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|&(_, code)| code)
            .collect()
    }
    pub fn any(&self) -> bool {
        !self.0.lock().unwrap().is_empty()
    }
}

pub struct Typer {
    dev: VirtualDevice,
    delay: Duration,
    unicode_fallback: bool,
    held: Arc<HeldModifiers>,
    /// Keystrokes that made up the last phrase, for "scratch that".
    last_len: usize,
    last_char: Option<char>,
    /// Words typed while the current phrase is still being spoken.
    live: Option<LivePhrase>,
}

struct LivePhrase {
    id: u64,
    /// Exactly what has been typed for this phrase (without the joining space).
    text: String,
    keystrokes: usize,
    /// What preceded the phrase, to restore spacing if it is retyped.
    char_before: Option<char>,
}

impl Typer {
    pub fn new(cfg: &TypingConfig, held: Arc<HeldModifiers>) -> Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        // KEY_ESC (1) through KEY_MICMUTE (248): every ordinary keyboard key.
        for code in 1..=248u16 {
            keys.insert(KeyCode::new(code));
        }
        let dev = VirtualDevice::builder()
            .and_then(|b| b.name(DEVICE_NAME).with_keys(&keys))
            .and_then(|b| b.build())
            .context(
                "cannot open /dev/uinput to type text. Run `imlazy setup` in the RavenVoice source \
                 folder once (it lets the `input` group use the virtual keyboard)",
            )?;
        // Give the compositor a moment to notice the new keyboard.
        std::thread::sleep(Duration::from_millis(400));
        Ok(Self {
            dev,
            delay: Duration::from_millis(cfg.key_delay_ms),
            unicode_fallback: cfg.unicode_fallback == "ctrl-shift-u",
            held,
            last_len: 0,
            last_char: None,
            live: None,
        })
    }

    /// Forget spacing context, e.g. when dictation restarts in another field.
    pub fn reset(&mut self) {
        self.last_char = None;
        self.last_len = 0;
        self.live = None;
    }

    /// Type the words of phrase `id` that have settled so far. `settled`
    /// is everything settled for the phrase; only what is new gets typed.
    pub fn stream(&mut self, id: u64, settled: &str) -> Result<()> {
        let settled = normalize_punctuation(settled);
        if self.live.as_ref().is_none_or(|l| l.id != id) {
            self.live = Some(LivePhrase {
                id,
                text: String::new(),
                keystrokes: 0,
                char_before: self.last_char,
            });
        }
        let live = self.live.as_ref().expect("set above");
        // If Whisper revised earlier words, leave them for the final pass.
        let Some(delta) = settled.strip_prefix(live.text.as_str()) else {
            return Ok(());
        };
        if delta.is_empty() {
            return Ok(());
        }
        let delta = delta.to_string();
        self.wait_for_modifiers_released();
        let typed = self.type_text(&delta)?;
        let live = self.live.as_mut().expect("set above");
        live.keystrokes += typed;
        live.text = settled;
        Ok(())
    }

    /// Phrase `id` is complete: add whatever the final transcript has beyond
    /// the words typed live, or take those back and type it afresh if
    /// Whisper changed its mind.
    pub fn finish(&mut self, id: u64, actions: &[Action]) -> Result<()> {
        let live = self.live.take().filter(|l| l.id == id && l.keystrokes > 0);
        let Some(live) = live else {
            return self.perform(actions);
        };
        self.wait_for_modifiers_released();
        if let [Action::Text(first), rest @ ..] = actions {
            let first = normalize_punctuation(first);
            // Keep what the live words and the final transcript share and
            // correct only from the first difference ("wrld" -> "world"
            // costs three backspaces, not the whole phrase).
            let common = live
                .text
                .chars()
                .zip(first.chars())
                .take_while(|(a, b)| a == b)
                .count();
            if common > 0 {
                let erase = live
                    .text
                    .chars()
                    .skip(common)
                    .filter(|&c| key_for(c).is_some() || self.unicode_fallback)
                    .count();
                if erase > 0 {
                    log::debug!("correcting {erase} characters of live words");
                }
                self.backspace(erase)?;
                self.last_char = live.text.chars().nth(common - 1);
                let tail: String = first.chars().skip(common).collect();
                let mut typed = live.keystrokes - erase + self.type_raw(&tail)?;
                typed += self.run(rest)?;
                self.last_len = typed;
                return Ok(());
            }
        }
        log::debug!("final transcript differs from live words; retyping the phrase");
        self.backspace(live.keystrokes)?;
        self.last_char = live.char_before;
        self.perform(actions)
    }

    fn backspace(&mut self, times: usize) -> Result<()> {
        for _ in 0..times {
            self.tap(KeyCombo {
                ctrl: false,
                shift: false,
                key: NamedKey::Backspace,
            })?;
        }
        Ok(())
    }

    pub fn perform(&mut self, actions: &[Action]) -> Result<()> {
        self.wait_for_modifiers_released();
        if actions == [Action::DeleteLast] {
            self.backspace(self.last_len)?;
            self.last_len = 0;
            self.last_char = None;
            return Ok(());
        }
        let typed = self.run(actions)?;
        if typed > 0 {
            self.last_len = typed;
        }
        Ok(())
    }

    /// Carry out `actions`, returning the keystrokes that produced characters.
    fn run(&mut self, actions: &[Action]) -> Result<usize> {
        let mut typed = 0;
        for action in actions {
            match action {
                Action::Text(text) => typed += self.type_text(text)?,
                Action::Key(combo) => {
                    self.tap(*combo)?;
                    match combo.key {
                        NamedKey::Enter if !combo.ctrl => {
                            typed += 1;
                            self.last_char = Some('\n');
                        }
                        NamedKey::Tab if !combo.ctrl => {
                            typed += 1;
                            self.last_char = Some('\t');
                        }
                        _ => {}
                    }
                }
                Action::DeleteLast | Action::StopListening => {}
            }
        }
        Ok(typed)
    }

    fn wait_for_modifiers_released(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.held.any() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Type `text`, adding a joining space after earlier dictation. Returns
    /// the number of keystrokes that produced visible characters.
    fn type_text(&mut self, text: &str) -> Result<usize> {
        let text = normalize_punctuation(text);
        let needs_space = match (self.last_char, text.chars().next()) {
            (Some(prev), Some(first)) => {
                !prev.is_whitespace()
                    && !first.is_whitespace()
                    && !matches!(first, '.' | ',' | '!' | '?' | ';' | ':' | ')')
            }
            _ => false,
        };
        let text: String = needs_space
            .then_some(' ')
            .into_iter()
            .chain(text.chars())
            .collect();
        self.type_raw(&text)
    }

    /// Type `text` exactly, with no joining space.
    fn type_raw(&mut self, text: &str) -> Result<usize> {
        let mut count = 0;
        for c in text.chars() {
            if let Some((code, shift)) = key_for(c) {
                self.press(code, shift, false)?;
                count += 1;
            } else if self.unicode_fallback {
                self.unicode_entry(c)?;
                count += 1;
            } else {
                log::warn!("no key for {c:?} on a US layout; skipped");
            }
            self.last_char = Some(c);
        }
        Ok(count)
    }

    fn tap(&mut self, combo: KeyCombo) -> Result<()> {
        let (code, shift) = match combo.key {
            NamedKey::Enter => (KeyCode::KEY_ENTER, false),
            NamedKey::Tab => (KeyCode::KEY_TAB, false),
            NamedKey::Escape => (KeyCode::KEY_ESC, false),
            NamedKey::Backspace => (KeyCode::KEY_BACKSPACE, false),
            NamedKey::Char(c) => key_for(c).context("unmappable shortcut key")?,
        };
        self.press(code, shift || combo.shift, combo.ctrl)
    }

    fn press(&mut self, code: KeyCode, shift: bool, ctrl: bool) -> Result<()> {
        let ev = |k: KeyCode, v: i32| InputEvent::new(EventType::KEY.0, k.0, v);
        if ctrl {
            self.dev.emit(&[ev(KeyCode::KEY_LEFTCTRL, 1)])?;
        }
        if shift {
            self.dev.emit(&[ev(KeyCode::KEY_LEFTSHIFT, 1)])?;
        }
        self.dev.emit(&[ev(code, 1)])?;
        self.dev.emit(&[ev(code, 0)])?;
        if shift {
            self.dev.emit(&[ev(KeyCode::KEY_LEFTSHIFT, 0)])?;
        }
        if ctrl {
            self.dev.emit(&[ev(KeyCode::KEY_LEFTCTRL, 0)])?;
        }
        std::thread::sleep(self.delay);
        Ok(())
    }

    /// GTK's Ctrl+Shift+U <hex> <space> unicode entry.
    fn unicode_entry(&mut self, c: char) -> Result<()> {
        self.tap(KeyCombo {
            ctrl: true,
            shift: true,
            key: NamedKey::Char('u'),
        })?;
        for h in format!("{:x}", c as u32).chars() {
            let (code, _) = key_for(h).expect("hex digits are mappable");
            self.press(code, false, false)?;
        }
        self.press(KeyCode::KEY_SPACE, false, false)
    }
}

/// Swap typographic characters Whisper likes for ones on the keyboard.
fn normalize_punctuation(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{2018}' | '\u{2019}' | '\u{201B}' | '\u{2032}' => out.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' => out.push('"'),
            '\u{2013}' | '\u{2014}' | '\u{2212}' => out.push('-'),
            '\u{2026}' => out.push_str("..."),
            '\u{00A0}' | '\u{2009}' | '\u{202F}' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// US-layout key and whether Shift is needed.
pub fn key_for(c: char) -> Option<(KeyCode, bool)> {
    use KeyCode as K;
    const LETTERS: [KeyCode; 26] = [
        K::KEY_A,
        K::KEY_B,
        K::KEY_C,
        K::KEY_D,
        K::KEY_E,
        K::KEY_F,
        K::KEY_G,
        K::KEY_H,
        K::KEY_I,
        K::KEY_J,
        K::KEY_K,
        K::KEY_L,
        K::KEY_M,
        K::KEY_N,
        K::KEY_O,
        K::KEY_P,
        K::KEY_Q,
        K::KEY_R,
        K::KEY_S,
        K::KEY_T,
        K::KEY_U,
        K::KEY_V,
        K::KEY_W,
        K::KEY_X,
        K::KEY_Y,
        K::KEY_Z,
    ];
    const DIGITS: [KeyCode; 10] = [
        K::KEY_0,
        K::KEY_1,
        K::KEY_2,
        K::KEY_3,
        K::KEY_4,
        K::KEY_5,
        K::KEY_6,
        K::KEY_7,
        K::KEY_8,
        K::KEY_9,
    ];
    Some(match c {
        'a'..='z' => (LETTERS[c as usize - 'a' as usize], false),
        'A'..='Z' => (LETTERS[c as usize - 'A' as usize], true),
        '0'..='9' => (DIGITS[c as usize - '0' as usize], false),
        ' ' => (K::KEY_SPACE, false),
        '\n' => (K::KEY_ENTER, false),
        '\t' => (K::KEY_TAB, false),
        '!' => (K::KEY_1, true),
        '@' => (K::KEY_2, true),
        '#' => (K::KEY_3, true),
        '$' => (K::KEY_4, true),
        '%' => (K::KEY_5, true),
        '^' => (K::KEY_6, true),
        '&' => (K::KEY_7, true),
        '*' => (K::KEY_8, true),
        '(' => (K::KEY_9, true),
        ')' => (K::KEY_0, true),
        '-' => (K::KEY_MINUS, false),
        '_' => (K::KEY_MINUS, true),
        '=' => (K::KEY_EQUAL, false),
        '+' => (K::KEY_EQUAL, true),
        '[' => (K::KEY_LEFTBRACE, false),
        '{' => (K::KEY_LEFTBRACE, true),
        ']' => (K::KEY_RIGHTBRACE, false),
        '}' => (K::KEY_RIGHTBRACE, true),
        '\\' => (K::KEY_BACKSLASH, false),
        '|' => (K::KEY_BACKSLASH, true),
        ';' => (K::KEY_SEMICOLON, false),
        ':' => (K::KEY_SEMICOLON, true),
        '\'' => (K::KEY_APOSTROPHE, false),
        '"' => (K::KEY_APOSTROPHE, true),
        ',' => (K::KEY_COMMA, false),
        '<' => (K::KEY_COMMA, true),
        '.' => (K::KEY_DOT, false),
        '>' => (K::KEY_DOT, true),
        '/' => (K::KEY_SLASH, false),
        '?' => (K::KEY_SLASH, true),
        '`' => (K::KEY_GRAVE, false),
        '~' => (K::KEY_GRAVE, true),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_ascii_printables() {
        for c in (' '..='~').chain(['\n', '\t']) {
            assert!(key_for(c).is_some(), "{c:?} has no key");
        }
        assert_eq!(key_for('Q'), Some((KeyCode::KEY_Q, true)));
        assert_eq!(key_for('é'), None);
    }

    #[test]
    fn normalizes_smart_punctuation() {
        assert_eq!(
            normalize_punctuation("It\u{2019}s \u{201C}ok\u{201D}\u{2026}"),
            "It's \"ok\"..."
        );
    }
}
