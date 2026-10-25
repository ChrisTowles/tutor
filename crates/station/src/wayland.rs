//! The station's Wayland client: captures the kid's screen with
//! `ext-image-copy-capture` and shows Dad's camera in a `wlr-layer-shell`
//! overlay. Runs the calloop event loop on the main thread.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_registry,
    dispatch2::Dispatch2,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            LoopHandle,
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use tutor_proto::{FromStation, VERSION};
use wayland_client::{
    Connection, QueueHandle, WEnum,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
};
use wayland_protocols::ext::{
    image_capture_source::v1::client::{
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
        ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};

use crate::{
    media::{DAD_CAM_SIZE, Media},
    out::Out,
};

/// Messages into the Wayland thread from the rest of the station.
pub enum LoopMsg {
    /// Screen frames per second; 0 pauses capture.
    Fps(u8),
    /// A decoded BGRx frame of `DAD_CAM_SIZE`, or `None` to hide the overlay.
    DadCam(Option<Vec<u8>>),
    Quit,
}

/// 32-bit shm formats we can encode, with their little-endian byte order as
/// GStreamer names it (the encoder ignores the X/A byte). COSMIC offers the
/// xBGR pair; other compositors the xRGB one.
const SHM_FORMATS: [(wl_shm::Format, &str); 4] = [
    (wl_shm::Format::Xrgb8888, "BGRA"),
    (wl_shm::Format::Argb8888, "BGRA"),
    (wl_shm::Format::Xbgr8888, "RGBA"),
    (wl_shm::Format::Abgr8888, "RGBA"),
];

/// Overlay position, inside the area panels leave free.
const OVERLAY_MARGIN: i32 = 16;

pub struct Wl {
    registry: RegistryState,
    outputs: OutputState,
    compositor: CompositorState,
    shm: Shm,
    layer_shell: Option<LayerShell>,
    copy_manager: ExtImageCopyCaptureManagerV1,
    source_manager: ExtOutputImageCaptureSourceManagerV1,
    qh: QueueHandle<Wl>,
    handle: LoopHandle<'static, Wl>,
    media: Arc<Media>,
    out: Out,
    hostname: String,

    capture: Option<Capture>,
    fps: u8,
    last_capture: Instant,
    timer_armed: bool,
    hello_sent: bool,

    overlay: Option<Overlay>,
    pub exit: bool,
}

struct Capture {
    _source: ExtImageCaptureSourceV1,
    session: ExtImageCopyCaptureSessionV1,
    /// Constraints as the compositor is sending them, applied on `done`.
    pending_size: (u32, u32),
    pending_formats: Vec<wl_shm::Format>,
    size: (u32, u32),
    /// The buffer's byte order in GStreamer's naming.
    gst_format: &'static str,
    pool: Option<SlotPool>,
    buffer: Option<Buffer>,
    frame: Option<ExtImageCopyCaptureFrameV1>,
}

struct Overlay {
    layer: LayerSurface,
    pool: SlotPool,
    configured: bool,
    pending: Option<Vec<u8>>,
}

impl Wl {
    pub fn connect(handle: LoopHandle<'static, Wl>, media: Arc<Media>, out: Out, hostname: String) -> Result<Self> {
        let conn = Connection::connect_to_env().context("connect to the Wayland session")?;
        let (globals, queue) = registry_queue_init(&conn).context("list Wayland globals")?;
        let qh = queue.handle();
        let state = Self {
            registry: RegistryState::new(&globals),
            outputs: OutputState::new(&globals, &qh),
            compositor: CompositorState::bind(&globals, &qh).context("wl_compositor")?,
            shm: Shm::bind(&globals, &qh).context("wl_shm")?,
            layer_shell: LayerShell::bind(&globals, &qh).ok(),
            copy_manager: globals.bind(&qh, 1..=1, Global).context("ext_image_copy_capture_manager_v1")?,
            source_manager: globals
                .bind(&qh, 1..=1, Global)
                .context("ext_output_image_capture_source_manager_v1")?,
            qh,
            handle: handle.clone(),
            media,
            out,
            hostname,
            capture: None,
            fps: 0,
            last_capture: Instant::now(),
            timer_armed: false,
            hello_sent: false,
            overlay: None,
            exit: false,
        };
        WaylandSource::new(conn, queue)
            .insert(handle)
            .map_err(|e| anyhow::anyhow!("watch Wayland socket: {e}"))?;
        Ok(state)
    }

    pub fn handle(&mut self, msg: LoopMsg) {
        match msg {
            LoopMsg::Fps(fps) => {
                self.fps = fps;
                self.maybe_capture();
            }
            LoopMsg::DadCam(Some(frame)) => self.show_dad_cam(frame),
            LoopMsg::DadCam(None) => self.overlay = None,
            LoopMsg::Quit => self.exit = true,
        }
    }

    fn start_session(&mut self, output: &wl_output::WlOutput) {
        let source = self.source_manager.create_source(output, &self.qh, Global);
        let session = self.copy_manager.create_session(&source, Options::PaintCursors, &self.qh, SessionData);
        self.capture = Some(Capture {
            _source: source,
            session,
            pending_size: (0, 0),
            pending_formats: Vec::new(),
            size: (0, 0),
            gst_format: "BGRA",
            pool: None,
            buffer: None,
            frame: None,
        });
    }

    /// Constraints arrived (first time, or the screen changed): size a buffer to match.
    fn constraints_done(&mut self) {
        let Some(cap) = &mut self.capture else { return };
        let (w, h) = cap.pending_size;
        let format = SHM_FORMATS.iter().find(|(f, _)| cap.pending_formats.contains(f)).copied();
        cap.pending_formats.clear();
        let Some((format, gst_format)) = format else {
            self.out.status("screen: compositor offers no 32-bit shm format");
            return;
        };
        if cap.size != (w, h) || cap.buffer.is_none() {
            let len = (w * h * 4) as usize;
            let made = SlotPool::new(len, &self.shm)
                .map_err(|e| e.to_string())
                .and_then(|mut pool| {
                    let (buffer, _) = pool
                        .create_buffer(w as i32, h as i32, (w * 4) as i32, format)
                        .map_err(|e| e.to_string())?;
                    Ok((pool, buffer))
                });
            match made {
                Ok((pool, buffer)) => {
                    cap.pool = Some(pool);
                    cap.buffer = Some(buffer);
                    cap.size = (w, h);
                    cap.gst_format = gst_format;
                }
                Err(e) => {
                    self.out.status(format!("screen: shm buffer: {e}"));
                    return;
                }
            }
        }
        if !self.hello_sent {
            self.hello_sent = true;
            self.out.send(FromStation::Hello {
                version: VERSION,
                hostname: self.hostname.clone(),
                width: w,
                height: h,
                encoder: self.media.encoder.to_string(),
            });
        }
        self.maybe_capture();
    }

    /// Ask for the next frame if one is due. The compositor answers only once
    /// the screen changes, so a still screen costs nothing.
    fn maybe_capture(&mut self) {
        if self.fps == 0 || self.timer_armed {
            return;
        }
        let Some(cap) = &mut self.capture else { return };
        let Some(buffer) = &cap.buffer else { return };
        if cap.frame.is_some() {
            return;
        }
        let interval = Duration::from_secs(1) / self.fps as u32;
        let since = self.last_capture.elapsed();
        if since < interval {
            self.timer_armed = true;
            let _ = self.handle.insert_source(Timer::from_duration(interval - since), |_, _, wl| {
                wl.timer_armed = false;
                wl.maybe_capture();
                TimeoutAction::Drop
            });
            return;
        }
        let (w, h) = cap.size;
        let frame = cap.session.create_frame(&self.qh, FrameData);
        frame.attach_buffer(buffer.wl_buffer());
        frame.damage_buffer(0, 0, w as i32, h as i32);
        frame.capture();
        cap.frame = Some(frame);
        self.last_capture = Instant::now();
    }

    fn frame_ready(&mut self) {
        let Some(cap) = &mut self.capture else { return };
        if let Some(f) = cap.frame.take() {
            f.destroy();
        }
        let (w, h) = cap.size;
        if let (Some(pool), Some(buffer)) = (&mut cap.pool, &cap.buffer)
            && let Some(canvas) = buffer.canvas(pool)
        {
            let pixels = canvas.to_vec();
            self.media.push_screen(w, h, cap.gst_format, pixels);
        }
        self.maybe_capture();
    }

    fn frame_failed(&mut self, reason: WEnum<FailureReason>) {
        let Some(cap) = &mut self.capture else { return };
        if let Some(f) = cap.frame.take() {
            f.destroy();
        }
        match reason {
            // New constraints follow, then `done` restarts capture.
            WEnum::Value(FailureReason::BufferConstraints) => {}
            // The session's `stopped` event handles this.
            WEnum::Value(FailureReason::Stopped) => {}
            _ => {
                self.last_capture = Instant::now();
                self.maybe_capture();
            }
        }
    }

    fn session_stopped(&mut self) {
        if let Some(cap) = self.capture.take() {
            if let Some(f) = cap.frame {
                f.destroy();
            }
            cap.session.destroy();
        }
        // The monitor went away or was reconfigured; try again shortly.
        let _ = self.handle.insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, wl| {
            if wl.capture.is_none()
                && let Some(output) = wl.outputs.outputs().next()
            {
                wl.start_session(&output);
            }
            TimeoutAction::Drop
        });
    }

    fn show_dad_cam(&mut self, frame: Vec<u8>) {
        if self.overlay.is_none() {
            let Some(layer_shell) = &self.layer_shell else {
                self.out.status("Dad's camera: no layer-shell on this desktop");
                return;
            };
            let (w, h) = DAD_CAM_SIZE;
            let surface = self.compositor.create_surface(&self.qh);
            // Empty input region: clicks fall through to whatever is underneath.
            if let Ok(region) = Region::new(&self.compositor) {
                surface.set_input_region(Some(region.wl_region()));
            }
            let layer = layer_shell.create_layer_surface(&self.qh, surface, Layer::Overlay, Some("tutor-dad-cam"), None);
            layer.set_anchor(Anchor::TOP | Anchor::LEFT);
            layer.set_margin(OVERLAY_MARGIN, 0, 0, OVERLAY_MARGIN);
            layer.set_size(w, h);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            layer.commit();
            let Ok(pool) = SlotPool::new((w * h * 4 * 2) as usize, &self.shm) else { return };
            self.overlay = Some(Overlay { layer, pool, configured: false, pending: None });
        }
        let overlay = self.overlay.as_mut().unwrap();
        overlay.pending = Some(frame);
        if overlay.configured {
            overlay.draw();
        }
    }
}

