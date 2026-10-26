//! `tutor-station`: started by `tutor` on Dad's PC over SSH. Speaks the
//! tutor-proto framing on stdin/stdout; logs go to stderr. Exits, putting the
//! kid's audio back, when stdin closes.

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
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use smithay_client_toolkit::reexports::calloop::{EventLoop, channel};
use tutor_proto::{FromStation, ToStation, VERSION, read_frame};

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
    replace_older();
    session_env();
    let args = parse_args()?;
    gstreamer::init()?;
    let hostname = fs::read_to_string("/etc/hostname").unwrap_or_default().trim().to_string();

    let (out, writer) = out::spawn();
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
    let mut injector = Injector::new().map_err(|e| out.status(format!("control: {e:#}"))).ok();

    let reader = {
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
                    Ok(Some(ToStation::Ping(t))) => out.send(FromStation::Pong { t, dropped: out.take_dropped() }),
                    Ok(Some(ToStation::Wake)) => {
                        if let Some(inj) = &mut injector
                            && let Err(e) = inj.wake()
                        {
                            out.status(format!("wake: {e:#}"));
                        }
                    }
                    Ok(Some(ToStation::SetMic(mic))) => {
                        if let Some(r) = route.lock().unwrap().as_mut() {
                            r.source = mic.clone();
                            media.set_source(&mic);
                        }
                        send_devices(&out, &route);
                    }
                    Ok(Some(ToStation::SetOutput(sink))) => {
                        if let Some(r) = route.lock().unwrap().as_mut() {
                            match r.set_sink(&sink) {
                                Ok(()) => media.set_sink(&sink),
                                Err(e) => out.status(format!("output: {e:#}")),
                            }
                        }
                        send_devices(&out, &route);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("tutor-station: stdin: {e}");
                        break;
                    }
                }
            }
            if let Some(inj) = &mut injector {
                let _ = inj.release_all();
            }
            let _ = loop_tx.send(LoopMsg::Quit);
        })?
    };

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
    let _ = reader.join();
    drop((out, media, loop_tx, route));
    let _ = writer.join();
    Ok(())
}

fn send_devices(out: &out::Out, route: &Mutex<Option<Route>>) {
    let (output, mic) = match route.lock().unwrap().as_ref() {
        Some(r) => (r.sink.clone(), r.source.clone()),
        None => return,
    };
    if let (Ok(outputs), Ok(mics)) = (audio_route::devices("sinks"), audio_route::devices("sources")) {
        out.send(FromStation::Devices { outputs, output, mics, mic });
    }
}

/// Follow headsets as they're plugged in and out (`pactl subscribe`).
fn watch_devices(route: Arc<Mutex<Option<Route>>>, media: Arc<Media>, out: out::Out) -> Option<Child> {
    let mut child = Command::new("pactl").arg("subscribe").stdout(Stdio::piped()).spawn().ok()?;
    let stdout = child.stdout.take()?;
    thread::Builder::new()
        .name("devices".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
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
                                eprintln!("tutor-station: output now {sink}");
                                media.set_sink(&sink);
                            }
                            if let Some(source) = source {
                                eprintln!("tutor-station: mic now {source}");
                                r.source = source.clone();
                                media.set_source(&source);
                            }
                        }
                        Err(e) => eprintln!("tutor-station: devices: {e:#}"),
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
    let Ok(out) = std::process::Command::new("pgrep").args(["-x", "tutor-station"]).output() else { return };
    let older: Vec<String> =
        String::from_utf8_lossy(&out.stdout).lines().map(str::trim).filter(|p| *p != me).map(str::to_string).collect();
    if older.is_empty() {
        return;
    }
    let _ = std::process::Command::new("kill").args(&older).status();
    // Let it tear down its audio route before we build ours.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    eprintln!("tutor-station: replaced older station {}", older.join(" "));
}
