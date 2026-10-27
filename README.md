# tutor

One window on Dad's PC to see, hear, talk to and drive the kids' PCs during tutoring.

- **`tutor`** (`crates/app`) — the GTK4 window. Shows the selected PC's screen big, their webcam on the side,
  four toggles (Talk, Hear apps, My cam, Control) and one card per PC to switch between them.
- **`tutor-station`** (`crates/station`) — runs on each kid's PC. `tutor` starts it over SSH and copies a fresh
  build there whenever the local one changes. Captures the screen through COSMIC's `ext-image-copy-capture`,
  encodes with NVENC (or openh264), streams their webcam/mic/app sound, plays Dad's voice, shows Dad's camera as a
  click-through overlay, and replays Dad's mouse/keyboard through `/dev/uinput`.
- **`tutor-proto`** (`crates/proto`) — the framed messages between them.

The design and the decisions behind it are in [docs/plan.html](docs/plan.html); what changed while building is in
[implementation-notes.md](implementation-notes.md).

## Build and run

Needs `libgtk-4-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev` on this PC.

```bash
just run                          # builds both binaries, then starts tutor
just build                        # build only
# or: cargo install --path crates/app && cargo install --path crates/station
```

Both binaries must sit in the same directory: `tutor` deploys the `tutor-station` next to it.

## Config

`~/.config/tutor/tutor.toml`, written with Logan's and Danielle's PCs on first run:

```toml
[me]
camera = "/dev/video0"
mic = "Jabra"        # substring of a PulseAudio source; "" = default
speakers = ""        # substring of a sink; "" = default

[[station]]
name = "Logan's PC"
host = "192.168.8.106"
headset = "Jabra"
camera = "/dev/video0"
```

`~/.config/tutor/settings.toml` is written by the app and remembers the last layout: which PC was on screen, Talk,
Hear apps, sidebar width, and per PC the screen quality and where your cam sits. Delete it to reset.

## Event log

Both binaries log through `tracing` (`crates/telemetry`): to stderr, and to `~/.local/share/tutor/telemetry/events-<date>.jsonl`
on the PC they run on, one JSON object per line, UTC days, 14 kept. `just events` follows today's log on this PC.
`RUST_LOG` only changes stderr; the file always gets our crates at `debug`. `TUTOR_TELEMETRY=0` turns the file off.

The rules, so the log can answer questions later without a live repro:

- **Every user action emits a `ui.action` event** with a stable `action` id (`talk`, `select`, `quality`, `timer_stop`, …),
  the `station` it applies to, and the outcome fields that matter (`on`, `seconds`, `device`).
- **Every change the app didn't make itself is an event named for what changed**, `noun.verb`: `station.connected`,
  `station.disconnected`, `link.lag` / `link.ok`, `station.deployed`, `audio.routed`. A panic is an `app.crash` record.
- **Never log content or continuous input**: no keystrokes, mouse moves, zoom steps, audio or frames. `TUTOR_LINK_LOG=1`
  adds the twice-a-second `link.sample` when you need it.

```bash
jq -c 'select(.message == "ui.action" and .action == "timer_stop") | {ts, seconds}' ~/.local/share/tutor/telemetry/events-*.jsonl
```

## Each kid's PC needs

- SSH access from Dad's PC with his key (`ssh -o BatchMode=yes <host> true` works).
- A COSMIC (or other wlroots-protocol) Wayland session logged in.
- The login user in the `input` group, for `/dev/uinput` (`sudo usermod -aG input <user>`, then log out and in).
- GStreamer 1.24 base/good/bad plugins (stock on Pop!_OS 24.04).
