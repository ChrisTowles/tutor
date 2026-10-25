//! One kid's PC: an SSH child running `tutor-station`, reconnecting forever,
//! with incoming streams handed to that station's playback pipelines.

use std::{
    fs,
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};
use sha2::{Digest, Sha256};
use tutor_proto::{FromStation, ToStation, VERSION, Streams, read_frame, write_frame};

use crate::{config::StationConfig, media::Playback};

const RETRY: Duration = Duration::from_secs(10);
const REMOTE_BIN: &str = ".local/bin/tutor-station";

pub enum Event {
    Online { hostname: String, encoder: String },
    Offline(String),
    Status(String),
    /// Their voice level, 0..1.
    Level(f64),
    Devices(Devices),
}

/// Their outputs and mics as (name, description), and the ones in use.
#[derive(Default)]
pub struct Devices {
    pub outputs: Vec<(String, String)>,
    pub output: String,
    pub mics: Vec<(String, String)>,
    pub mic: String,
}

pub struct Station {
    pub config: StationConfig,
    tx: Sender<ToStation>,
    rx: Receiver<ToStation>,
    streams: Mutex<Streams>,
    online: AtomicBool,
    stopping: AtomicBool,
    child: Mutex<Option<Child>>,
    pub playback: Playback,
}

impl Station {
    pub fn new(config: StationConfig, playback: Playback) -> Arc<Self> {
        let (tx, rx) = bounded(256);
        Arc::new(Self {
            config,
            tx,
            rx,
            streams: Mutex::new(Streams::default()),
            online: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            child: Mutex::new(None),
            playback,
        })
    }

    pub fn online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// Dropped while offline or when SSH is backed up.
    pub fn send(&self, msg: ToStation) {
        if self.online()
            && let Err(TrySendError::Full(_)) = self.tx.try_send(msg)
        {
            eprintln!("tutor: {} send queue full", self.config.name);
        }
    }

    pub fn set_streams(&self, streams: Streams) {
        let mut cur = self.streams.lock().unwrap();
        if *cur != streams {
            *cur = streams;
            self.send(ToStation::Streams(streams));
        }
    }

    pub fn streams(&self) -> Streams {
        *self.streams.lock().unwrap()
    }

