//! GStreamer pipelines on the kid's PC: screen encoder, their camera and mic,
//! their app sound, and Dad's voice and camera coming the other way. Each one
//! runs only while its stream is switched on.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use tutor_proto::{CAM_FPS, FromStation, Streams};

use crate::{audio_route::APP_SINK, out::Out, wayland::LoopMsg};

/// Size of Dad's camera overlay on the kid's screen.
/// How long to wait before building a failed screen pipeline again.
const RETRY_BUILD: std::time::Duration = std::time::Duration::from_secs(5);

pub const DAD_CAM_SIZE: (u32, u32) = (480, 270);

const VOICE_CAPS: &str = "audio/x-raw,format=S16LE,rate=48000,channels=1,layout=interleaved";
const APP_CAPS: &str = "audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved";

pub struct Media {
    out: Out,
    loop_tx: Sender<LoopMsg>,
    headset_sink: Mutex<String>,
    headset_source: Mutex<String>,
    camera: String,
    /// nvh264enc, or openh264enc when NVENC can't start (e.g. a game has
    /// filled the GPU's memory). Can drop to openh264enc mid-session.
    encoder: Mutex<&'static str>,
    /// When the screen pipeline last failed to build; no retry for `RETRY_BUILD`.
    screen_failed: Mutex<Option<std::time::Instant>>,
    streams: Mutex<Streams>,
    screen: Mutex<Option<Screen>>,
    /// Drop delta frames until the next keyframe (one was lost).
    waiting_key: Arc<AtomicBool>,
    kid_cam: Mutex<Option<gst::Pipeline>>,
    kid_voice: Mutex<Option<gst::Pipeline>>,
    app_audio: Mutex<Option<gst::Pipeline>>,
    dad_voice: Mutex<Option<(gst::Pipeline, gst_app::AppSrc)>>,
    dad_cam: Mutex<Option<(gst::Pipeline, gst_app::AppSrc)>>,
}

struct Screen {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    sink: gst_app::AppSink,
    size: (u32, u32),
    format: &'static str,
    scale: u32,
}

impl Media {
    pub fn new(out: Out, loop_tx: Sender<LoopMsg>, headset_sink: String, headset_source: String, camera: String) -> Self {
        let encoder = pick_encoder();
        Self {
            out,
            loop_tx,
            headset_sink: Mutex::new(headset_sink),
            headset_source: Mutex::new(headset_source),
            camera,
            encoder: Mutex::new(encoder),
            screen_failed: Mutex::new(None),
            streams: Mutex::new(Streams::default()),
            screen: Mutex::new(None),
            waiting_key: Arc::new(AtomicBool::new(false)),
            kid_cam: Mutex::new(None),
            kid_voice: Mutex::new(None),
            app_audio: Mutex::new(None),
            dad_voice: Mutex::new(None),
            dad_cam: Mutex::new(None),
        }
    }

    pub fn apply(&self, next: Streams) {
        let prev = std::mem::replace(&mut *self.streams.lock().unwrap(), next);
        tracing::debug!(?next, "streams.applied");
        if prev.screen_fps != next.screen_fps {
            let _ = self.loop_tx.send(LoopMsg::Fps(next.screen_fps));
            if next.screen_fps == 0 {
                // Screen off: free the encoder and its GPU memory for their game.
                if let Some(s) = self.screen.lock().unwrap().take() {
                    let _ = s.pipeline.set_state(gst::State::Null);
                }
            } else if prev.screen_fps == 0 {
                // Back on: the GPU may have room again, so try NVENC afresh.
                *self.encoder.lock().unwrap() = pick_encoder();
                *self.screen_failed.lock().unwrap() = None;
            }
        }
        if prev.kid_cam != next.kid_cam {
            self.toggle(&self.kid_cam, next.kid_cam, "camera", || self.build_kid_cam());
        }
        if prev.kid_voice != next.kid_voice {
            self.toggle(&self.kid_voice, next.kid_voice, "mic", || self.build_kid_voice());
        }
        if prev.app_audio != next.app_audio {
            self.toggle(&self.app_audio, next.app_audio, "app sound", || self.build_app_audio());
        }
        if prev.dad_voice != next.dad_voice {
            self.toggle_src(&self.dad_voice, next.dad_voice, "Dad's voice", || self.build_dad_voice());
        }
        if prev.dad_cam_at != next.dad_cam_at {
            let _ = self.loop_tx.send(LoopMsg::DadCamAt(next.dad_cam_at));
        }
        if prev.dad_cam != next.dad_cam {
            self.toggle_src(&self.dad_cam, next.dad_cam, "Dad's camera", || self.build_dad_cam());
            if !next.dad_cam {
                let _ = self.loop_tx.send(LoopMsg::DadCam(None));
            }
        }
    }

