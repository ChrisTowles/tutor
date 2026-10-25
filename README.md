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
cargo build --release
./target/release/tutor            # or: cargo install --path crates/app && cargo install --path crates/station
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

## Each kid's PC needs

- SSH access from Dad's PC with his key (`ssh -o BatchMode=yes <host> true` works).
- A COSMIC (or other wlroots-protocol) Wayland session logged in.
- The login user in the `input` group, for `/dev/uinput` (`sudo usermod -aG input <user>`, then log out and in).
- GStreamer 1.24 base/good/bad plugins (stock on Pop!_OS 24.04).
