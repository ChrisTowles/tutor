//! One kid's PC: an SSH child running `tutor-station`, reconnecting forever,
//! with incoming streams handed to that station's playback pipelines.

use std::{
    fs,
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};
use sha2::{Digest, Sha256};
use tutor_proto::{Presence, FromStation, ToStation, VERSION, Streams, read_frame, rss_kb, write_frame};

use crate::{config::StationConfig, media::Playback};

const RETRY: Duration = Duration::from_secs(10);
const PING_EVERY: Duration = Duration::from_millis(500);
/// Round trip above this, or any dropped frame, counts as laggy.
pub const LAGGY_MS: u32 = 150;
/// No reply for this long counts as stalled.
pub const STALLED_MS: u32 = 1500;
const REMOTE_BIN: &str = ".local/bin/tutor-station";

pub enum Event {
    Online { hostname: String, encoder: String },
    Offline(String),
    Status(String),
    /// Their voice level, 0..1.
    Level(f64),
    Devices(Devices),
    Presence(Presence),
    Link(Link),
}

/// How the connection is doing, every `PING_EVERY`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Link {
    /// Last round trip, through every queue on both sides.
    pub rtt_ms: u32,
    /// How long since the last reply.
    pub silent_ms: u32,
    pub fps: u32,
    /// Their screen coming in.
    pub kbps: u32,
    /// Their webcam coming in.
    pub cam_kbps: u32,
    /// My webcam going out to them.
    pub my_cam_kbps: u32,
    /// Screen frames the station dropped since the last report.
    pub dropped: u32,
    /// Resident memory of `tutor-station` on their PC.
    pub their_rss_mb: u32,
    /// Resident memory of this `tutor` process (every station together).
    pub my_rss_mb: u32,
}

impl Link {
    pub fn stalled(&self) -> bool {
        self.silent_ms >= STALLED_MS
    }

    pub fn laggy(&self) -> bool {
        self.stalled() || self.rtt_ms >= LAGGY_MS || self.dropped > 0
    }
}

/// Counters the reader fills and the pinger reports.
#[derive(Default)]
struct Stats {
    frames: AtomicU32,
    bytes: AtomicU64,
    cam_bytes: AtomicU64,
    my_cam_bytes: AtomicU64,
    dropped: AtomicU32,
    their_rss_kb: AtomicU32,
    rtt_us: AtomicU64,
    /// Microseconds since `Station::epoch` of the last pong.
    pong_at: AtomicU64,
}