    /// Their output changed: Dad's voice follows it.
    pub fn set_sink(&self, sink: &str) {
        *self.headset_sink.lock().unwrap() = sink.to_string();
        if self.streams.lock().unwrap().dad_voice {
            self.toggle_src(&self.dad_voice, true, "Dad's voice", || self.build_dad_voice());
        }
    }

    /// Their mic changed.
    pub fn set_source(&self, source: &str) {
        *self.headset_source.lock().unwrap() = source.to_string();
        if self.streams.lock().unwrap().kid_voice {
            self.toggle(&self.kid_voice, true, "mic", || self.build_kid_voice());
        }
    }

    pub fn dad_voice(&self, pcm: Vec<u8>) {
        if let Some((_, src)) = &*self.dad_voice.lock().unwrap() {
            let _ = src.push_buffer(gst::Buffer::from_mut_slice(pcm));
        }
    }

    pub fn dad_cam(&self, jpeg: Vec<u8>) {
        if let Some((_, src)) = &*self.dad_cam.lock().unwrap() {
            let _ = src.push_buffer(gst::Buffer::from_mut_slice(jpeg));
        }
    }

    /// One captured frame with no row padding; `format` is BGRA or RGBA.
    pub fn push_screen(&self, width: u32, height: u32, format: &'static str, pixels: Vec<u8>) {
        let scale = (self.streams.lock().unwrap().screen_scale as u32).clamp(1, 4);
        let mut screen = self.screen.lock().unwrap();
        if screen.as_ref().is_none_or(|s| s.size != (width, height) || s.format != format || s.scale != scale) {
            if let Some(old) = screen.take() {
                let _ = old.pipeline.set_state(gst::State::Null);
            }
            // A failed build retried on every frame flooded Dad with errors and
            // leaked a socket pair each time until the station ran out of files.
            let mut failed = self.screen_failed.lock().unwrap();
            if failed.is_some_and(|t| t.elapsed() < RETRY_BUILD) {
                return;
            }
            match self.build_screen(width, height, format, scale) {
                Ok(s) => {
                    *screen = Some(s);
                    *failed = None;
                }
                Err(e) => {
                    *failed = Some(std::time::Instant::now());
                    let mut encoder = self.encoder.lock().unwrap();
                    if *encoder == "nvh264enc" {
                        *encoder = "openh264enc";
                        *failed = None;
                        self.out.status(format!("screen: NVENC failed ({e:#}); using the CPU encoder"));
                    } else {
                        self.out.status(format!("screen: {e:#}"));
                    }
                    return;
                }
            }
        }
        let _ = screen.as_ref().unwrap().src.push_buffer(gst::Buffer::from_mut_slice(pixels));
    }

    pub fn force_keyframe(&self) {
        if let Some(s) = &*self.screen.lock().unwrap() {
            request_keyframe(&s.sink);
        }
    }

    pub fn stop_all(&self) {
        self.apply(Streams::default());
        if let Some(s) = self.screen.lock().unwrap().take() {
            let _ = s.pipeline.set_state(gst::State::Null);
        }
    }

    fn toggle(&self, slot: &Mutex<Option<gst::Pipeline>>, on: bool, label: &str, build: impl FnOnce() -> Result<gst::Pipeline>) {
        let mut slot = slot.lock().unwrap();
        if let Some(p) = slot.take() {
            let _ = p.set_state(gst::State::Null);
        }
        if on {
            match build().and_then(|p| self.play(p, label)) {
                Ok(p) => *slot = Some(p),
                Err(e) => self.out.status(format!("{label}: {e:#}")),
            }
        }
    }

