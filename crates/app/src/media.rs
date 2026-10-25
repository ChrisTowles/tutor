//! GStreamer on Dad's PC: per-station playback (screen, webcam, voice, app
//! sound) and Dad's own mic and camera feeding every station.

use std::{process::Command, sync::Arc};

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gtk4::gdk;

const VOICE_CAPS: &str = "audio/x-raw,format=S16LE,rate=48000,channels=1,layout=interleaved";
const APP_CAPS: &str = "audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved";

/// What the UI shows for one station. GTK-thread only.
pub struct Paintables {
    pub screen: gdk::Paintable,
    pub webcam: gdk::Paintable,
}

/// Where one station's incoming streams play; fed from its connection thread.
pub struct Playback {
    video: gst_app::AppSrc,
    cam: gst_app::AppSrc,
    voice: gst_app::AppSrc,
    apps: gst_app::AppSrc,
    _pipelines: Vec<gst::Pipeline>,
}

impl Playback {
    /// Build on the GTK thread: the paintables belong to it.
    /// `speakers` is a sink name, or `None` for the default.
    pub fn new(name: &str, speakers: Option<&str>) -> Result<(Self, Paintables)> {
        let (video_p, video, screen) = video_pipeline(
            "video/x-h264,stream-format=byte-stream,alignment=au",
            &["h264parse", decoder()],
        )?;
        let (cam_p, cam, webcam) = video_pipeline("image/jpeg", &["jpegparse", "jpegdec"])?;
        let device = speakers.map(|s| format!("device=\"{s}\"")).unwrap_or_default();
        let audio = |caps: &str, label: &str| -> Result<(gst::Pipeline, gst_app::AppSrc)> {
            let p = launch(&format!(
                "appsrc name=src is-live=true do-timestamp=true format=time caps=\"{caps}\" \
                 ! queue max-size-time=200000000 leaky=downstream ! audioconvert \
                 ! pulsesink {device} client-name=\"{name} {label}\" sync=false slave-method=none \
                   buffer-time=120000 latency-time=20000"
            ))?;
            let src = app_src(&p)?;
            Ok((p, src))
        };
        let (voice_p, voice) = audio(VOICE_CAPS, "voice")?;
        let (apps_p, apps) = audio(APP_CAPS, "apps")?;
        let pipelines = vec![video_p, cam_p, voice_p, apps_p];
        for p in &pipelines {
            p.set_state(gst::State::Playing).context("start playback")?;
        }
        Ok((Self { video, cam, voice, apps, _pipelines: pipelines }, Paintables { screen, webcam }))
    }

    pub fn video(&self, h264: Vec<u8>) {
        let _ = self.video.push_buffer(gst::Buffer::from_mut_slice(h264));
    }
    pub fn cam(&self, jpeg: Vec<u8>) {
        let _ = self.cam.push_buffer(gst::Buffer::from_mut_slice(jpeg));
    }
    pub fn voice(&self, pcm: Vec<u8>) {
        let _ = self.voice.push_buffer(gst::Buffer::from_mut_slice(pcm));
    }
    pub fn apps(&self, pcm: Vec<u8>) {
        let _ = self.apps.push_buffer(gst::Buffer::from_mut_slice(pcm));
    }
}

fn decoder() -> &'static str {
    if gst::ElementFactory::find("nvh264dec").is_some() { "nvh264dec" } else { "avdec_h264" }
}

