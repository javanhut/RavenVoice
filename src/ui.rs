//! The overlay: a floating pill with the microphone button, a live waveform,
//! what is happening right now, and quick access to read-aloud and settings.
//!
//! On Raven it is a wlr layer surface above all windows that does not take
//! keyboard focus, so clicking the microphone button leaves the cursor in the
//! app you are dictating into. Focus is only accepted while the read-aloud
//! panel is open (so you can type in it).
//!
//! "Closing" shrinks the bar to just the microphone button rather than
//! unmapping the window: huginn disconnects a client that commits to a layer
//! surface's wl_surface after destroying it, which is what GTK does on hide.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::f64::consts::PI;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;

use crate::audio::MicInfo;
use crate::config::{Accuracy, Config};
use crate::engine::{Cmd, Engine, UiEvent};

const APP_ID: &str = "org.raven.RavenVoice";

/// Bars in the waveform; newest on the right.
const WAVE_BARS: usize = 34;

struct Ui {
    engine: Engine,
    cfg: Config,
    window: gtk::ApplicationWindow,
    mic_button: gtk::Button,
    wave: Wave,
    title: gtk::Label,
    subtitle: gtk::Label,
    mic_model: gtk::StringList,
    mic_select: gtk::DropDown,
    engine_info: gtk::Label,
    tts_toggle: gtk::ToggleButton,
    tts_entry: gtk::Entry,
    /// Everything but the microphone and close buttons, hidden in compact mode.
    extras: Vec<gtk::Widget>,
    close: gtk::Button,
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

/// What the two status lines say, and how.
struct Status {
    title: String,
    subtitle: String,
    tone: Tone,
}

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Normal,
    Live,
    Done,
    Error,
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
        start_timers(&ui);
    });
    // GTK must not try to parse our command line.
    app.run_with_args::<&str>(&[])
}

/// Render the bar in a given state to a PNG, for checking the design without
/// a screenshot tool. States: idle, listening, hearing, speaking, error, compact.
pub fn preview(cfg: Config, out: PathBuf, state: String) -> glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id("org.raven.RavenVoice.Preview")
        .build();
    app.connect_activate(move |app| {
        // An engine nobody listens to: the preview only needs its channels.
        let (ui_tx, _) = async_channel::unbounded();
        let engine = Engine {
            cmd: crossbeam_channel::unbounded().0,
            ui: ui_tx,
            shared: Default::default(),
            held: Default::default(),
        };
        let ui = build(app, cfg.clone(), engine);
        ui.handle(UiEvent::Ready {
            model: "base.en".into(),
            tts: "espeak-ng (en-us)".into(),
        });
        *ui.flash.borrow_mut() = None;
        match state.as_str() {
            "listening" | "hearing" => {
                ui.handle(UiEvent::Listening(true));
                ui.handle(UiEvent::MicLive);
                for i in 0..WAVE_BARS {
                    let x = i as f64 / WAVE_BARS as f64;
                    let speechy = ((x * 23.0).sin() * 0.5 + 0.5) * ((x * 7.0).cos() * 0.3 + 0.7);
                    ui.handle(UiEvent::Level(0.3 + 0.7 * speechy));
                }
                if state == "hearing" {
                    ui.handle(UiEvent::Hearing(true));
                    ui.handle(UiEvent::Partial("and so my fellow Americans".into()));
                }
            }
            "speaking" => ui.handle(UiEvent::Speaking(true)),
            "error" => ui.handle(UiEvent::Error(
                "Could not open the microphone: no microphone found".into(),
            )),
            "compact" => ui.set_compact(true),
            "panel" => ui.tts_toggle.set_active(true),
            _ => {}
        }
        ui.render();
        let out = out.clone();
        let app = app.clone();
        glib::timeout_add_local_once(Duration::from_millis(900), move || {
            if let Err(e) = snapshot_to_png(&ui, &out) {
                eprintln!("preview failed: {e}");
            } else {
                println!("wrote {}", out.display());
            }
            app.quit();
        });
    });
    app.run_with_args::<&str>(&[])
}