    fn toggle_src(
        &self,
        slot: &Mutex<Option<(gst::Pipeline, gst_app::AppSrc)>>,
        on: bool,
        label: &str,
        build: impl FnOnce() -> Result<gst::Pipeline>,
    ) {
        let mut slot = slot.lock().unwrap();
        if let Some((p, _)) = slot.take() {
            let _ = p.set_state(gst::State::Null);
        }
        if on {
            match build().and_then(|p| self.play(p, label)).and_then(|p| Ok((app_src(&p)?, p))) {
                Ok((src, p)) => *slot = Some((p, src)),
                Err(e) => self.out.status(format!("{label}: {e:#}")),
            }
        }
    }

    /// Start a pipeline, reporting its later errors (camera unplugged, device busy) as status.
    fn play(&self, pipeline: gst::Pipeline, label: &str) -> Result<gst::Pipeline> {
        let out = self.out.clone();
        let tag = label.to_string();
        pipeline.bus().context("pipeline bus")?.set_sync_handler(move |_, msg| {
            if let gst::MessageView::Error(e) = msg.view() {
                out.status(format!("{tag}: {}", e.error()));
            }
            gst::BusSyncReply::Drop
        });
        pipeline.set_state(gst::State::Playing).with_context(|| format!("start {label}"))?;
        Ok(pipeline)
    }