/// appsrc ! <decode chain> ! gtk4paintablesink, through GL when GTK has a GL context.
fn video_pipeline(caps: &str, decode: &[&str]) -> Result<(gst::Pipeline, gst_app::AppSrc, gdk::Paintable)> {
    let pipeline = gst::Pipeline::new();
    let src = gst_app::AppSrc::builder()
        .caps(&caps.parse::<gst::Caps>().context("caps")?)
        .is_live(true)
        .do_timestamp(true)
        .format(gst::Format::Time)
        .build();
    let gtksink = gst::ElementFactory::make("gtk4paintablesink").property("sync", false).build()?;
    let paintable = gtksink.property::<gdk::Paintable>("paintable");
    let has_gl = paintable.property::<Option<gdk::GLContext>>("gl-context").is_some();
    let sink = if has_gl {
        gst::ElementFactory::make("glsinkbin").property("sink", &gtksink).build()?
    } else {
        let bin = gst::Bin::new();
        let convert = gst::ElementFactory::make("videoconvert").build()?;
        bin.add_many([&convert, &gtksink])?;
        convert.link(&gtksink)?;
        bin.add_pad(&gst::GhostPad::with_target(&convert.static_pad("sink").unwrap())?)?;
        bin.upcast()
    };
    let mut chain: Vec<gst::Element> = vec![src.clone().upcast()];
    for name in decode {
        chain.push(gst::ElementFactory::make(name).build().with_context(|| format!("element {name}"))?);
    }
    chain.push(sink);
    pipeline.add_many(&chain)?;
    gst::Element::link_many(&chain)?;
    Ok((pipeline, src, paintable))
}

/// Dad's mic and camera. Each callback gets every chunk/frame; callers decide
/// which stations want it.
pub struct Me {
    _mic: gst::Pipeline,
    camera: std::sync::Mutex<Option<gst::Pipeline>>,
    camera_device: String,
    on_frame: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
}

impl Me {
    pub fn new(
        mic: &str,
        camera_device: String,
        on_voice: impl Fn(Vec<u8>) + Send + Sync + 'static,
        on_frame: impl Fn(Vec<u8>) + Send + Sync + 'static,
    ) -> Result<Self> {
        let device = find_device("sources", mic).map(|d| format!("device=\"{d}\"")).unwrap_or_default();
        let mic = launch(&format!(
            "pulsesrc {device} client-name=tutor buffer-time=40000 latency-time=10000 \
             ! audioconvert ! audioresample ! {VOICE_CAPS} ! appsink name=sink sync=false"
        ))?;
        on_sample(&mic, on_voice)?;
        mic.set_state(gst::State::Playing).context("start my mic")?;
        Ok(Self { _mic: mic, camera: Default::default(), camera_device, on_frame: Arc::new(on_frame) })
    }

    /// Run the camera only while some station shows it.
    pub fn set_camera(&self, on: bool) -> Result<()> {
        let mut cam = self.camera.lock().unwrap();
        match (on, cam.is_some()) {
            (true, false) => {
                let p = launch(&format!(
                    "v4l2src device=\"{}\" ! image/jpeg,width=640,height=360,framerate=30/1 \
                     ! videorate drop-only=true ! image/jpeg,framerate=15/1 \
                     ! appsink name=sink sync=false max-buffers=2 drop=true",
                    self.camera_device
                ))?;
                let f = self.on_frame.clone();
                on_sample(&p, move |jpeg| f(jpeg))?;
                p.set_state(gst::State::Playing).context("start my camera")?;
                *cam = Some(p);
            }
            (false, true) => {
                let _ = cam.take().unwrap().set_state(gst::State::Null);
            }
            _ => {}
        }
        Ok(())
    }
}

/// PulseAudio sink or source (`kind` = "sinks" | "sources", never a monitor)
/// whose name contains `pattern`. `None` means use the default.
pub fn find_device(kind: &str, pattern: &str) -> Option<String> {
    if pattern.is_empty() {
        return None;
    }
    let out = Command::new("pactl").args(["list", "short", kind]).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split('\t').nth(1))
        .find(|n| n.contains(pattern) && !n.ends_with(".monitor"))
        .map(str::to_string)
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

fn on_sample(p: &gst::Pipeline, f: impl Fn(Vec<u8>) + Send + Sync + 'static) -> Result<()> {
    let sink: gst_app::AppSink =
        p.by_name("sink").context("no sink")?.downcast().map_err(|_| anyhow::anyhow!("sink is not an appsink"))?;
    sink.set_callbacks(
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
