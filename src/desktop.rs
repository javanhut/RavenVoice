//! The slice of `~/.config/raven/desktop.toml` RavenVoice follows: theme
//! mode, accent and window transparency. Raven Settings owns the file; every
//! key is optional and a parse error means the defaults, so a newer Settings
//! never breaks an older RavenVoice.

use std::path::PathBuf;

use serde::Deserialize;

pub const DEFAULT_ACCENT: &str = "#7AA2F7";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    #[default]
    Dark,
    Auto,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme_mode: ThemeMode,
    pub accent: String,
    pub transparency: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme_mode: ThemeMode::Dark,
            accent: DEFAULT_ACCENT.into(),
            transparency: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Desktop {
    pub appearance: Appearance,
}

impl Desktop {
    pub fn load() -> Desktop {
        std::fs::read_to_string(path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// The accent as `#RRGGBB`, or the default when the file's is not one.
    pub fn accent(&self) -> &str {
        if is_hex(&self.appearance.accent) {
            &self.appearance.accent
        } else {
            DEFAULT_ACCENT
        }
    }
}

pub fn is_hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Where Settings writes the file. It may not exist yet, and Settings
/// replaces it by rename, so a watcher follows the directory, not the file.
pub fn path() -> PathBuf {
    config_dir().join("desktop.toml")
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("raven")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_accent_falls_back() {
        let d: Desktop = toml::from_str("[appearance]\naccent = \"red\"\n").unwrap();
        assert_eq!(d.accent(), DEFAULT_ACCENT);
        let d: Desktop = toml::from_str(
            "[appearance]\naccent = \"#F7768E\"\ntheme_mode = \"light\"\ntransparency = false\n",
        )
        .unwrap();
        assert_eq!(d.accent(), "#F7768E");
        assert_eq!(d.appearance.theme_mode, ThemeMode::Light);
        assert!(!d.appearance.transparency);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let d: Desktop = toml::from_str("[privacy]\nx = 1\n[appearance]\nblur = true\n").unwrap();
        assert_eq!(d.appearance.theme_mode, ThemeMode::Dark);
    }
}
