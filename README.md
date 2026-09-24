# RavenVoice

Offline dictation and read-aloud for Raven Linux: talk instead of typing, in
any app. Built for accessibility — for anyone for whom a keyboard is slow,
painful or impossible.

- **Speech to text** with [Whisper](https://github.com/ggml-org/whisper.cpp)
  (via `whisper-rs`), entirely on your machine. Nothing is sent anywhere.
- **Types into whatever has focus** — browsers, editors, terminals, native
  Wayland and XWayland apps alike.
- **Any microphone**: built-in, USB or Bluetooth. New microphones are
  announced when plugged in; if yours is unplugged mid-sentence RavenVoice
  switches to the system default and switches back when it returns.
- **Text to speech**: read typed or pasted text, the clipboard, or your last
  dictation aloud (espeak-ng, or Piper for natural voices).
- **A floating overlay** (GTK4 + layer-shell) that never steals focus from the
  app you are dictating into, with a live level meter, a live transcript and
  screen-reader announcements.

## Quick start

```sh
imlazy run        # installs anything missing, builds, starts the overlay
imlazy install    # install for your user and start with every session
```

That's all. `imlazy` runs `scripts/system-deps.sh` before every build. That
script installs whatever is missing — with `rvn` on Raven, or `pacman`, `apt`,
`dnf`, `zypper`, `xbps` or `apk` elsewhere — and lets the `input` group use
`/dev/uinput`, the virtual keyboard that types dictated text. It asks for your
password only when something actually needs installing. The first start
downloads the speech model (`base.en`, 142 MB) once.

Check everything with:

```sh
imlazy doctor
```

## Using it

| Do this | To |
|---|---|
| **Ctrl+Alt+D**, or click the microphone | start / stop dictating |
| **Ctrl+Alt+R** | read the last dictated phrase aloud |
| **Ctrl+Alt+X** | stop reading aloud |
| click 🔊 on the overlay | open the read-aloud panel (type, paste, *Read clipboard*) |
| click ⌄ on the overlay | shrink it to just the microphone button |

Speak naturally and pause briefly between phrases; each phrase is typed about
half a second after you pause. The microphone button turns red while
listening and glows while it hears you.

**Spoken commands** (say them on their own, as a separate phrase):

| Say | Does |
|---|---|
| "new line" / "new paragraph" | Enter / Enter twice (these also work mid-sentence) |
| "scratch that" / "delete that" | erase the last phrase |
| "undo" / "redo" | Ctrl+Z / Ctrl+Shift+Z |
| "select all" / "copy that" / "cut that" / "paste" | Ctrl+A / C / X / V |
| "press enter" / "press tab" / "press escape" / "backspace" | that key |
| "stop listening" | turn dictation off |

### From scripts and other programs

The running instance listens on `$XDG_RUNTIME_DIR/ravenvoice.sock`:

```sh
ravenvoice toggle | start | stop
ravenvoice speak "Hello there"      # or: some-command | ravenvoice speak
ravenvoice speak-last | stop-speaking
ravenvoice show | hide | status | quit
ravenvoice devices                  # list microphones and their ids
ravenvoice models                   # list Whisper models
ravenvoice download-model small.en
ravenvoice transcribe recording.wav # offline test
```

## Accuracy

Pick it in the overlay's ⚙ menu, or run `ravenvoice accuracy fast|accurate`:

| Mode | How it works | Speed on CPU |
|---|---|---|
| **Fast** (default) | `base.en` does everything | text ~0.5 s after you pause |
| **Accurate** | `base.en` types words live while you talk, then `small.en` re-transcribes each finished phrase and corrects only the words it got wrong | corrections land ~2 s after you pause |

On noisy test sentences, Fast wrote "RVM" for "rvn" and "for thesis comparing
photos emphasis in Algae"; Accurate got them right. The first switch
downloads `small.en-q5_1` (190 MB). Beam search and larger models were
measured too: beam search was slower and no better, and `large-v3-turbo`
takes ~13 s per phrase on CPU. It needs a GPU build (`--features vulkan`).

Names and jargon it keeps missing go in `stt.vocabulary`.

## Settings

`~/.config/ravenvoice/config.toml` is created on first run. The ones people
change most:

```toml
[stt]
vocabulary = "Raven, Huginn, rvn"   # names and jargon it should know

[audio]
device = "pulseaudio:alsa_input.usb-..."   # from `ravenvoice devices`; unset = system default
silence_ms = 800        # pause that ends a phrase; raise it if you speak slowly

[hotkeys]
toggle = "Ctrl+Alt+D"
push_to_talk = "RightCtrl"   # hold to talk, release to stop (empty = off)

[typing]
key_delay_ms = 4        # raise if an app drops letters

[tts]
rate_wpm = 175
echo_dictation = false  # read each dictated phrase back to you
piper_model = "/path/to/en_US-amy-medium.onnx"   # natural voice via Piper

[overlay]
position = "bottom"     # or "top"
font_size = 13          # raise for low vision
```

Choosing a microphone in the overlay's drop-down saves it here too.

## How it works on Raven

| Need | Huginn offers | RavenVoice uses |
|---|---|---|
| overlay above every window, no focus stealing | `zwlr_layer_shell_v1` | gtk4-layer-shell, keyboard interactivity *none* |
| typing into other apps | no virtual-keyboard or input-method protocol | a kernel virtual keyboard (`/dev/uinput`) |
| global shortcuts | no shortcut protocol | reading keyboards via evdev (`input` group), never grabbing them |
| microphones | PipeWire (pipewire-pulse) | cpal's PulseAudio host, 16 kHz mono straight from the server |

Speech is split into phrases by an adaptive voice-activity detector, and each
phrase goes to Whisper with the encoder window sized to the phrase, which is
3–4× faster than Whisper's fixed 30-second window. On a Meteor Lake laptop
with `base.en`, a phrase comes back in about 0.4 s.

`imlazy install` registers a supervised `raven-init --user` service
(`~/.config/raven/services/ravenvoice.toml`); `raven-rc --user logs ravenvoice`
shows its log. On other distributions it adds an XDG autostart entry instead.

## Limitations

- Text is typed for a **US keyboard layout**. Characters with no key on it
  (é, ñ, …) are skipped, or typed with GTK's Ctrl+Shift+U entry if
  `typing.unicode_fallback = "ctrl-shift-u"` (GTK apps only).
- Wayland only gives the clipboard to a focused window, so *Read clipboard*
  briefly takes keyboard focus to read it.
- Reading the *selected* text of another app is not possible yet: huginn has
  no primary-selection or data-control protocol.
- "Hide" shrinks the overlay instead of unmapping it. Huginn disconnects a
  client that commits to a layer surface's `wl_surface` after destroying the
  layer surface, which is what GTK does when a window is hidden.

## Development

```sh
imlazy test | check | clippy | fmt
imlazy debug             # run with debug logging
cargo build --features vulkan   # GPU Whisper (needs vulkan-headers, shaderc)
```

`.cargo/config.toml` skips bindgen (no libclang needed) and points the
whisper.cpp build at `/usr/bin/cmake`. On Raven, `/usr/sbin` comes first in
PATH and cmake started through that symlink cannot find its modules.