/// Their outputs and mics as (name, description), the ones in use (empty when
/// there's none) and the output's volume in percent.
#[derive(Default)]
pub struct Devices {
    pub outputs: Vec<(String, String)>,
    pub output: String,
    pub mics: Vec<(String, String)>,
    pub mic: String,
    pub volume: u8,
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
    epoch: Instant,
    stats: Stats,
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
            epoch: Instant::now(),
            stats: Stats::default(),
        })
    }

    pub fn online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// Dropped while offline or when SSH is backed up.
    pub fn send(&self, msg: ToStation) {
        if !self.online() {
            return;
        }
        let cam_len = match &msg {
            ToStation::DadCam(jpeg) => jpeg.len() as u64,
            _ => 0,
        };
        match self.tx.try_send(msg) {
            Ok(()) => {
                self.stats.my_cam_bytes.fetch_add(cam_len, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) => tracing::warn!(station = %self.config.name, "station.send_queue_full"),
            Err(TrySendError::Disconnected(_)) => {}
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
                    tracing::warn!(station = %me.config.name, %reason, "station.disconnected");
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

    fn session(self: &Arc<Self>, events: &async_channel::Sender<Event>) -> Result<()> {
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

        let pinger = self.spawn_pinger(alive.clone(), events.clone());
        let result = self.read_loop(BufReader::with_capacity(1 << 20, stdout), events);
        alive.store(false, Ordering::Relaxed);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = writer.join();
        let _ = pinger.join();
        result
    }

    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    /// Pings through the same queues as everything else, so a backed-up link
    /// shows as a long round trip, and reports a `Link` each time.
    fn spawn_pinger(self: &Arc<Self>, alive: Arc<AtomicBool>, events: async_channel::Sender<Event>) -> thread::JoinHandle<()> {
        let me = self.clone();
        thread::spawn(move || {
            // 0 until the first pong: the station's startup isn't lag.
            me.stats.pong_at.store(0, Ordering::Relaxed);
            let mut last = Instant::now();
            let mut was_laggy = false;
            while alive.load(Ordering::Relaxed) {
                thread::sleep(PING_EVERY);
                let now = me.now_us();
                if !me.online() {
                    continue;
                }
                let _ = me.tx.try_send(ToStation::Ping(now));
                let pong_at = me.stats.pong_at.load(Ordering::Relaxed);
                if pong_at == 0 {
                    continue;
                }
                let secs = last.elapsed().as_secs_f64().max(0.001);
                last = Instant::now();
                let kbps = |bytes: &AtomicU64| (bytes.swap(0, Ordering::Relaxed) as f64 * 8.0 / 1000.0 / secs) as u32;
                let link = Link {
                    rtt_ms: (me.stats.rtt_us.load(Ordering::Relaxed) / 1000) as u32,
                    silent_ms: (now.saturating_sub(pong_at) / 1000) as u32,
                    fps: (me.stats.frames.swap(0, Ordering::Relaxed) as f64 / secs).round() as u32,
                    kbps: kbps(&me.stats.bytes),
                    cam_kbps: kbps(&me.stats.cam_bytes),
                    my_cam_kbps: kbps(&me.stats.my_cam_bytes),
                    dropped: me.stats.dropped.swap(0, Ordering::Relaxed),
                    their_rss_mb: me.stats.their_rss_kb.load(Ordering::Relaxed) / 1024,
                    my_rss_mb: rss_kb() / 1024,
                };
                // An event per lag episode, for working out what caused it later.
                if link.laggy() != was_laggy {
                    was_laggy = link.laggy();
                    let name = if was_laggy { "link.lag" } else { "link.ok" };
                    tracing::warn!(station = %me.config.name, rtt_ms = link.rtt_ms, silent_ms = link.silent_ms, dropped = link.dropped, "{name}");
                }
                // Two a second per PC: too many for the log unless asked for.
                if std::env::var_os("TUTOR_LINK_LOG").is_some() {
                    tracing::info!(station = %me.config.name, ?link, "link.sample");
                }
                let _ = events.try_send(Event::Link(link));
            }
        })
    }

    fn read_loop(&self, mut r: impl std::io::Read, events: &async_channel::Sender<Event>) -> Result<()> {
        let mut level = Level::default();
        let mut last_status: Option<String> = None;
        while let Some(msg) = read_frame::<FromStation>(&mut r)? {
            match msg {
                FromStation::Hello { version, hostname, width, height, encoder } => {
                    if version != VERSION {
                        bail!("station speaks v{version}, tutor v{VERSION}");
                    }
                    tracing::info!(station = %self.config.name, %hostname, width, height, %encoder, "station.connected");
                    self.online.store(true, Ordering::Relaxed);
                    // Toggles changed between connecting and now would otherwise be lost.
                    let _ = self.tx.try_send(ToStation::Streams(self.streams()));
                    let _ = events.send_blocking(Event::Online { hostname, encoder });
                }
                FromStation::Video { data, .. } => {
                    self.stats.frames.fetch_add(1, Ordering::Relaxed);
                    self.stats.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                    self.playback.video(data)
                }
                FromStation::Pong { t, dropped, rss_kb } => {
                    let now = self.now_us();
                    self.stats.rtt_us.store(now.saturating_sub(t), Ordering::Relaxed);
                    self.stats.pong_at.store(now, Ordering::Relaxed);
                    self.stats.dropped.fetch_add(dropped, Ordering::Relaxed);
                    self.stats.their_rss_kb.store(rss_kb, Ordering::Relaxed);
                }
                FromStation::KidCam(jpeg) => {
                    self.stats.cam_bytes.fetch_add(jpeg.len() as u64, Ordering::Relaxed);
                    self.playback.cam(jpeg)
                }
                FromStation::KidVoice(pcm) => {
                    if let Some(l) = level.feed(&pcm) {
                        let _ = events.try_send(Event::Level(l));
                    }
                    self.playback.voice(pcm);
                }
                FromStation::AppAudio(pcm) => self.playback.apps(pcm),
                FromStation::Devices { outputs, output, mics, mic, volume } => {
                    let _ = events.send_blocking(Event::Devices(Devices { outputs, output, mics, mic, volume }));
                }
                FromStation::Presence(p) => {
                    let _ = events.send_blocking(Event::Presence(p));
                }
                // An old station could send one error per frame; showing each froze the window.
                FromStation::Status(s) if last_status.as_deref() == Some(s.as_str()) => {}
                FromStation::Status(s) => {
                    last_status = Some(s.clone());
                    tracing::warn!(station = %self.config.name, status = %s, "station.status");
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
    copy_to(host, &bin)?;
    tracing::info!(%host, sha256 = %local, "station.deployed");
    Ok(())
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
