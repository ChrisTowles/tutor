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
- **Presence: locked / asleep / idle** (not in the plan), after Dad couldn't tell a connected-but-asleep PC from a
  broken one. Protocol v3 adds `FromStation::Presence` (logind `LockedHint` polled every 3 s, wlr output power for
  display off, `ext_idle_notifier` at 60 s) and `ToStation::Wake` (a Shift tap). While locked or asleep a banner over
  the screen says so with a **Wake & unlock** button; the sidebar button does the same. The kid's card shows
  "🔒 locked", "💤 screen asleep" or "idle".
- **One station per PC.** An orphaned station from an earlier session was still running (holding the camera and
  audio route). A new station now kills any older `tutor-station` before starting.
- **Connection health pill** (not in the plan), after Dad saw lag with nothing on screen saying so. Protocol v4 adds
  `ToStation::Ping` / `FromStation::Pong { dropped }`: the app pings every 500 ms through the same queues as
  everything else, so a backed-up link shows as a long round trip. Bottom-right of the screen view:
  "● 3 ms · 30 fps · 2.1 Mb/s", amber "⚠ Laggy" at ≥150 ms or any frame the station dropped, red "⚠ Stalled" after
  1.5 s with no reply. The card shows "⚠ laggy"/"⚠ stalled". Each lag episode is logged (`LAG` / `ok again`);
  `TUTOR_LINK_LOG=1` logs every sample. First minute on Logan's PC: median 2 ms, p90 4 ms, one 465 ms spike.
- **Lag cause found: Wi-Fi power saving on Logan's PC.** Both PCs are on Wi-Fi (same 5 GHz AP, channel 36). Logan's
  MediaTek MT7925 has power saving **on**: `/etc/NetworkManager/conf.d/99-wifi-powersave-off.conf` (=2) sorts
  *before* `default-wifi-powersave-on.conf` (=3), so the "on" file wins (`NetworkManager --print-config` →
  `wifi.powersave=3`). Dad's PC has it off. The stall pattern fits: ~25 s with round trips of 1–3.4 s at only
  ~1–4 Mbit/s (not bandwidth), no frames lost to the encoder. Fix needs sudo on his PC.
- **Screen quality picker** (not in the plan): Full / Half / Low per PC, protocol v5 `Streams::screen_scale`. The
  station scales on the CPU (`videoscale n-threads=4`) before NVENC and divides the bitrate by the same factor.
  GPU scaling via `glcolorscale` into `nvh264enc` failed to negotiate on his PC; CPU scaling of 3440×1440 at 30 fps
  measured ~⅔ of one core.
- **Movable cam overlay** (plan: fixed top-left). Protocol v6 `Streams::dad_cam_at` is where the overlay's centre goes
  in their screen's 0..ABS_MAX space; the station turns it into layer-shell margins (kept 16 px inside the edges)
  from the output's logical size. In the app, Shift+drag on their screen moves it live (you see it move in their
  stream); remembered per PC for the app's lifetime. Half quality confirmed working by Dad.
- **Settings survive restarts** in `~/.config/tutor/settings.toml` (app-written; `tutor.toml` stays hand-edited):
  selected PC, Talk, Hear apps, sidebar width, and per PC (by name) screen quality, My cam and its position. Written
  500 ms after the last change and on close. Control is never restored (always starts off). A missing or broken file
  falls back to defaults.