impl Overlay {
    fn draw(&mut self) {
        let Some(frame) = self.pending.take() else { return };
        let (w, h) = DAD_CAM_SIZE;
        let Ok((buffer, canvas)) = self.pool.create_buffer(w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Xrgb8888)
        else {
            return;
        };
        let n = canvas.len().min(frame.len());
        canvas[..n].copy_from_slice(&frame[..n]);
        let surface = self.layer.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        if buffer.attach_to(surface).is_ok() {
            self.layer.commit();
        }
    }
}

/// User data for globals and capture sources, which have no events we use.
struct Global;
struct SessionData;
struct FrameData;

impl Dispatch2<ExtImageCopyCaptureManagerV1, Wl> for Global {
    fn event(&self, _: &mut Wl, _: &ExtImageCopyCaptureManagerV1, _: <ExtImageCopyCaptureManagerV1 as wayland_client::Proxy>::Event, _: &Connection, _: &QueueHandle<Wl>) {}
}

impl Dispatch2<ExtOutputImageCaptureSourceManagerV1, Wl> for Global {
    fn event(&self, _: &mut Wl, _: &ExtOutputImageCaptureSourceManagerV1, _: <ExtOutputImageCaptureSourceManagerV1 as wayland_client::Proxy>::Event, _: &Connection, _: &QueueHandle<Wl>) {}
}

