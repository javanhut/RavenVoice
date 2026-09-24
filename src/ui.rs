//! The overlay: a small floating bar that shows whether you are being heard,
//! what was understood, which microphone is in use, and a panel for reading
//! text aloud.
//!
//! On Raven it is a wlr layer surface above all windows that does not take
//! keyboard focus, so clicking the microphone button leaves the cursor in the
//! app you are dictating into. Focus is only accepted while the read-aloud
//! panel is open (so you can type in it).
//!
//! "Hiding" shrinks the bar to just the microphone button rather than
//! unmapping the window: huginn disconnects a client that commits to a layer
//! surface's wl_surface after destroying it, which is what GTK does on hide.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;

use crate::audio::MicInfo;
use crate::config::Config;
use crate::engine::{Cmd, Engine, UiEvent};

const APP_ID: &str = "org.raven.RavenVoice";

struct Ui {
    engine: Engine,
    cfg: Config,
    window: gtk::ApplicationWindow,
    mic_button: gtk::Button,
    level: gtk::LevelBar,
    status: gtk::Label,
    mic_model: gtk::StringList,
    mic_select: gtk::DropDown,
    tts_toggle: gtk::ToggleButton,
    tts_entry: gtk::Entry,
    /// Everything but the microphone button, hidden in compact mode.
    extras: Vec<gtk::Widget>,
    minimize: gtk::Button,
    compact: Cell<bool>,

    mic_ids: RefCell<Vec<Option<String>>>,
    /// Set while we change the dropdown ourselves, so it is not taken as a user choice.
    updating_mics: Cell<bool>,
    ready: Cell<bool>,
    download: Cell<Option<f32>>,
    listening: Cell<bool>,
    /// Listening was requested but the microphone has not delivered audio yet.
    opening: Cell<bool>,
    hearing: Cell<bool>,
    transcribing: Cell<bool>,
    speaking: Cell<bool>,
    partial: RefCell<String>,
    last_final: RefCell<Option<(String, Instant)>>,
    flash: RefCell<Option<(String, Instant, bool)>>,
    fatal: RefCell<Option<String>>,
}

