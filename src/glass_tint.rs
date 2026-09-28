//! The glass theme, `appearance.glass_theme` in `~/.config/raven/desktop.toml`,
//! for an app's own windows: the same Black, Fog, Arctic, Midnight, Rose,
//! Tokyo Neon, Clear, Ember and Nebula glass the compositor draws its panels in, so a window matches the dock,
//! the launcher and its own title bar.
//!
//! Raven Glass (`raven-glass.css`, and `raven-glass-light.css` over it) is
//! Black Glass. Every other theme is the same stylesheet with its grounds and
//! text re-tinted: [`css`] answers the `@define-color`s and the glass window
//! backgrounds to lay over it, one provider priority above it. The washes,
//! hairlines and catch-lights stay as they are — white on dark glass, a
//! whisper of black on light — which is what the compositor does too.
//!
//! The grounds and text are the compositor's (RavenGUI,
//! `huginn-comp/src/theme.rs`). Keep every app's copy of this file identical.

/// (value, dark ground, dark text, light ground, light text).
const THEMES: [(&str, &str, &str, &str, &str); 8] = [
    ("fog", "#6E7D94", "#FFFFFF", "#E5EAF1", "#1C2433"),
    ("arctic", "#4F7F9F", "#FFFFFF", "#DDEEF8", "#0E2A3C"),
    ("midnight", "#0E1630", "#E8EEFF", "#E3E7F6", "#101A3A"),
    ("rose", "#5A3A4E", "#FFF4F8", "#F6E7EE", "#3A1E2C"),
    ("tokyo-neon", "#0A1A14", "#E6FFF1", "#E2F6EA", "#06291A"),
    ("clear", "#1C1F26", "#FFFFFF", "#F7F9FC", "#0F1218"),
    ("ember", "#3A2418", "#FFF3E8", "#F8EBDF", "#3A1F10"),
    ("nebula", "#24163A", "#F3EAFF", "#EEE6FA", "#22123C"),
];

/// Clear Glass is the see-through one: its windows let the desktop through
/// as its panels do, where every other glass window is near-opaque.
const CLEAR: &str = "clear";

/// The CSS that turns Raven Glass into `theme`, in light or dark. Empty for
/// Black Glass and for a value this build does not know, which the
/// compositor also draws as black.
pub fn css(theme: &str, light: bool) -> String {
    let want = squash(theme);
    let want = want.strip_suffix("glass").unwrap_or(&want);
    let Some(&(value, dark_bg, dark_fg, light_bg, light_fg)) =
        THEMES.iter().find(|t| squash(t.0) == want)
    else {
        return String::new();
    };
    let (Some(bg), Some(fg)) = (
        rgb(if light { light_bg } else { dark_bg }),
        rgb(if light { light_fg } else { dark_fg }),
    ) else {
        return String::new();
    };
    const WHITE: [u8; 3] = [255, 255, 255];
    const BLACK: [u8; 3] = [0, 0, 0];
    // Raised surfaces are a shade toward the light on dark glass and toward
    // white on light glass; the sidebar is a shade toward the shadow.
    let (view, dialog, popover, sidebar, glass_alpha) = if light {
        (
            mix(bg, WHITE, 0.55),
            mix(bg, WHITE, 0.35),
            mix(bg, WHITE, 0.70),
            mix(bg, fg, 0.05),
            if value == CLEAR { 0.62 } else { 0.88 },
        )
    } else {
        (
            mix(bg, WHITE, 0.04),
            mix(bg, WHITE, 0.08),
            mix(bg, WHITE, 0.11),
            mix(bg, BLACK, 0.22),
            if value == CLEAR { 0.55 } else { 0.85 },
        )
    };
    let (bg, fg) = (hex(bg), hex(fg));
    let (view, dialog, popover, sidebar) = (hex(view), hex(dialog), hex(popover), hex(sidebar));
    let sidebar_glass = if light {
        "alpha(#ffffff, 0.30)".to_owned()
    } else {
        format!("alpha({sidebar}, 0.40)")
    };
    format!(
        "@define-color window_bg_color {bg};\n\
         @define-color window_fg_color {fg};\n\
         @define-color headerbar_bg_color {bg};\n\
         @define-color headerbar_fg_color {fg};\n\
         @define-color view_bg_color {view};\n\
         @define-color view_fg_color {fg};\n\
         @define-color card_fg_color {fg};\n\
         @define-color dialog_bg_color {dialog};\n\
         @define-color dialog_fg_color {fg};\n\
         @define-color popover_bg_color {popover};\n\
         @define-color popover_fg_color {fg};\n\
         @define-color sidebar_bg_color {sidebar};\n\
         @define-color sidebar_fg_color {fg};\n\
         @define-color sidebar_backdrop_color {sidebar};\n\
         window.raven.glass {{ background-color: alpha({bg}, {glass_alpha}); }}\n\
         window.raven.glass .sidebar {{ background-color: {sidebar_glass}; }}\n"
    )
}

fn squash(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase()
}

fn rgb(hex: &str) -> Option<[u8; 3]> {
    let h = hex.strip_prefix('#')?;
    let at = |i: usize| h.get(i..i + 2).and_then(|c| u8::from_str_radix(c, 16).ok());
    Some([at(0)?, at(2)?, at(4)?])
}

fn mix(a: [u8; 3], b: [u8; 3], t: f64) -> [u8; 3] {
    let m = |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

fn hex(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

#[cfg(test)]
mod glass_tint_tests {
    use super::*;

    #[test]
    fn black_and_unknown_themes_leave_raven_glass_alone() {
        assert_eq!(css("black", false), "");
        assert_eq!(css("", true), "");
        assert_eq!(css("sepia", false), "");
    }

    #[test]
    fn every_spelling_of_a_theme_tints() {
        for v in ["rose", "Rose Glass", "rose-glass"] {
            let c = css(v, false);
            assert!(
                c.contains("@define-color window_bg_color #5a3a4e;"),
                "{v}: {c}"
            );
        }
        let c = css("fog", true);
        assert!(c.contains("@define-color window_fg_color #1c2433;"), "{c}");
        assert!(c.contains("window.raven.glass {"), "{c}");
        for v in ["tokyo-neon", "Tokyo Neon Glass", "tokyoneon"] {
            let c = css(v, false);
            assert!(
                c.contains("@define-color window_bg_color #0a1a14;"),
                "{v}: {c}"
            );
        }
    }

    #[test]
    fn clear_glass_is_the_most_see_through() {
        assert!(css("clear", false).contains("alpha(#1c1f26, 0.55)"));
        assert!(css("clear", true).contains("alpha(#f7f9fc, 0.62)"));
        assert!(css("ember", false).contains("alpha(#3a2418, 0.85)"));
    }
}