- **Lockup during Split Fiction: GPU memory full.** The game used 7.3 of the RTX 5060's 8 GB. Switching to Low rebuilt
  the encoder; NVENC couldn't start and `nvh264enc` aborted (`gst_nv_base_enc_set_format: code should not be
  reached`), killing the station. The new station picked `nvh264enc` because the registry still listed it, but
  CUDA couldn't start in that process, so every build failed ("no property preset"). The station retried on every
  frame: 16k errors to Dad (the window froze handling them) and a leaked socket pair per attempt until it hit 1024
  open files. Fixes: the station probes NVENC by creating the element and drops to `openh264enc` when a build fails;
  a failed build waits 5 s before retrying; repeated status messages are dropped on both sides. Low quality on the
  CPU encoder runs at ~9% CPU on his PC.
- **"Off" screen quality.** Stops screen capture (fps 0) and tears down the encoder so a game keeps every frame and
  its GPU memory; voice, webcam and Dad's cam keep running. The screen view shows a "Screen sharing is off" banner.
  Turning it back on re-probes NVENC, since the GPU may have room again.
- **Webcams at 10 fps, and the pill shows what they cost.** Both webcams dropped from 15 to 10 fps (`CAM_FPS` in
  `tutor-proto`). Protocol v7 adds `rss_kb` to `Pong`. The health pill now reads
  "● 3 ms · 30 fps · screen 2.1 Mb/s · cam 1.2 in, 0.8 out" over "memory: theirs 180 MB · tutor 420 MB": their
  webcam in, my webcam out to that PC, `tutor-station`'s resident memory there and this `tutor`'s here. The memory
  figures are whole processes, since GStreamer can't give a per-pipeline number.
- **Session timer.** A "⏱ Start timer" button under Wake & unlock: click to start from 0:00, click to stop, and it
  keeps showing "Took 42:10" until the next start. One timer for the whole window, not per PC; not saved.
- **Event log, modelled on towles-tool's telemetry.** New `tutor-telemetry` crate: a `tracing` subscriber with a
  stderr layer and a JSONL event-log layer (spans written on close with `duration_ms`, daily files, 14 kept, panic
  hook → `app.crash`). Writer only: towles-tool's reader, Attention and Rules screens aren't ported, `jq` covers it
  for now. Every `eprintln!` in both binaries became a named event. The station logs on the kid's PC; its stderr
  still reaches Dad's terminal through SSH.
- **"Their sound on" stuck at (None) on Logan's PC.** The station started while his Jabra was unplugged and his
  monitor (the HDMI output) asleep, so PipeWire offered only its `auto_null` placeholder. `Route::setup` picked it,
  the loopback couldn't be moved onto it ("loopback won't stay on auto_null"), and setup failed for the whole
  session: no device list for the pickers, no device watching, and "Hear apps" silent. `auto_null` is no longer a
  device, and a missing output or mic no longer fails setup: the app sink still goes up and the watcher picks the
  device when it appears. Dad's voice and their mic report "no output"/"no microphone" until then.
- **A stopped station puts their audio back.** SIGTERM, SIGINT and SIGHUP now take the same exit as stdin closing
  (`signal-hook`): media stopped, audio route torn down, `station.stopped` logged with the `signal`. Before, a kill
  left `tutor_apps` as their default sink. `replace_older` relied on that teardown without getting it; it now waits
  up to 5 s for the older station to exit, then SIGKILLs it (`stuck` in `station.replaced_older`), so the older
  teardown can't unload the newer station's `tutor_apps`.
- **Problems stay on the card until fixed.** Under each PC's name, in amber: "🔇 no speakers", "🎧 headset unplugged"
  (its `headset` pattern is set but the output isn't it), "🎙 no mic", "📷 camera not working". Worked out in the app
  from the `Devices` the station already sends; the camera one is set by a `camera:` status and cleared by the next
  webcam frame. Protocol v8.
- **Pointing.** With Control off, a left click or drag on their screen puts a red dot (56 px, white ring) there; it
  stays 1.5 s after the button comes up. It's a second layer-shell overlay with an empty input region, positioned by
  margins from the screen's corner (`exclusive_zone -1`, so their panel doesn't offset it). Panning while zoomed
  moved from left-drag to right- or middle-drag; Shift+left-drag still moves my cam.
- **Their volume.** A slider under "Their sound on" sets their output sink's volume (`pactl set-sink-volume`), sent
  at most every 100 ms while dragging and logged once it settles. `Devices` carries the volume, and the station
  re-sends it when their sink changes volume, so their own volume keys move the slider.
- **The station exits after teardown instead of joining its threads.** The presence and lock watchers hold the
  writer open forever, so a station whose stdin closed but whose stdout didn't (SSH still up) used to hang after
  restoring the audio.
