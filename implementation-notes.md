# Implementation notes

Running log while building against [docs/plan.html](docs/plan.html).

## Facts found while building

- 2026-10-25 — COSMIC on Logan's PC (cosmic-comp 0.1~1790118748) advertises `ext_image_copy_capture_manager_v1`,
  `ext_output_image_capture_source_manager_v1`, `zwlr_layer_shell_v1` and `zwp_virtual_keyboard_manager_v1` to an
  ordinary SSH-launched client. No virtual-pointer protocol, so input goes through `/dev/uinput`
  (`crw-rw----+ root input`; `ctowles` is in `input`).
- Logan's PC GStreamer 1.24.2 has `nvh264enc` (takes BGRA/RGBA in system, GL or CUDA memory), `openh264enc`,
  `v4l2src`, `pulsesrc`; no `x264enc`.
- Dad's PC: RTX 4070, has `nvh264dec` and `avdec_h264`. GTK4 4.14.5 runtime; the `-dev` packages were missing.
- Danielle's PC: `popos-danielle.lan` still resolves to 192.168.8.214 but nothing answers there, nothing on the LAN
  except Logan's PC has port 22 open, and it isn't in mDNS. Either it's still asleep/off the network or it has no
  `openssh-server`.
- COSMIC's capture offers shm `Xbgr8888`/`Abgr8888` (plus 10-bit), not `Xrgb8888`. Both byte orders are now
  supported (RGBA vs BGRA into `nvh264enc`, no conversion).
- First H.264 frame arrives ~2 s after connect (CUDA/NVENC init); then ~30 fps while the screen moves.
- Logan's Jabra wasn't connected during testing; the station fell back to his HDMI audio and the C920's mic.
- Logan's PC was at the COSMIC lock screen after a reboot: capture works there too (you see the lock screen).
  `loginctl unlock-session` over SSH unlocked it without his password.

## Deviations

- **Deploy by hash, not version.** The plan said a `Hello` version mismatch triggers copying `tutor-station`. The app
  instead compares the SHA-256 of its local `tutor-station` with the remote one before every connect and copies on any
  difference. Stricter (catches same-version rebuilds); the version check stays as a guard.
- **Kid webcam at 640×360 MJPEG**, not 720p like `logan-cam.sh`: the sidebar is 360 px wide and 720p MJPEG is
  ~12 Mbit/s over his Wi-Fi.
- **The `tutor_apps` sink is recreated on every connect** instead of reused: a leftover loopback pointed at a headset
  that had since been unplugged, which would have silenced his apps.
- **Added an "Unlock their screen" button** (not in the plan) after the first real test found his PC locked. Runs
  `loginctl unlock-session` for his seat sessions over SSH.
- **Overlay margin 16 px** inside the free area rather than the old fixed 40,40: layer-shell positions relative to
  the panel-free area, so it lands just under the panel.
- **Hear apps is on by default** (plan: off). Dad asked to always know what the kid is hearing.
- **Added "Their sound on" and "Their mic" pickers** (not in the plan), with protocol v2 messages `ToStation::SetOutput`, `ToStation::SetMic` and
  `FromStation::Devices`. The station watches `pactl subscribe` and moves the app loopback, Dad's voice and the mic to
  the configured headset whenever it's plugged in; a vanished device falls back to another. A manual pick holds until
  the next plug event, then the headset wins again (conservative: headset is what the kid wears).
- **Loopback pinned explicitly.** `module-stream-restore` remembered the loopback stream by its stable name and put it
  on `tutor_apps` itself — a silent feedback loop, so the kid heard no app sound through the headset. The station now
  moves the loopback's stream to the real output after loading it and never sweeps it onto the app sink.
- **The first loopback fix wasn't enough.** Moving the loopback stream right after loading raced
  `module-stream-restore`, which put it back on `tutor_apps` (measured: 63% of samples clipped on their side). The
  loopback now gets its own `media.name` (`tutor-apps-to-headset`, no stale restore entry) and `sink_dont_move`, and the
  station re-checks for 2 s that it stays on the headset.
- `tools/audio-check.sh` checks "Hear apps" end to end: a 3 s 1 kHz tone at −20 dB played as an app on their PC,
  recorded on their side and as it arrives here. After the fix both sides: −23 dBFS, 0 dropouts, 0 clipped samples,
  100% tone purity.
- **Resizable layout and zoom** (not in the plan), after Dad found Logan's 3440×1440 ultrawide too small on his
  smaller monitor: the screen/sidebar split is a draggable `gtk::Paned` (sidebar starts at 360 px, scrolls if short);
  the webcam keeps 16:9 at any width; Ctrl+scroll zooms 1–4× around the pointer, drag pans, double-click fits,
  Ctrl+0/+/− and F11 full screen work while Control is off. Pointer mapping stays correct when zoomed because it uses
  the picture's own coordinates.
- **Sidebar can shrink**: its minimum width was set by unshortened labels and device names. Labels, buttons and
  dropdown buttons now ellipsize, pickers stack under their labels, card thumbnails are 72 px, and the paned may clip
  the sidebar (`shrink_end_child`) so the divider always drags further.
- **"⌨ Shortcuts (F1)" button** replaces the hint line: a popover listing every key and mouse trick.
