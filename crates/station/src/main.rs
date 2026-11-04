//! `tutor-station`: started by `tutor` on Dad's PC over SSH. Speaks the
//! tutor-proto framing on stdin/stdout; logs go to stderr. Exits, putting the
//! kid's audio back, when stdin closes or it's told to stop (SIGTERM, SIGINT,
//! SIGHUP).

mod audio_route;
mod input;
mod media;
mod out;
mod presence;
mod wayland;

use std::{
    env, fs,
    io::{self, BufRead, BufReader},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI32, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGTERM},
    iterator::Signals,
};
use smithay_client_toolkit::reexports::calloop::{EventLoop, channel};
use tutor_proto::{FromStation, ToStation, VERSION, read_frame, rss_kb};

use crate::{
    audio_route::Route,
    input::Injector,
    media::Media,
    wayland::{LoopMsg, Wl},
};

struct Args {
    /// Substring of their headset's PulseAudio name; empty = their defaults.
    headset: String,
    camera: String,
}

fn parse_args() -> Result<Args> {
    let mut args = Args { headset: String::new(), camera: "/dev/video0".into() };
    let mut it = env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next().with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--headset" => args.headset = value,
            "--camera" => args.camera = value,
            _ => anyhow::bail!("unknown flag {flag}"),
        }
    }
    Ok(args)
}

/// SSH sessions don't carry the desktop's environment; find it.
fn session_env() {
    let runtime = env::var("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|_| {
        let uid = fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(1000);
        PathBuf::from(format!("/run/user/{uid}"))
    });
    let wayland = env::var_os("WAYLAND_DISPLAY").is_none().then(|| {
        let mut names: Vec<String> = fs::read_dir(&runtime)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
            .collect();
        names.sort();
        names.pop()
    });
    // SAFETY: called first thing in main, before any other thread exists.
    unsafe {
        env::set_var("XDG_RUNTIME_DIR", &runtime);
        if let Some(Some(socket)) = wayland {
            env::set_var("WAYLAND_DISPLAY", socket);
        }
    }
}

