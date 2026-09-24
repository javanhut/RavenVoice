//! Spoken editing commands.
//!
//! A phrase that is *only* a command ("scratch that", "select all") runs the
//! command. "new line" and "new paragraph" also work in the middle of a
//! sentence, since that is how people naturally dictate them.

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Text(String),
    Key(KeyCombo),
    /// Erase the previously typed phrase.
    DeleteLast,
    StopListening,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NamedKey {
    Enter,
    Tab,
    Escape,
    Backspace,
    Char(char),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyCombo {
    pub ctrl: bool,
    pub shift: bool,
    pub key: NamedKey,
}

impl KeyCombo {
    const fn plain(key: NamedKey) -> Self {
        Self {
            ctrl: false,
            shift: false,
            key,
        }
    }
    const fn ctrl(c: char) -> Self {
        Self {
            ctrl: true,
            shift: false,
            key: NamedKey::Char(c),
        }
    }
}

/// Lowercase, letters and digits only: "Scratch that." -> "scratch that".
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn whole_phrase_command(norm: &str) -> Option<Action> {
    use NamedKey::*;
    let key = |k| Some(Action::Key(KeyCombo::plain(k)));
    match norm {
        "scratch that" | "delete that" | "erase that" => Some(Action::DeleteLast),
        "stop listening" | "stop dictation" | "stop dictating" => Some(Action::StopListening),
        "press enter" | "press return" => key(Enter),
        "press tab" => key(Tab),
        "press escape" => key(Escape),
        "backspace" | "press backspace" => key(Backspace),
        "undo" | "undo that" => Some(Action::Key(KeyCombo::ctrl('z'))),
        "redo" | "redo that" => Some(Action::Key(KeyCombo {
            ctrl: true,
            shift: true,
            key: Char('z'),
        })),
        "select all" => Some(Action::Key(KeyCombo::ctrl('a'))),
        "copy that" => Some(Action::Key(KeyCombo::ctrl('c'))),
        "cut that" => Some(Action::Key(KeyCombo::ctrl('x'))),
        "paste" | "paste that" => Some(Action::Key(KeyCombo::ctrl('v'))),
        _ => None,
    }
}

/// Turn a recognised phrase into things to type or do.
pub fn interpret(text: &str, commands_enabled: bool) -> Vec<Action> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    if !commands_enabled {
        return vec![Action::Text(text.to_string())];
    }
    if let Some(cmd) = whole_phrase_command(&normalize(text)) {
        return vec![cmd];
    }

    let words: Vec<&str> = text.split_whitespace().collect();
    let mut actions = Vec::new();
    let mut buf: Vec<&str> = Vec::new();
    let flush = |buf: &mut Vec<&str>, actions: &mut Vec<Action>| {
        if buf.is_empty() {
            return;
        }
        let joined = buf.join(" ");
        // "Dear Sam, new line" -> drop the comma Whisper put before the command.
        let joined = joined.trim_end_matches([',', ';']).to_string();
        if !joined.is_empty() {
            actions.push(Action::Text(joined));
        }
        buf.clear();
    };

    let mut i = 0;
    while i < words.len() {
        let pair = words
            .get(i + 1)
            .map(|next| normalize(&format!("{} {}", words[i], next)));
        let enters = match pair.as_deref() {
            Some("new line") => 1,
            Some("new paragraph") => 2,
            _ => 0,
        };
        if enters > 0 {
            flush(&mut buf, &mut actions);
            for _ in 0..enters {
                actions.push(Action::Key(KeyCombo::plain(NamedKey::Enter)));
            }
            i += 2;
        } else {
            buf.push(words[i]);
            i += 1;
        }
    }
    flush(&mut buf, &mut actions);
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enter() -> Action {
        Action::Key(KeyCombo::plain(NamedKey::Enter))
    }

    #[test]
    fn whole_phrase_commands() {
        assert_eq!(interpret("Scratch that.", true), vec![Action::DeleteLast]);
        assert_eq!(
            interpret(" Stop listening!", true),
            vec![Action::StopListening]
        );
        assert_eq!(
            interpret("Select all.", true),
            vec![Action::Key(KeyCombo::ctrl('a'))]
        );
    }

    #[test]
    fn inline_new_line() {
        assert_eq!(
            interpret("Dear Sam, new line. Thanks for the notes.", true),
            vec![
                Action::Text("Dear Sam".into()),
                enter(),
                Action::Text("Thanks for the notes.".into())
            ]
        );
        assert_eq!(
            interpret("End of section. New paragraph.", true),
            vec![Action::Text("End of section.".into()), enter(), enter()]
        );
    }

    #[test]
    fn plain_text_and_disabled() {
        assert_eq!(
            interpret("Hello world.", true),
            vec![Action::Text("Hello world.".into())]
        );
        assert_eq!(
            interpret("Scratch that.", false),
            vec![Action::Text("Scratch that.".into())]
        );
    }
}