/// Open a focused window with a text field, type into it through the virtual
/// keyboard exactly as dictation does, and print what arrived.
pub fn test_typing(cfg: Config, watch: Option<u64>) -> glib::ExitCode {
    const SAMPLE: &str = "Hello from RavenVoice, testing 1 2 3.";
    let app = gtk::Application::builder()
        .application_id("org.raven.RavenVoice.TypingTest")
        .build();
    app.connect_activate(move |app| {
        let window = gtk::ApplicationWindow::new(app);
        window.set_title(Some("RavenVoice typing test"));
        window.set_default_size(520, 80);
        let entry = gtk::Entry::new();
        window.set_child(Some(&entry));
        window.present();
        entry.grab_focus();
        if let Some(secs) = watch {
            let started = Instant::now();
            entry.connect_changed(move |e| {
                println!("{:6.2}s  {:?}", started.elapsed().as_secs_f32(), e.text());
            });
            let app = app.clone();
            glib::timeout_add_local_once(Duration::from_secs(secs), move || app.quit());
            return;
        }
        let (cfg, app) = (cfg.clone(), app.clone());
        glib::timeout_add_local_once(Duration::from_millis(700), move || {
            let (tx, rx) = std::sync::mpsc::channel();
            let typing = cfg.typing.clone();
            std::thread::spawn(move || {
                let result = crate::typer::Typer::new(&typing, Default::default())
                    .and_then(|mut t| t.perform(&[crate::commands::Action::Text(SAMPLE.into())]));
                let _ = tx.send(result.map_err(|e| format!("{e:#}")));
            });
            glib::timeout_add_local(Duration::from_millis(100), move || match rx.try_recv() {
                Ok(result) => {
                    let entry = entry.clone();
                    let app = app.clone();
                    glib::timeout_add_local_once(Duration::from_millis(400), move || {
                        let got = entry.text();
                        println!("sent:     {SAMPLE:?}");
                        println!("received: {got:?}");
                        match result {
                            Err(e) => println!("typer error: {e}"),
                            Ok(()) if got == SAMPLE => println!("OK: typing works"),
                            Ok(()) => println!("MISMATCH: keys were sent but did not all arrive"),
                        }
                        app.quit();
                    });
                    glib::ControlFlow::Break
                }
                Err(_) => glib::ControlFlow::Continue,
            });
        });
    });
    app.run_with_args::<&str>(&[])
}

fn snapshot_to_png(ui: &Ui, out: &std::path::Path) -> Result<(), String> {
    let root = &ui.window;
    let (w, h) = (root.width() as f64, root.height() as f64);
    log::debug!(
        "window {w}x{h}, mic {}x{}",
        ui.mic_button.width(),
        ui.mic_button.height()
    );
    let paintable = gtk::WidgetPaintable::new(Some(root));
    let snapshot = gtk::Snapshot::new();
    snapshot.scale(2.0, 2.0);
    paintable.snapshot(&snapshot, w, h);
    let node = snapshot.to_node().ok_or("nothing drawn")?;
    let renderer = ui.window.renderer().ok_or("no renderer")?;
    let texture = renderer.render_texture(&node, None);
    texture.save_to_png(out).map_err(|e| e.to_string())
}