pub fn run(
    cfg: Config,
    engine: Engine,
    events: async_channel::Receiver<UiEvent>,
) -> glib::ExitCode {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    let events = RefCell::new(Some(events));
    app.connect_activate(move |app| {
        if let Some(win) = app.active_window() {
            win.present();
            return;
        }
        let Some(events) = events.borrow_mut().take() else {
            return;
        };
        let ui = build(app, cfg.clone(), engine.clone());
        let ui2 = ui.clone();
        glib::spawn_future_local(async move {
            while let Ok(ev) = events.recv().await {
                ui2.handle(ev);
            }
        });
        // Re-render periodically so brief notices expire.
        let weak = Rc::downgrade(&ui);
        glib::timeout_add_local(Duration::from_millis(500), move || match weak.upgrade() {
            Some(ui) => {
                ui.render();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
    });
    // GTK must not try to parse our command line.
    app.run_with_args::<&str>(&[])
}

fn css(font_size: u32) -> String {
    format!(
        r#"
window.ravenvoice {{ background: transparent; }}
.rv-bar {{
  background-color: rgba(22, 22, 31, 0.94);
  color: #f2f2f7;
  border-radius: 20px;
  border: 1px solid rgba(255, 255, 255, 0.14);
  padding: 8px 10px;
  font-size: {font_size}pt;
}}
.rv-bar button, .rv-bar dropdown > button {{
  min-height: 36px;
  min-width: 36px;
  border-radius: 12px;
}}
.rv-mic {{ border-radius: 999px; min-width: 44px; min-height: 44px; }}
.rv-mic.listening {{ background: #d7263d; color: #ffffff; }}
.rv-mic.hearing {{ background: #ff3b52; box-shadow: 0 0 0 3px rgba(255, 59, 82, 0.45); }}
.rv-status {{ padding: 0 6px; }}
.rv-partial {{ font-style: italic; color: #c9c9d6; }}
.rv-error {{ color: #ff9a9a; font-weight: bold; }}
.rv-ok {{ color: #9be7a4; }}
levelbar.rv-level block.filled {{ background-color: #4fd17a; }}
levelbar.rv-level block.empty {{ background-color: rgba(255, 255, 255, 0.12); }}
.rv-panel {{ margin-top: 8px; }}
"#
    )
}

fn icon_button(icon: &str, label: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.set_tooltip_text(Some(label));
    b.update_property(&[gtk::accessible::Property::Label(label)]);
    b.set_focus_on_click(false);
    b
}

fn build(app: &gtk::Application, cfg: Config, engine: Engine) -> Rc<Ui> {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&css(cfg.overlay.font_size.clamp(8, 40)));
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("a display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let window = gtk::ApplicationWindow::new(app);
    window.set_title(Some("RavenVoice"));
    window.add_css_class("ravenvoice");
    window.set_resizable(false);
    window.set_decorated(false);
    place_overlay(&window, &cfg);

    let bar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bar.add_css_class("rv-bar");
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);

    let mic_button = icon_button("audio-input-microphone-symbolic", "Start dictation");
    mic_button.add_css_class("rv-mic");

    let level = gtk::LevelBar::for_interval(0.0, 1.0);
    level.add_css_class("rv-level");
    level.set_size_request(70, 8);
    level.set_valign(gtk::Align::Center);
    level.update_property(&[gtk::accessible::Property::Label("Microphone level")]);

    let status = gtk::Label::new(Some("Starting…"));
    status.add_css_class("rv-status");
    status.set_width_chars(40);
    status.set_max_width_chars(40);
    status.set_ellipsize(gtk::pango::EllipsizeMode::Start);
    status.set_xalign(0.0);
    status.set_hexpand(true);

    let mic_model = gtk::StringList::new(&["System default"]);
    let mic_select = gtk::DropDown::new(Some(mic_model.clone()), gtk::Expression::NONE);
    mic_select.set_tooltip_text(Some("Microphone"));
    mic_select.update_property(&[gtk::accessible::Property::Label("Microphone")]);
    mic_select.set_focus_on_click(false);

    let tts_toggle = gtk::ToggleButton::new();
    tts_toggle.set_icon_name("audio-speakers-symbolic");
    tts_toggle.set_tooltip_text(Some("Read text aloud"));
    tts_toggle.update_property(&[gtk::accessible::Property::Label("Read text aloud")]);
    tts_toggle.set_focus_on_click(false);

    let minimize = icon_button("go-down-symbolic", "Shrink to the microphone button");

    row.append(&mic_button);
    row.append(&level);
    row.append(&status);
    row.append(&mic_select);
    row.append(&tts_toggle);
    row.append(&minimize);
    bar.append(&row);

    // Read-aloud panel.
    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    let panel = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    panel.add_css_class("rv-panel");
    let tts_entry = gtk::Entry::new();
    tts_entry.set_placeholder_text(Some("Type or paste text to read aloud, then press Enter"));
    tts_entry.set_hexpand(true);
    tts_entry.update_property(&[gtk::accessible::Property::Label("Text to read aloud")]);
    let speak_btn = gtk::Button::with_label("Speak");
    let clip_btn = gtk::Button::with_label("Read clipboard");
    let last_btn = gtk::Button::with_label("Read last dictation");
    let stop_btn = icon_button("media-playback-stop-symbolic", "Stop speaking");
    panel.append(&tts_entry);
    panel.append(&speak_btn);
    panel.append(&clip_btn);
    panel.append(&last_btn);
    panel.append(&stop_btn);
    revealer.set_child(Some(&panel));
    bar.append(&revealer);
    window.set_child(Some(&bar));

    let ui = Rc::new(Ui {
        engine,
        cfg,
        window: window.clone(),
        mic_button: mic_button.clone(),
        level: level.clone(),
        status: status.clone(),
        mic_model,
        mic_select: mic_select.clone(),
        tts_toggle: tts_toggle.clone(),
        tts_entry: tts_entry.clone(),
        extras: vec![
            level.clone().upcast(),
            status.clone().upcast(),
            mic_select.clone().upcast(),
            tts_toggle.clone().upcast(),
        ],
        minimize: minimize.clone(),
        compact: Cell::new(false),
        mic_ids: RefCell::new(vec![None]),
        updating_mics: Cell::new(false),
        ready: Cell::new(false),
        download: Cell::new(None),
        listening: Cell::new(false),
        opening: Cell::new(false),
        hearing: Cell::new(false),
        transcribing: Cell::new(false),
        speaking: Cell::new(false),
        partial: RefCell::new(String::new()),
        last_final: RefCell::new(None),
        flash: RefCell::new(None),
        fatal: RefCell::new(None),
    });

    let e = ui.engine.clone();
    mic_button.connect_clicked(move |_| e.send(Cmd::Toggle));

    let weak = Rc::downgrade(&ui);
    mic_select.connect_selected_notify(move |dd| {
        let Some(ui) = weak.upgrade() else { return };
        if ui.updating_mics.get() {
            return;
        }
        let id = ui
            .mic_ids
            .borrow()
            .get(dd.selected() as usize)
            .cloned()
            .flatten();
        ui.engine.send(Cmd::SelectDevice(id));
    });

    let win = window.clone();
    tts_toggle.connect_toggled(move |t| {
        revealer.set_reveal_child(t.is_active());
        set_keyboard_focusable(&win, t.is_active());
    });

    let weak = Rc::downgrade(&ui);
    minimize.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.set_compact(!ui.compact.get());
        }
    });

    let speak_entry = {
        let (e, entry) = (ui.engine.clone(), tts_entry.clone());
        move || {
            let text = entry.text().to_string();
            if !text.trim().is_empty() {
                e.send(Cmd::Speak(text));
            }
        }
    };
    let s = speak_entry.clone();
    tts_entry.connect_activate(move |_| s());
    speak_btn.connect_clicked(move |_| speak_entry());

    let e = ui.engine.clone();
    last_btn.connect_clicked(move |_| e.send(Cmd::SpeakLast));
    let e = ui.engine.clone();
    stop_btn.connect_clicked(move |_| e.send(Cmd::StopSpeaking));

    let weak = Rc::downgrade(&ui);
    clip_btn.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            glib::spawn_future_local(async move { ui.speak_clipboard().await });
        }
    });

    ui.set_compact(!ui.cfg.overlay.visible);
    window.present();
    ui.render();
    ui
}

#[cfg(feature = "layer-shell")]
fn place_overlay(window: &gtk::ApplicationWindow, cfg: &Config) {
    use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
    if !gtk4_layer_shell::is_supported() {
        log::warn!("compositor has no layer-shell; the overlay is a normal window");
        return;
    }
    window.init_layer_shell();
    window.set_namespace(Some("ravenvoice"));
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::None);
    let edge = if cfg.overlay.position == "top" {
        Edge::Top
    } else {
        Edge::Bottom
    };
    window.set_anchor(edge, true);
    window.set_margin(edge, cfg.overlay.margin);
    window.set_exclusive_zone(-1);
}

#[cfg(not(feature = "layer-shell"))]
fn place_overlay(_: &gtk::ApplicationWindow, _: &Config) {}

#[cfg(feature = "layer-shell")]
fn set_keyboard_focusable(window: &gtk::ApplicationWindow, mode: impl Into<Option<bool>>) {
    use gtk4_layer_shell::{KeyboardMode, LayerShell};
    if window.is_layer_window() {
        window.set_keyboard_mode(match mode.into() {
            Some(true) => KeyboardMode::OnDemand,
            Some(false) => KeyboardMode::None,
            None => KeyboardMode::Exclusive,
        });
    }
}

#[cfg(not(feature = "layer-shell"))]
fn set_keyboard_focusable(_: &gtk::ApplicationWindow, _: impl Into<Option<bool>>) {}

impl Ui {
    fn handle(&self, ev: UiEvent) {
        match ev {
            UiEvent::Ready { model, tts } => {
                self.ready.set(true);
                self.download.set(None);
                self.flash(format!("Ready — Whisper {model}, voice: {tts}"), false);
            }
            UiEvent::ModelProgress(f) => self.download.set(Some(f)),
            UiEvent::Listening(on) => {
                self.listening.set(on);
                self.opening.set(on);
                self.partial.borrow_mut().clear();
                let label = if on {
                    "Stop dictation"
                } else {
                    "Start dictation"
                };
                self.mic_button.set_tooltip_text(Some(label));
                self.mic_button
                    .update_property(&[gtk::accessible::Property::Label(label)]);
                if on {
                    self.mic_button.add_css_class("listening");
                    self.fatal.borrow_mut().take();
                } else {
                    self.mic_button.remove_css_class("listening");
                    self.mic_button.remove_css_class("hearing");
                }
                if !on {
                    self.announce("Dictation off");
                }
            }
            UiEvent::MicLive => {
                self.opening.set(false);
                self.announce("Listening");
            }
            UiEvent::Hearing(on) => {
                self.hearing.set(on);
                if on {
                    self.mic_button.add_css_class("hearing");
                } else {
                    self.mic_button.remove_css_class("hearing");
                }
            }
            UiEvent::Transcribing(on) => self.transcribing.set(on),
            UiEvent::Speaking(on) => self.speaking.set(on),
            UiEvent::Level(v) => self.level.set_value(v),
            UiEvent::Partial(text) => *self.partial.borrow_mut() = text,
            UiEvent::Final(text) => {
                self.partial.borrow_mut().clear();
                self.announce(&text);
                *self.last_final.borrow_mut() = Some((text, Instant::now()));
            }
            UiEvent::Mics { list, selected } => self.set_mics(&list, selected),
            UiEvent::Notice(text) => {
                self.announce(&text);
                self.flash(text, false);
            }
            UiEvent::Error(text) => {
                log::error!("{text}");
                self.announce(&text);
                if self.ready.get() || self.listening.get() {
                    self.flash(text, true);
                } else {
                    // Nothing works until this is fixed: keep it on screen.
                    *self.fatal.borrow_mut() = Some(text);
                }
            }
            UiEvent::SetVisible(v) => {
                let show = v.unwrap_or(self.compact.get());
                self.set_compact(!show);
            }
        }
        self.render();
    }

    fn set_compact(&self, compact: bool) {
        self.compact.set(compact);
        if compact {
            self.tts_toggle.set_active(false);
        }
        for w in &self.extras {
            w.set_visible(!compact);
        }
        let (icon, label) = if compact {
            ("go-up-symbolic", "Show the full RavenVoice bar")
        } else {
            ("go-down-symbolic", "Shrink to the microphone button")
        };
        self.minimize.set_icon_name(icon);
        self.minimize.set_tooltip_text(Some(label));
        self.minimize
            .update_property(&[gtk::accessible::Property::Label(label)]);
    }

    fn flash(&self, text: String, error: bool) {
        *self.flash.borrow_mut() = Some((text, Instant::now(), error));
    }

    /// Tell screen readers (Orca) about a change without moving focus.
    fn announce(&self, text: &str) {
        if !text.is_empty() {
            self.status
                .announce(text, gtk::AccessibleAnnouncementPriority::Medium);
        }
    }

    fn render(&self) {
        let (text, class) = self.status_text();
        self.status.set_text(&text);
        self.status.set_tooltip_text(Some(&text));
        for c in ["rv-partial", "rv-error", "rv-ok"] {
            self.status.remove_css_class(c);
        }
        if let Some(class) = class {
            self.status.add_css_class(class);
        }
    }

    fn status_text(&self) -> (String, Option<&'static str>) {
        if let Some(err) = self.fatal.borrow().as_ref() {
            return (err.clone(), Some("rv-error"));
        }
        {
            let mut flash = self.flash.borrow_mut();
            if let Some((text, at, error)) = flash.as_ref() {
                let ttl = if *error { 8 } else { 4 };
                if at.elapsed() < Duration::from_secs(ttl) {
                    return (text.clone(), error.then_some("rv-error"));
                }
                *flash = None;
            }
        }
        if !self.ready.get() {
            return match self.download.get() {
                Some(f) => (format!("Downloading speech model… {:.0}%", f * 100.0), None),
                None => ("Loading speech model…".into(), None),
            };
        }
        if self.speaking.get() {
            return ("Speaking…".into(), None);
        }
        let recent_final = self
            .last_final
            .borrow()
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(5))
            .map(|(t, _)| format!("✓ {t}"));
        if self.listening.get() {
            if self.opening.get() {
                return ("Opening microphone…".into(), None);
            }
            let partial = self.partial.borrow();
            if !partial.is_empty() {
                return (partial.clone(), Some("rv-partial"));
            }
            if self.hearing.get() {
                return ("Hearing you…".into(), None);
            }
            if self.transcribing.get() {
                return ("Writing…".into(), None);
            }
            if let Some(t) = recent_final {
                return (t, Some("rv-ok"));
            }
            return ("Listening… speak now".into(), None);
        }
        if let Some(t) = recent_final {
            return (t, Some("rv-ok"));
        }
        let hint = if self.cfg.hotkeys.enabled && !self.cfg.hotkeys.toggle.is_empty() {
            format!(
                "Press {} or the microphone to dictate",
                self.cfg.hotkeys.toggle
            )
        } else {
            "Click the microphone to dictate".into()
        };
        (hint, None)
    }

    fn set_mics(&self, list: &[MicInfo], selected: Option<String>) {
        self.updating_mics.set(true);
        let mut ids = vec![None];
        let mut names = vec!["System default".to_string()];
        for mic in list {
            ids.push(Some(mic.id.clone()));
            let mut name = mic.display();
            if mic.is_default {
                name.push_str(" •");
            }
            names.push(name);
        }
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        self.mic_model.splice(0, self.mic_model.n_items(), &refs);
        let index = ids.iter().position(|id| *id == selected).unwrap_or(0);
        self.mic_select.set_selected(index as u32);
        *self.mic_ids.borrow_mut() = ids;
        self.updating_mics.set(false);
    }

    /// Wayland only hands the clipboard to a focused window, so take focus
    /// for a moment, read it, then give focus back.
    async fn speak_clipboard(self: Rc<Self>) {
        set_keyboard_focusable(&self.window, None);
        glib::timeout_future(Duration::from_millis(250)).await;
        let text = self.window.clipboard().read_text_future().await;
        set_keyboard_focusable(&self.window, self.tts_toggle.is_active());
        match text {
            Ok(Some(text)) if !text.trim().is_empty() => {
                self.tts_entry.set_text(&text);
                self.engine.send(Cmd::Speak(text.to_string()));
            }
            _ => self.handle(UiEvent::Notice(
                "The clipboard has no text — copy something first".into(),
            )),
        }
    }
}