    pub fn start(self: &Arc<Self>, events: async_channel::Sender<Event>) {
        let me = self.clone();
        thread::Builder::new()
            .name(format!("station {}", self.config.name))
            .spawn(move || {
                while !me.stopping.load(Ordering::Relaxed) {
                    let reason = match me.session(&events) {
                        Ok(()) => "disconnected".to_string(),
                        Err(e) => format!("{e:#}"),
                    };
                    me.online.store(false, Ordering::Relaxed);
                    if me.stopping.load(Ordering::Relaxed) {
                        break;
                    }
                    let _ = events.send_blocking(Event::Offline(reason));
                    thread::sleep(RETRY);
                }
            })
            .expect("spawn station thread");
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Unlock their lock screen through logind, so nobody has to type their password.
    pub fn unlock(&self) -> Result<()> {
        let out = Command::new("ssh")
            .args(ssh_opts())
            .arg(&self.config.host)
            .arg("loginctl unlock-session; loginctl list-sessions --no-legend | awk '$4 ~ /^seat/ {print $1}' | xargs -r -n1 loginctl unlock-session")
            .stdin(Stdio::null())
            .output()?;
        anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
        Ok(())
    }

    fn session(&self, events: &async_channel::Sender<Event>) -> Result<()> {
        deploy(&self.config.host)?;
        let mut child = Command::new("ssh")
            .args(ssh_opts())
            .arg(&self.config.host)
            .arg(format!(
                "exec ~/{REMOTE_BIN} --headset {} --camera {}",
                shell_quote(&self.config.headset),
                shell_quote(&self.config.camera)
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("start ssh")?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        *self.child.lock().unwrap() = Some(child);

        // Anything queued while offline is stale.
        while self.rx.try_recv().is_ok() {}
        let alive = Arc::new(AtomicBool::new(true));
        let writer = {
            let (rx, alive, streams) = (self.rx.clone(), alive.clone(), self.streams());
            thread::spawn(move || {
                let mut w = BufWriter::new(stdin);
                let mut result = write_frame(&mut w, &ToStation::Hello { version: VERSION })
                    .and_then(|()| write_frame(&mut w, &ToStation::Streams(streams)));
                while result.is_ok() && alive.load(Ordering::Relaxed) {
                    match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(msg) => result = write_frame(&mut w, &msg),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                let _ = w.flush();
            })
        };

        let result = self.read_loop(BufReader::with_capacity(1 << 20, stdout), events);
        alive.store(false, Ordering::Relaxed);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = writer.join();
        result
    }

    fn read_loop(&self, mut r: impl std::io::Read, events: &async_channel::Sender<Event>) -> Result<()> {
        let mut level = Level::default();
        while let Some(msg) = read_frame::<FromStation>(&mut r)? {
            match msg {
                FromStation::Hello { version, hostname, width, height, encoder } => {
                    if version != VERSION {
                        bail!("station speaks v{version}, tutor v{VERSION}");
                    }
                    eprintln!("tutor: {} up: {hostname} {width}x{height} via {encoder}", self.config.name);
                    self.online.store(true, Ordering::Relaxed);
                    // Toggles changed between connecting and now would otherwise be lost.
                    let _ = self.tx.try_send(ToStation::Streams(self.streams()));
                    let _ = events.send_blocking(Event::Online { hostname, encoder });
                }
                FromStation::Video { data, .. } => self.playback.video(data),
                FromStation::KidCam(jpeg) => self.playback.cam(jpeg),
                FromStation::KidVoice(pcm) => {
                    if let Some(l) = level.feed(&pcm) {
                        let _ = events.try_send(Event::Level(l));
                    }
                    self.playback.voice(pcm);
                }
                FromStation::AppAudio(pcm) => self.playback.apps(pcm),
                FromStation::Devices { outputs, output, mics, mic } => {
                    let _ = events.send_blocking(Event::Devices(Devices { outputs, output, mics, mic }));
                }
                FromStation::Status(s) => {
                    eprintln!("tutor: {}: {s}", self.config.name);
                    let _ = events.send_blocking(Event::Status(s));
                }
            }
        }
        Ok(())
    }
}

/// Peak voice level over ~100 ms windows.
#[derive(Default)]
struct Level {
    peak: i32,
    since: Option<Instant>,
}

impl Level {
    fn feed(&mut self, pcm: &[u8]) -> Option<f64> {
        for s in pcm.chunks_exact(2) {
            self.peak = self.peak.max((i16::from_le_bytes([s[0], s[1]]) as i32).abs());
        }
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() < Duration::from_millis(100) {
            return None;
        }
        let l = self.peak as f64 / i16::MAX as f64;
        self.peak = 0;
        self.since = Some(Instant::now());
        Some(l)
    }
}

fn ssh_opts() -> [&'static str; 8] {
    ["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=2"]
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The `tutor-station` built alongside this binary.
fn local_station_bin() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let bin = exe.with_file_name("tutor-station");
    anyhow::ensure!(bin.exists(), "{} missing; build the workspace", bin.display());
    Ok(bin)
}

/// Copy our `tutor-station` to the kid's PC unless it already has this exact build.
fn deploy(host: &str) -> Result<()> {
    let bin = local_station_bin()?;
    let local = hex(&Sha256::digest(fs::read(&bin)?));
    let remote = Command::new("ssh")
        .args(ssh_opts())
        .arg(host)
        .arg(format!("sha256sum ~/{REMOTE_BIN} 2>/dev/null || true"))
        .stdin(Stdio::null())
        .output()
        .context("ssh")?;
    if !remote.status.success() {
        bail!("unreachable ({})", String::from_utf8_lossy(&remote.stderr).trim());
    }
    if String::from_utf8_lossy(&remote.stdout).split_whitespace().next() == Some(local.as_str()) {
        return Ok(());
    }
    eprintln!("tutor: installing tutor-station on {host}");
    copy_to(host, &bin)
}

fn copy_to(host: &str, bin: &Path) -> Result<()> {
    let status = Command::new("ssh")
        .args(ssh_opts())
        .arg(host)
        // Rename into place so a running copy keeps its old inode.
        .arg(format!("mkdir -p ~/.local/bin && cat > ~/{REMOTE_BIN}.new && chmod +x ~/{REMOTE_BIN}.new && mv ~/{REMOTE_BIN}.new ~/{REMOTE_BIN}"))
        .stdin(fs::File::open(bin)?)
        .status()?;
    anyhow::ensure!(status.success(), "copy tutor-station to {host} failed");
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