impl Dispatch2<ExtImageCaptureSourceV1, Wl> for Global {
    fn event(&self, _: &mut Wl, _: &ExtImageCaptureSourceV1, _: <ExtImageCaptureSourceV1 as wayland_client::Proxy>::Event, _: &Connection, _: &QueueHandle<Wl>) {}
}

impl Dispatch2<ExtImageCopyCaptureSessionV1, Wl> for SessionData {
    fn event(
        &self,
        wl: &mut Wl,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &Connection,
        _: &QueueHandle<Wl>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                if let Some(cap) = &mut wl.capture {
                    cap.pending_size = (width, height);
                }
            }
            Event::ShmFormat { format: WEnum::Value(format) } => {
                if let Some(cap) = &mut wl.capture {
                    cap.pending_formats.push(format);
                }
            }
            Event::Done => wl.constraints_done(),
            Event::Stopped => wl.session_stopped(),
            _ => {}
        }
    }
}

impl Dispatch2<ExtImageCopyCaptureFrameV1, Wl> for FrameData {
    fn event(
        &self,
        wl: &mut Wl,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &Connection,
        _: &QueueHandle<Wl>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Ready => wl.frame_ready(),
            Event::Failed { reason } => wl.frame_failed(reason),
            _ => {}
        }
    }
}

impl OutputHandler for Wl {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // One monitor per kid's PC: capture the first one.
        if self.capture.is_none() {
            self.start_session(&output);
        }
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl CompositorHandler for Wl {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl LayerShellHandler for Wl {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.overlay = None;
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface, _: LayerSurfaceConfigure, _: u32) {
        if let Some(overlay) = &mut self.overlay {
            overlay.configured = true;
            overlay.draw();
        }
    }
}

impl ShmHandler for Wl {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Wl {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_registry!(Wl);
smithay_client_toolkit::delegate_dispatch2!(Wl);
