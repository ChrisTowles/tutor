//! Everything the station sends goes through one writer thread on stdout.
//! Voice and status jump the queue; video and camera frames are dropped
//! instead of queued when SSH can't keep up, so latency never builds.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    thread,
};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select_biased};
use tutor_proto::{FromStation, write_frame};

#[derive(Clone)]
pub struct Out {
    prio: Sender<FromStation>,
    video: Sender<FromStation>,
    cam: Sender<FromStation>,
    dropped: Arc<AtomicU32>,
    last_status: Arc<std::sync::Mutex<Option<String>>>,
}

impl Out {
    /// Screen frames dropped since the last call.
    pub fn take_dropped(&self) -> u32 {
        self.dropped.swap(0, Ordering::Relaxed)
    }

    /// Audio and status. Only dropped if the pipe has been stuck for seconds.
    pub fn send(&self, msg: FromStation) {
        let _ = self.prio.try_send(msg);
    }

    /// Repeats of the last message are dropped.
    pub fn status(&self, text: impl Into<String>) {
        let text = text.into();
        if self.last_status.lock().unwrap().replace(text.clone()).as_deref() == Some(text.as_str()) {
            return;
        }
        tracing::warn!(%text, "station.status");
        self.send(FromStation::Status(text));
    }

    /// False when the frame was dropped; the caller must restart from a keyframe.
    pub fn video(&self, msg: FromStation) -> bool {
        let full = matches!(self.video.try_send(msg), Err(TrySendError::Full(_)));
        if full {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        !full
    }

    pub fn cam(&self, msg: FromStation) {
        let _ = self.cam.try_send(msg);
    }
}

pub fn spawn() -> (Out, thread::JoinHandle<()>) {
    let (prio, prio_rx) = bounded(1024);
    let (video, video_rx) = bounded(3);
    let (cam, cam_rx) = bounded(2);
    let handle = thread::Builder::new()
        .name("writer".into())
        .spawn(move || {
            if let Err(e) = write_loop(prio_rx, video_rx, cam_rx) {
                tracing::info!(error = %e, "stdout.closed");
            }
        })
        .expect("spawn writer");
    (Out { prio, video, cam, dropped: Arc::default(), last_status: Arc::default() }, handle)
}

fn write_loop(
    prio: Receiver<FromStation>,
    video: Receiver<FromStation>,
    cam: Receiver<FromStation>,
) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    loop {
        let msg = select_biased! {
            recv(prio) -> m => m,
            recv(video) -> m => m,
            recv(cam) -> m => m,
        };
        let Ok(msg) = msg else { return Ok(()) };
        write_frame(&mut stdout, &msg)?;
    }
}