fn main() -> Result<()> {
    tutor_telemetry::init("tutor-station");
    replace_older();
    session_env();
    let args = parse_args()?;
    gstreamer::init()?;
    let hostname = fs::read_to_string("/etc/hostname").unwrap_or_default().trim().to_string();
    tracing::info!(headset = %args.headset, camera = %args.camera, "station.started");

    let (out, _writer) = out::spawn();
    let route = Arc::new(Mutex::new(Route::setup(&args.headset).map_err(|e| out.status(format!("audio: {e:#}"))).ok()));
    let (loop_tx, loop_rx) = channel::channel::<LoopMsg>();
    let (sink, source) =
        route.lock().unwrap().as_ref().map(|r| (r.sink.clone(), r.source.clone())).unwrap_or_default();
    let media = Arc::new(Media::new(out.clone(), loop_tx.clone(), sink, source, args.camera));
    send_devices(&out, &route);
    let presence = Arc::new(presence::Reporter::new(out.clone()));
    presence.announce();
    presence::watch_lock(presence.clone());
    let watcher = watch_devices(route.clone(), media.clone(), out.clone());
    let signalled = Arc::new(AtomicI32::new(0));
    {
        let (signalled, loop_tx) = (signalled.clone(), loop_tx.clone());
        let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP])?;
        thread::Builder::new().name("signals".into()).spawn(move || {
            if let Some(sig) = signals.forever().next() {
                signalled.store(sig, Ordering::Relaxed);
                let _ = loop_tx.send(LoopMsg::Quit);
            }
        })?;
    }
    let mut injector = Injector::new().map_err(|e| out.status(format!("control: {e:#}"))).ok();

    {
        let (media, out, loop_tx, route) = (media.clone(), out.clone(), loop_tx.clone(), route.clone());
        thread::Builder::new().name("reader".into()).spawn(move || {
            let mut stdin = io::stdin().lock();
            loop {
                match read_frame::<ToStation>(&mut stdin) {
                    Ok(Some(ToStation::Hello { version })) if version != VERSION => {
                        out.status(format!("tutor speaks v{version}, station v{VERSION}"));
                        break;
                    }
                    Ok(Some(ToStation::Hello { .. })) => {}
                    Ok(Some(ToStation::Streams(s))) => media.apply(s),
                    Ok(Some(ToStation::Input(i))) => {
                        if let Some(inj) = &mut injector
                            && let Err(e) = inj.apply(i)
                        {
                            out.status(format!("control: {e:#}"));
                        }
                    }
                    Ok(Some(ToStation::DadVoice(pcm))) => media.dad_voice(pcm),
                    Ok(Some(ToStation::DadCam(jpeg))) => media.dad_cam(jpeg),
                    Ok(Some(ToStation::Keyframe)) => media.force_keyframe(),
                    Ok(Some(ToStation::Ping(t))) => out.send(FromStation::Pong { t, dropped: out.take_dropped(), rss_kb: rss_kb() }),
                    Ok(Some(ToStation::Wake)) => {
                        if let Some(inj) = &mut injector
                            && let Err(e) = inj.wake()
                        {
                            out.status(format!("wake: {e:#}"));
                        }
                    }
                    Ok(Some(ToStation::SetMic(mic))) => {
                        // Only a mic they have: the name goes into pactl and a gst pipeline string.
                        if !audio_route::is_device("sources", &mic) {
                            out.status(format!("mic: no device {mic:?}"));
                        } else if let Some(r) = route.lock().unwrap().as_mut() {
                            r.source = mic.clone();
                            media.set_source(&mic);
                        }
                        send_devices(&out, &route);
                    }
                    Ok(Some(ToStation::SetVolume(percent))) => {
                        let sink = route.lock().unwrap().as_ref().map(|r| r.sink.clone()).unwrap_or_default();
                        if !sink.is_empty()
                            && let Err(e) = audio_route::set_volume(&sink, percent)
                        {
                            out.status(format!("volume: {e:#}"));
                        }
                        send_devices(&out, &route);
                    }
                    Ok(Some(ToStation::Point(at))) => {
                        let _ = loop_tx.send(LoopMsg::Point(at));
                    }
                    Ok(Some(ToStation::SetOutput(sink))) => {
                        // Only an output they have: the name goes into pactl module args.
                        if !audio_route::is_device("sinks", &sink) {
                            out.status(format!("output: no device {sink:?}"));
                        } else if let Some(r) = route.lock().unwrap().as_mut() {
                            match r.set_sink(&sink) {
                                Ok(()) => media.set_sink(&sink),
                                Err(e) => out.status(format!("output: {e:#}")),
                            }
                        }
                        send_devices(&out, &route);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "stdin.failed");
                        break;
                    }
                }
            }
            if let Some(inj) = &mut injector {
                let _ = inj.release_all();
            }
            let _ = loop_tx.send(LoopMsg::Quit);
        })?;
    }

    let mut event_loop: EventLoop<'static, Wl> = EventLoop::try_new()?;
    match Wl::connect(event_loop.handle(), media.clone(), out.clone(), hostname.clone(), presence.clone()) {
        Ok(mut wl) => {
            event_loop
                .handle()
                .insert_source(loop_rx, |event, _, wl: &mut Wl| match event {
                    channel::Event::Msg(msg) => wl.handle(msg),
                    channel::Event::Closed => wl.exit = true,
                })
                .map_err(|e| anyhow::anyhow!("watch commands: {e}"))?;
            while !wl.exit {
                event_loop.dispatch(None, &mut wl)?;
            }
        }
        Err(e) => {
            // No screen, but voice and cameras still work.
            out.send(FromStation::Hello {
                version: VERSION,
                hostname,
                width: 0,
                height: 0,
                encoder: media.encoder().to_string(),
            });
            out.status(format!("screen: {e:#}"));
            let mut fallback: EventLoop<'static, bool> = EventLoop::try_new()?;
            fallback
                .handle()
                .insert_source(loop_rx, |event, _, quit: &mut bool| {
                    *quit |= matches!(event, channel::Event::Closed | channel::Event::Msg(LoopMsg::Quit));
                })
                .map_err(|e| anyhow::anyhow!("watch commands: {e}"))?;
            let mut quit = false;
            while !quit {
                fallback.dispatch(None, &mut quit)?;
            }
        }
    }

    if let Some(mut w) = watcher {
        let _ = w.kill();
        let _ = w.wait();
    }
    media.stop_all();
    if let Some(route) = route.lock().unwrap().as_ref() {
        route.teardown();
    }
    // Exit rather than join: after a signal the reader is still blocked on
    // stdin, and the presence and lock watchers hold the writer open for good.
    // Held keys go with the uinput devices.
    let signal = signalled.load(Ordering::Relaxed);
    tracing::info!(signal, "station.stopped");
    std::process::exit(0)
}