fn start_timers(ui: &Rc<Ui>) {
    // Re-render periodically so brief notices expire.
    let weak = Rc::downgrade(ui);
    glib::timeout_add_local(Duration::from_millis(500), move || match weak.upgrade() {
        Some(ui) => {
            ui.render();
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });
    // Keep the waveform alive: a synthetic voice while speaking, a gentle
    // fall back to dots when nothing is being heard.
    let weak = Rc::downgrade(ui);
    let mut phase = 0.0f64;
    glib::timeout_add_local(Duration::from_millis(50), move || {
        let Some(ui) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if ui.speaking.get() {
            phase += 0.55;
            let v = 0.55 + 0.3 * (phase).sin() * (phase * 0.37).cos() + 0.15 * (phase * 2.3).sin();
            ui.wave.push(v.clamp(0.0, 1.0));
        } else if !ui.listening.get() {
            ui.wave.decay();
        }
        glib::ControlFlow::Continue
    });
}

fn css(font_size: u32) -> String {
    let title = font_size + 2;
    let small = font_size.saturating_sub(1).max(8);
    format!(
        r#"
window.ravenvoice {{ background: transparent; }}

.rv-bar {{
  background-color: rgba(17, 20, 31, 0.96);
  background-image: linear-gradient(180deg, rgba(255,255,255,0.035), rgba(255,255,255,0));
  color: #eef0f8;
  border-radius: 40px;
  border: 1px solid rgba(255, 255, 255, 0.09);
  box-shadow: 0 10px 30px rgba(0, 0, 0, 0.45);
  padding: 8px 12px 8px 8px;
  margin: 14px;
  font-size: {font_size}pt;
}}

/* Microphone: a dark disc inside a ring that lights up while listening. */
button.rv-mic, button.rv-mic:hover, button.rv-mic:active, button.rv-mic:checked {{
  min-width: 0; min-height: 0;
  padding: 3px;
  border: none;
  border-radius: 999px;
  background-color: transparent;
  background-image: linear-gradient(135deg, #4a4e66, #2c2f42);
  box-shadow: none;
  transition: box-shadow 200ms ease;
}}
button.rv-mic.listening {{
  background-image: linear-gradient(135deg, #6d28d9 0%, #a21caf 55%, #e11d48 100%);
  box-shadow: 0 0 18px rgba(192, 38, 211, 0.40);
}}
button.rv-mic.hearing {{ box-shadow: 0 0 26px rgba(236, 72, 153, 0.70); }}
.rv-mic-core {{
  min-width: 64px; min-height: 64px;
  border-radius: 999px;
  background-color: #151826;
  color: #ffffff;
}}
button.rv-mic:hover .rv-mic-core {{ background-color: #1c2031; }}
button.rv-mic:active .rv-mic-core {{ background-color: #23283b; }}

.rv-divider {{
  min-width: 1px;
  background-color: rgba(255, 255, 255, 0.10);
  margin: 10px 4px;
}}

.rv-title {{ font-size: {title}pt; font-weight: 600; color: #f4f5fb; }}
.rv-subtitle {{ font-size: {small}pt; color: #8a90a8; }}
.rv-title.live {{ color: #f4f5fb; }}
.rv-subtitle.live {{ color: #c7b8f5; font-style: italic; }}
.rv-title.done {{ color: #86efac; }}
.rv-title.error {{ color: #fca5a5; }}
.rv-subtitle.error {{ color: #e8b4b4; }}

/* Square-ish controls on the right. */
.rv-group {{
  border-radius: 14px;
  background-color: rgba(255, 255, 255, 0.06);
}}
.rv-group button {{ border-radius: 14px; }}
button.rv-ctl, .rv-group button, menubutton.rv-ctl > button {{
  min-width: 44px; min-height: 44px;
  padding: 0 10px;
  border: none;
  background-color: rgba(255, 255, 255, 0.06);
  background-image: none;
  box-shadow: none;
  color: #dfe2ee;
  border-radius: 14px;
}}
.rv-group button {{ background-color: transparent; }}
.rv-group button.rv-read {{ padding: 0 8px 0 12px; }}
.rv-group menubutton > button {{ min-width: 30px; padding: 0 10px 0 4px; }}
button.rv-ctl:hover, .rv-group button:hover, menubutton.rv-ctl > button:hover {{
  background-color: rgba(255, 255, 255, 0.12);
}}
.rv-group button:checked {{ background-color: rgba(139, 92, 246, 0.28); color: #ffffff; }}
.rv-glyph {{ font-weight: 700; font-size: {title}pt; }}

/* Read-aloud panel. */
.rv-panel {{ margin: 10px 4px 2px 4px; }}
.rv-panel entry {{
  min-height: 40px;
  border-radius: 12px;
  background-color: rgba(255, 255, 255, 0.06);
  color: #eef0f8;
  border: 1px solid rgba(255, 255, 255, 0.08);
  box-shadow: none;
}}
.rv-panel button.rv-primary {{
  background-image: linear-gradient(135deg, #7c3aed, #c026d3);
  color: white; font-weight: 600; min-height: 40px; border-radius: 12px; border: none;
  padding: 0 16px;
}}

/* Popovers. */
popover.rv-pop > contents {{
  background-color: #171a28;
  color: #eef0f8;
  border-radius: 14px;
  border: 1px solid rgba(255, 255, 255, 0.09);
  padding: 8px;
}}
popover.rv-pop button {{
  min-height: 36px; border-radius: 10px; padding: 0 12px;
  background: transparent; border: none; box-shadow: none; color: #eef0f8;
}}
popover.rv-pop button:hover {{ background-color: rgba(255, 255, 255, 0.08); }}
popover.rv-pop .rv-heading {{ font-weight: 600; color: #8a90a8; font-size: {small}pt; margin: 4px 6px; }}
popover.rv-pop .rv-info {{ color: #8a90a8; font-size: {small}pt; margin: 4px 6px; }}
"#
    )
}

fn icon_button(icon: &str, label: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    set_label(&b, label);
    b.set_focus_on_click(false);
    b
}

fn set_label(w: &(impl IsA<gtk::Widget> + IsA<gtk::Accessible>), label: &str) {
    w.set_tooltip_text(Some(label));
    w.update_property(&[gtk::accessible::Property::Label(label)]);
}

fn menu_item(label: &str, action: impl Fn() + 'static) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    if let Some(l) = b.child().and_downcast::<gtk::Label>() {
        l.set_xalign(0.0);
    }
    b.connect_clicked(move |_| action());
    b
}

fn popover(bottom_anchored: bool) -> (gtk::Popover, gtk::Box) {
    let pop = gtk::Popover::new();
    pop.add_css_class("rv-pop");
    pop.set_has_arrow(false);
    pop.set_position(if bottom_anchored {
        gtk::PositionType::Top
    } else {
        gtk::PositionType::Bottom
    });
    let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
    pop.set_child(Some(&body));
    (pop, body)
}

fn build(app: &gtk::Application, cfg: Config, engine: Engine) -> Rc<Ui> {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&css(cfg.overlay.font_size.clamp(8, 40)));
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("a display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let bottom = cfg.overlay.position != "top";

    let window = gtk::ApplicationWindow::new(app);
    window.set_title(Some("RavenVoice"));
    window.add_css_class("ravenvoice");
    window.set_resizable(false);
    window.set_decorated(false);
    place_overlay(&window, &cfg);

    let bar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bar.add_css_class("rv-bar");
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);

    // Microphone.
    let mic_button = gtk::Button::new();
    mic_button.add_css_class("rv-mic");
    mic_button.set_focus_on_click(false);
    mic_button.set_valign(gtk::Align::Center);
    let core = gtk::Box::new(gtk::Orientation::Vertical, 0);
    core.add_css_class("rv-mic-core");
    let mic_icon = gtk::Image::from_icon_name("audio-input-microphone-symbolic");
    mic_icon.set_pixel_size(26);
    mic_icon.set_vexpand(true);
    mic_icon.set_valign(gtk::Align::Center);
    core.append(&mic_icon);
    mic_button.set_child(Some(&core));
    set_label(&mic_button, "Start dictation");

    // Waveform.
    let wave = Wave::new();

    let divider = gtk::Box::new(gtk::Orientation::Vertical, 0);
    divider.add_css_class("rv-divider");

    // Status.
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_valign(gtk::Align::Center);
    text.set_hexpand(true);
    let title = gtk::Label::new(Some("Starting…"));
    title.add_css_class("rv-title");
    let subtitle = gtk::Label::new(None);
    subtitle.add_css_class("rv-subtitle");
    for (label, chars) in [(&title, 26), (&subtitle, 34)] {
        label.set_xalign(0.0);
        label.set_width_chars(chars);
        label.set_max_width_chars(chars);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    }
    text.append(&title);
    text.append(&subtitle);

    // Read aloud: the ✦A toggle opens the panel, the chevron offers shortcuts.
    let group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    group.add_css_class("rv-group");
    group.set_valign(gtk::Align::Center);
    let tts_toggle = gtk::ToggleButton::new();
    tts_toggle.add_css_class("rv-read");
    let glyph = gtk::Label::new(None);
    glyph.set_markup("<span size='62%' rise='5000'>✦</span>A");
    glyph.add_css_class("rv-glyph");
    tts_toggle.set_child(Some(&glyph));
    tts_toggle.set_focus_on_click(false);
    set_label(&tts_toggle, "Read text aloud");
    let tts_menu = gtk::MenuButton::new();
    tts_menu.set_icon_name("pan-down-symbolic");
    tts_menu.set_focus_on_click(false);
    set_label(&tts_menu, "Read-aloud options");
    group.append(&tts_toggle);
    group.append(&tts_menu);

    let divider2 = gtk::Box::new(gtk::Orientation::Vertical, 0);
    divider2.add_css_class("rv-divider");

    let settings = gtk::MenuButton::new();
    settings.set_icon_name("emblem-system-symbolic");
    settings.add_css_class("rv-ctl");
    settings.set_focus_on_click(false);
    settings.set_valign(gtk::Align::Center);
    set_label(&settings, "Microphone and settings");

    let close = icon_button("window-close-symbolic", "Shrink to the microphone button");
    close.add_css_class("rv-ctl");
    close.set_valign(gtk::Align::Center);

    row.append(&mic_button);
    row.append(wave.widget());
    row.append(&divider);
    row.append(&text);
    row.append(&group);
    row.append(&divider2);
    row.append(&settings);
    row.append(&close);
    bar.append(&row);

    // Read-aloud panel.
    let revealer = gtk::Revealer::new();
    // A collapsed revealer still claims its child's width; keep it out of
    // layout entirely until it is opened.
    revealer.set_visible(false);
    revealer.connect_child_revealed_notify(|r| {
        if !r.is_child_revealed() && !r.reveals_child() {
            r.set_visible(false);
        }
    });
    revealer.set_transition_type(if bottom {
        gtk::RevealerTransitionType::SlideUp
    } else {
        gtk::RevealerTransitionType::SlideDown
    });
    let panel = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    panel.add_css_class("rv-panel");
    let tts_entry = gtk::Entry::new();
    tts_entry.set_placeholder_text(Some("Type or paste text to read aloud, then press Enter"));
    tts_entry.set_hexpand(true);
    // Size to the bar rather than to the placeholder text.
    tts_entry.set_width_chars(8);
    tts_entry.update_property(&[gtk::accessible::Property::Label("Text to read aloud")]);
    let speak_btn = gtk::Button::with_label("Speak");
    speak_btn.add_css_class("rv-primary");
    let stop_btn = icon_button("media-playback-stop-symbolic", "Stop speaking");
    stop_btn.add_css_class("rv-ctl");
    panel.append(&tts_entry);
    panel.append(&speak_btn);
    panel.append(&stop_btn);
    revealer.set_child(Some(&panel));
    if bottom {
        // Grow upwards, away from the screen edge.
        bar.prepend(&revealer);
    } else {
        bar.append(&revealer);
    }
    window.set_child(Some(&bar));

    // Settings popover: microphone choice, what is loaded, config, quit.
    let (settings_pop, body) = popover(bottom);
    let heading = gtk::Label::new(Some("MICROPHONE"));
    heading.add_css_class("rv-heading");
    heading.set_xalign(0.0);
    let mic_model = gtk::StringList::new(&["System default"]);
    let mic_select = gtk::DropDown::new(Some(mic_model.clone()), gtk::Expression::NONE);
    set_label(&mic_select, "Microphone");
    let engine_info = gtk::Label::new(Some("Loading speech model…"));
    engine_info.add_css_class("rv-info");
    engine_info.set_xalign(0.0);
    engine_info.set_wrap(true);
    engine_info.set_max_width_chars(34);
    let accuracy_heading = gtk::Label::new(Some("ACCURACY"));
    accuracy_heading.add_css_class("rv-heading");
    accuracy_heading.set_xalign(0.0);
    let accuracy_select = gtk::DropDown::from_strings(&[
        "Fast — text ~0.5 s after you pause",
        "Accurate — fixes mistakes, ~2 s per phrase",
    ]);
    set_label(&accuracy_select, "Accuracy");
    accuracy_select.set_selected(match Accuracy::of(&cfg.stt) {
        Accuracy::Fast => 0,
        Accuracy::Accurate => 1,
    });
    let e = engine.clone();
    accuracy_select.connect_selected_notify(move |dd| {
        e.send(Cmd::SetAccuracy(if dd.selected() == 1 {
            Accuracy::Accurate
        } else {
            Accuracy::Fast
        }));
    });
    body.append(&heading);
    body.append(&mic_select);
    body.append(&accuracy_heading);
    body.append(&accuracy_select);
    body.append(&engine_info);
    body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    body.append(&menu_item("Open settings file", || {
        let path = crate::config::config_path();
        let uri = gio::File::for_path(&path).uri();
        if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
            log::warn!("opening {}: {e}", path.display());
        }
    }));
    let quit_app = app.clone();
    body.append(&menu_item("Quit RavenVoice", move || quit_app.quit()));
    settings.set_popover(Some(&settings_pop));

    let extras: Vec<gtk::Widget> = vec![
        wave.widget().clone().upcast(),
        divider.upcast(),
        text.upcast(),
        group.upcast(),
        divider2.upcast(),
        settings.clone().upcast(),
    ];
    let ui = Rc::new(Ui {
        engine,
        cfg,
        window: window.clone(),
        mic_button: mic_button.clone(),
        wave,
        title,
        subtitle,
        mic_model,
        mic_select: mic_select.clone(),
        engine_info,
        tts_toggle: tts_toggle.clone(),
        tts_entry: tts_entry.clone(),
        extras,
        close: close.clone(),
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
    // Read-aloud options popover.
    let (tts_pop, body) = popover(bottom);
    let e = ui.engine.clone();
    body.append(&menu_item("Read last dictation", move || {
        e.send(Cmd::SpeakLast)
    }));
    let weak = Rc::downgrade(&ui);
    body.append(&menu_item("Read clipboard", move || {
        if let Some(ui) = weak.upgrade() {
            glib::spawn_future_local(async move { ui.speak_clipboard().await });
        }
    }));
    let e = ui.engine.clone();
    body.append(&menu_item("Stop speaking", move || {
        e.send(Cmd::StopSpeaking)
    }));
    tts_menu.set_popover(Some(&tts_pop));

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
    let entry = tts_entry.clone();
    tts_toggle.connect_toggled(move |t| {
        if t.is_active() {
            revealer.set_visible(true);
        }
        revealer.set_reveal_child(t.is_active());
        set_keyboard_focusable(&win, t.is_active());
        if t.is_active() {
            entry.grab_focus();
        }
    });

    let weak = Rc::downgrade(&ui);
    close.connect_clicked(move |_| {
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
    stop_btn.connect_clicked(move |_| e.send(Cmd::StopSpeaking));

    ui.set_compact(!ui.cfg.overlay.visible);
    window.present();
    ui.render();
    ui
}

/// Scrolling level history drawn as rounded bars (dots when quiet), violet to blue.
struct Wave {
    area: gtk::DrawingArea,
    levels: Rc<RefCell<VecDeque<f64>>>,
    active: Rc<Cell<bool>>,
}

impl Wave {
    fn new() -> Wave {
        let area = gtk::DrawingArea::new();
        area.set_content_width(170);
        area.set_content_height(56);
        area.set_valign(gtk::Align::Center);
        area.update_property(&[gtk::accessible::Property::Label("Microphone level")]);
        let levels: Rc<RefCell<VecDeque<f64>>> =
            Rc::new(RefCell::new(std::iter::repeat_n(0.0, WAVE_BARS).collect()));
        let active = Rc::new(Cell::new(false));
        let (l, a) = (levels.clone(), active.clone());
        area.set_draw_func(move |_, cr, w, h| {
            draw_wave(cr, w as f64, h as f64, &l.borrow(), a.get())
        });
        Wave {
            area,
            levels,
            active,
        }
    }

    fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// Add a meter reading (0..1 from `audio::meter_level`).
    fn push(&self, level: f64) {
        let mut levels = self.levels.borrow_mut();
        levels.pop_front();
        levels.push_back(level);
        drop(levels);
        self.area.queue_draw();
    }

    fn set_active(&self, on: bool) {
        self.active.set(on);
        self.area.queue_draw();
    }

    fn decay(&self) {
        let mut levels = self.levels.borrow_mut();
        if levels.iter().all(|v| *v < 0.01) {
            return;
        }
        for v in levels.iter_mut() {
            *v *= 0.8;
        }
        drop(levels);
        self.area.queue_draw();
    }
}

fn draw_wave(cr: &gtk::cairo::Context, w: f64, h: f64, levels: &VecDeque<f64>, active: bool) {
    let n = levels.len().max(1);
    let step = w / n as f64;
    let bar = (step * 0.5).clamp(2.0, 4.0);
    let mid = h / 2.0;

    let gradient = gtk::cairo::LinearGradient::new(0.0, 0.0, w, 0.0);
    if active {
        gradient.add_color_stop_rgba(0.0, 0.85, 0.55, 0.98, 1.0); // #d98cfa
        gradient.add_color_stop_rgba(0.45, 0.55, 0.36, 0.96, 1.0); // #8b5cf6
        gradient.add_color_stop_rgba(1.0, 0.38, 0.55, 0.98, 1.0); // #608cfa
    } else {
        gradient.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.22);
        gradient.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.22);
    }
    let _ = cr.set_source(&gradient);

    for (i, level) in levels.iter().enumerate() {
        // Quiet room noise shows as dots; speech rises out of it. The ends
        // are tapered so the shape reads as a voice, not a meter.
        let amp = ((level - 0.3) / 0.6).clamp(0.0, 1.0);
        let taper = (PI * (i as f64 + 0.5) / n as f64).sin().powf(0.6);
        let height = amp * taper * (h - 4.0);
        let x = i as f64 * step + step / 2.0;
        if height < bar * 1.6 {
            cr.arc(x, mid, bar / 2.0 + 0.3, 0.0, 2.0 * PI);
            let _ = cr.fill();
        } else {
            let top = mid - height / 2.0;
            let r = bar / 2.0;
            cr.new_sub_path();
            cr.arc(x, top + r, r, PI, 0.0);
            cr.arc(x, top + height - r, r, 0.0, PI);
            cr.close_path();
            let _ = cr.fill();
        }
    }
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
    // The bar's CSS margin leaves room for its shadow.
    window.set_margin(edge, (cfg.overlay.margin - 14).max(0));
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
                self.engine_info
                    .set_text(&format!("Whisper {model} · voice: {tts}"));
                self.flash(format!("Ready — Whisper {model}"), false);
            }
            UiEvent::ModelProgress(f) => self.download.set(Some(f)),
            UiEvent::Loading(model) => {
                self.ready.set(false);
                self.download.set(None);
                self.engine_info
                    .set_text(&format!("Loading Whisper {model}…"));
            }
            UiEvent::Listening(on) => {
                self.listening.set(on);
                self.opening.set(on);
                self.partial.borrow_mut().clear();
                self.wave.set_active(on);
                set_label(
                    &self.mic_button,
                    if on {
                        "Stop dictation"
                    } else {
                        "Start dictation"
                    },
                );
                if on {
                    self.mic_button.add_css_class("listening");
                    self.fatal.borrow_mut().take();
                } else {
                    self.mic_button.remove_css_class("listening");
                    self.mic_button.remove_css_class("hearing");
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
            UiEvent::Speaking(on) => {
                self.speaking.set(on);
                self.wave.set_active(on || self.listening.get());
            }
            UiEvent::Level(v) => self.wave.push(v),
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
            ("window-close-symbolic", "Shrink to the microphone button")
        };
        self.close.set_icon_name(icon);
        set_label(&self.close, label);
    }

    fn flash(&self, text: String, error: bool) {
        *self.flash.borrow_mut() = Some((text, Instant::now(), error));
    }

    /// Tell screen readers (Orca) about a change without moving focus.
    fn announce(&self, text: &str) {
        if !text.is_empty() {
            self.title
                .announce(text, gtk::AccessibleAnnouncementPriority::Medium);
        }
    }

    fn render(&self) {
        let s = self.status();
        self.title.set_text(&s.title);
        self.subtitle.set_text(&s.subtitle);
        self.subtitle.set_visible(!s.subtitle.is_empty());
        let tooltip = if s.subtitle.is_empty() {
            s.title.clone()
        } else {
            format!("{}\n{}", s.title, s.subtitle)
        };
        self.title.set_tooltip_text(Some(&tooltip));
        self.subtitle.set_tooltip_text(Some(&tooltip));
        for label in [&self.title, &self.subtitle] {
            for c in ["live", "done", "error"] {
                label.remove_css_class(c);
            }
            match s.tone {
                Tone::Live => label.add_css_class("live"),
                Tone::Done => label.add_css_class("done"),
                Tone::Error => label.add_css_class("error"),
                Tone::Normal => {}
            }
        }
    }

    fn hint(&self, verb: &str) -> String {
        if self.cfg.hotkeys.enabled && !self.cfg.hotkeys.toggle.is_empty() {
            format!("Press {} to {verb}", self.cfg.hotkeys.toggle)
        } else {
            format!("Click the microphone to {verb}")
        }
    }

    fn status(&self) -> Status {
        let s = |title: &str, subtitle: String, tone| Status {
            title: title.to_string(),
            subtitle,
            tone,
        };
        if let Some(err) = self.fatal.borrow().as_ref() {
            return s("Something needs fixing", err.clone(), Tone::Error);
        }
        {
            let mut flash = self.flash.borrow_mut();
            if let Some((text, at, error)) = flash.as_ref() {
                let ttl = if *error { 8 } else { 4 };
                if at.elapsed() < Duration::from_secs(ttl) {
                    return if *error {
                        s("Problem", text.clone(), Tone::Error)
                    } else {
                        s("RavenVoice", text.clone(), Tone::Normal)
                    };
                }
                *flash = None;
            }
        }
        if !self.ready.get() {
            return match self.download.get() {
                Some(f) => s(
                    "Getting ready…",
                    format!("Downloading speech model {:.0}%", f * 100.0),
                    Tone::Normal,
                ),
                None => s(
                    "Getting ready…",
                    "Loading speech model".into(),
                    Tone::Normal,
                ),
            };
        }
        if self.speaking.get() {
            let stop = if self.cfg.hotkeys.stop_speaking.is_empty() {
                "Use the chevron menu to stop".to_string()
            } else {
                format!("Press {} to stop", self.cfg.hotkeys.stop_speaking)
            };
            return s("Speaking…", stop, Tone::Normal);
        }
        let recent_final = self
            .last_final
            .borrow()
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(5))
            .map(|(t, _)| t.clone());
        if self.listening.get() {
            if self.opening.get() {
                return s("Opening microphone…", "One moment".into(), Tone::Normal);
            }
            let partial = self.partial.borrow();
            if !partial.is_empty() {
                return s("Hearing you…", partial.clone(), Tone::Live);
            }
            if self.hearing.get() {
                return s("Hearing you…", self.hint("stop"), Tone::Normal);
            }
            if self.transcribing.get() {
                return s("Writing…", self.hint("stop"), Tone::Normal);
            }
            if let Some(t) = recent_final {
                return s("Typed", t, Tone::Done);
            }
            return s("Listening…", self.hint("stop"), Tone::Normal);
        }
        if let Some(t) = recent_final {
            return s("Typed", t, Tone::Done);
        }
        s("Ready to dictate", self.hint("start"), Tone::Normal)
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