    pub fn encoder(&self) -> &'static str {
        *self.encoder.lock().unwrap()
    }

    fn build_screen(&self, width: u32, height: u32, format: &'static str, scale: u32) -> Result<Screen> {
        // Fewer pixels need fewer bits for the same quality.
        let kbps = 12000 / scale;
        let encode = match self.encoder() {
            // Straight into NVENC: no CPU colour conversion.
            "nvh264enc" => format!("nvh264enc preset=low-latency-hq rc-mode=cbr bitrate={kbps} gop-size=300 zerolatency=true"),
            _ => format!(
                "videoconvert ! video/x-raw,format=I420 ! openh264enc usage-type=screen bitrate={} complexity=low gop-size=300",
                kbps * 1000 * 2 / 3
            ),
        };
        // CPU scaling: COSMIC hands us shm frames, and NVENC won't take
        // GL memory from glcolorscale here (tested). Even sizes for H.264.
        let shrink = if scale > 1 {
            format!(
                "! videoscale method=bilinear n-threads=4 ! video/x-raw,width={},height={} ",
                width / scale & !1,
                height / scale & !1
            )
        } else {
            String::new()
        };
        let pipeline = launch(&format!(
            "appsrc name=src is-live=true do-timestamp=true format=time \
               caps=video/x-raw,format={format},width={width},height={height},framerate=30/1 \
             ! queue max-size-buffers=2 leaky=downstream \
             {shrink}! {encode} \
             ! h264parse config-interval=-1 \
             ! video/x-h264,stream-format=byte-stream,alignment=au \
             ! appsink name=sink sync=false max-buffers=4"
        ))?;
        let sink = app_sink(&pipeline)?;
        let out = self.out.clone();
        let waiting = self.waiting_key.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let Some(buf) = sample.buffer() else { return Ok(gst::FlowSuccess::Ok) };
                    let key = !buf.flags().contains(gst::BufferFlags::DELTA_UNIT);
                    if waiting.load(Ordering::Relaxed) && !key {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    let data = buf.map_readable().map_err(|_| gst::FlowError::Error)?.to_vec();
                    if out.video(FromStation::Video { key, data }) {
                        if key {
                            waiting.store(false, Ordering::Relaxed);
                        }
                    } else if !waiting.swap(true, Ordering::Relaxed) {
                        request_keyframe(sink);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let src = app_src(&pipeline)?;
        let pipeline = self.play(pipeline, "screen")?;
        tracing::info!(width, height, scale, kbps, encoder = self.encoder(), "screen.encoder_started");
        Ok(Screen { pipeline, src, sink, size: (width, height), format, scale })
    }

    fn build_kid_cam(&self) -> Result<gst::Pipeline> {
        let pipeline = launch(&format!(
            "v4l2src device=\"{}\" ! image/jpeg,width=640,height=360,framerate=30/1 \
             ! videorate drop-only=true ! image/jpeg,framerate={CAM_FPS}/1 \
             ! appsink name=sink sync=false max-buffers=2 drop=true",
            self.camera
        ))?;
        let out = self.out.clone();
        on_sample(&pipeline, move |data| out.cam(FromStation::KidCam(data)))?;
        Ok(pipeline)
    }

    fn build_kid_voice(&self) -> Result<gst::Pipeline> {
        anyhow::ensure!(!self.headset_source.lock().unwrap().is_empty(), "no microphone");
        let pipeline = launch(&format!(
            "pulsesrc device=\"{}\" client-name=tutor-station buffer-time=40000 latency-time=10000 \
             ! audioconvert ! audioresample ! {VOICE_CAPS} ! appsink name=sink sync=false",
            self.headset_source.lock().unwrap()
        ))?;
        let out = self.out.clone();
        on_sample(&pipeline, move |data| out.send(FromStation::KidVoice(data)))?;
        Ok(pipeline)
    }

    fn build_app_audio(&self) -> Result<gst::Pipeline> {
        let pipeline = launch(&format!(
            "pulsesrc device={APP_SINK}.monitor client-name=tutor-station buffer-time=40000 latency-time=10000 \
             ! audioconvert ! audioresample ! {APP_CAPS} ! appsink name=sink sync=false"
        ))?;
        let out = self.out.clone();
        on_sample(&pipeline, move |data| out.send(FromStation::AppAudio(data)))?;
        Ok(pipeline)
    }

    fn build_dad_voice(&self) -> Result<gst::Pipeline> {
        // Straight to the headset by name: the default sink is tutor_apps,
        // which streams back to Dad and would echo his own voice.
        anyhow::ensure!(!self.headset_sink.lock().unwrap().is_empty(), "no output");
        launch(&format!(
            "appsrc name=src is-live=true do-timestamp=true format=time caps=\"{VOICE_CAPS}\" \
             ! queue max-size-time=200000000 leaky=downstream ! audioconvert \
             ! pulsesink device=\"{}\" client-name=\"Dad voice\" sync=false buffer-time=60000 latency-time=10000",
            self.headset_sink.lock().unwrap()
        ))
    }

    fn build_dad_cam(&self) -> Result<gst::Pipeline> {
        let (w, h) = DAD_CAM_SIZE;
        let pipeline = launch(&format!(
            "appsrc name=src is-live=true do-timestamp=true format=time caps=image/jpeg \
             ! jpegparse ! jpegdec ! videoconvert ! videoscale \
             ! video/x-raw,format=BGRx,width={w},height={h} \
             ! appsink name=sink sync=false max-buffers=1 drop=true"
        ))?;
        let tx = self.loop_tx.clone();
        on_sample(&pipeline, move |data| {
            let _ = tx.send(LoopMsg::DadCam(Some(data)));
        })?;
        Ok(pipeline)
    }
}

fn request_keyframe(sink: &gst_app::AppSink) {
    sink.send_event(gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build());
}

fn launch(desc: &str) -> Result<gst::Pipeline> {
    gst::parse::launch(desc)
        .with_context(|| format!("build pipeline: {desc}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))
}

fn app_src(p: &gst::Pipeline) -> Result<gst_app::AppSrc> {
    p.by_name("src").context("no src")?.downcast().map_err(|_| anyhow::anyhow!("src is not an appsrc"))
}

fn app_sink(p: &gst::Pipeline) -> Result<gst_app::AppSink> {
    p.by_name("sink").context("no sink")?.downcast().map_err(|_| anyhow::anyhow!("sink is not an appsink"))
}

/// Hand every buffer reaching the pipeline's appsink to `f`.
fn on_sample(p: &gst::Pipeline, f: impl Fn(Vec<u8>) + Send + Sync + 'static) -> Result<()> {
    app_sink(p)?.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(buf) = sample.buffer() {
                    f(buf.map_readable().map_err(|_| gst::FlowError::Error)?.to_vec());
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(())
}

/// nvh264enc if NVENC can start right now, else the CPU encoder. `find` isn't
/// enough: the registry lists nvh264enc even when CUDA can't start in this
/// process, and then every build fails.
fn pick_encoder() -> &'static str {
    if gst::ElementFactory::make("nvh264enc").build().is_ok() { "nvh264enc" } else { "openh264enc" }
}