fn send_devices(out: &out::Out, route: &Mutex<Option<Route>>) {
    let (output, mic, volume) = match route.lock().unwrap().as_mut() {
        Some(r) => {
            r.volume = audio_route::volume(&r.sink).unwrap_or(0);
            (r.sink.clone(), r.source.clone(), r.volume)
        }
        None => return,
    };
    if let (Ok(outputs), Ok(mics)) = (audio_route::devices("sinks"), audio_route::devices("sources")) {
        out.send(FromStation::Devices { outputs, output, mics, mic, volume });
    }
}

/// Follow headsets as they're plugged in and out, and volume changes they
/// make (`pactl subscribe`).
fn watch_devices(route: Arc<Mutex<Option<Route>>>, media: Arc<Media>, out: out::Out) -> Option<Child> {
    let mut child = Command::new("pactl").arg("subscribe").stdout(Stdio::piped()).spawn().ok()?;
    let stdout = child.stdout.take()?;
    thread::Builder::new()
        .name("devices".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.contains("'change' on sink #") {
                    let changed = route
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|r| audio_route::volume(&r.sink).is_some_and(|v| v != r.volume));
                    if changed {
                        send_devices(&out, &route);
                    }
                    continue;
                }
                let device = line.contains(" on sink #") || line.contains(" on source #");
                if !device || !(line.contains("'new'") || line.contains("'remove'")) {
                    continue;
                }
                // Let the rest of the device's events land first.
                thread::sleep(Duration::from_millis(700));
                {
                    let mut guard = route.lock().unwrap();
                    let Some(r) = guard.as_mut() else { continue };
                    match r.wanted() {
                        Ok((sink, source)) => {
                            if let Some(sink) = sink
                                && r.set_sink(&sink).is_ok()
                            {
                                tracing::info!(%sink, "output.changed");
                                media.set_sink(&sink);
                            }
                            if let Some(source) = source {
                                tracing::info!(%source, "mic.changed");
                                r.source = source.clone();
                                media.set_source(&source);
                            }
                        }
                        Err(e) => tracing::warn!(error = format!("{e:#}"), "devices.failed"),
                    }
                }
                send_devices(&out, &route);
            }
        })
        .ok()?;
    Some(child)
}

/// One station per PC: an older one whose SSH session died can linger holding
/// the camera, the overlay and the audio route. The newest connection wins.
fn replace_older() {
    let me = std::process::id().to_string();
    // Ours only: another user's station isn't ours to stop.
    let Ok(uid) = fs::metadata("/proc/self").map(|m| m.uid().to_string()) else { return };
    let running = || -> Vec<String> {
        let Ok(out) = Command::new("pgrep").args(["-x", "-u", &uid, "tutor-station"]).output() else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout).lines().map(str::trim).filter(|p| *p != me).map(str::to_string).collect()
    };
    let older = running();
    if older.is_empty() {
        return;
    }
    let _ = Command::new("kill").args(&older).status();
    // Let it tear down its audio route before we build ours: its teardown
    // unloads every tutor_apps module, ours included if we went first.
    let alive = || older.iter().filter(|p| fs::exists(format!("/proc/{p}")).unwrap_or(false)).count();
    for _ in 0..50 {
        if alive() == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    // Look again rather than trust 5 s old PIDs: one may since belong to something else.
    let stuck: Vec<String> = running().into_iter().filter(|p| older.contains(p)).collect();
    if !stuck.is_empty() {
        let _ = Command::new("kill").arg("-KILL").args(&stuck).status();
    }
    tracing::info!(pids = older.join(" "), stuck = stuck.join(" "), "station.replaced_older");
}
