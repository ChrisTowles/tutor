//! Messages between `tutor` (Dad's PC) and `tutor-station` (a kid's PC).
//!
//! Both run over the stdin/stdout of one SSH session. Each frame is a
//! little-endian `u32` length followed by a postcard-encoded message.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Bumped whenever a message changes shape.
pub const VERSION: u32 = 2;

/// Largest frame either side accepts. A 3440x1440 keyframe is well under 2 MB.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Voice and app audio: raw signed 16-bit little-endian PCM at this rate.
pub const AUDIO_RATE: u32 = 48_000;

/// Absolute pointer coordinates span `0..=ABS_MAX` across the kid's screen.
pub const ABS_MAX: u16 = u16::MAX;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToStation {
    Hello { version: u32 },
    /// Sent on connect and on every toggle or switch.
    Streams(Streams),
    Input(Input),
    /// 48 kHz mono s16le, ~20 ms chunks.
    DadVoice(Vec<u8>),
    /// One JPEG frame from Dad's camera.
    DadCam(Vec<u8>),
    /// Dad's decoder lost sync; send an IDR.
    Keyframe,
    /// Play their sound (apps and Dad's voice) on this output, by name.
    SetOutput(String),
    /// Use this mic for their voice, by name.
    SetMic(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Streams {
    /// 0 pauses the screen.
    pub screen_fps: u8,
    pub kid_cam: bool,
    pub kid_voice: bool,
    pub app_audio: bool,
    /// Play `DadVoice` on their headset.
    pub dad_voice: bool,
    /// Show Dad's camera overlay on their screen.
    pub dad_cam: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Input {
    /// Absolute position, `0..=ABS_MAX` on each axis.
    Move { x: u16, y: u16 },
    /// 1 left, 2 middle, 3 right, 8 back, 9 forward.
    Button { button: u8, down: bool },
    /// Wheel movement in 1/120ths of a detent; positive `dy` scrolls down.
    Scroll { dx: i32, dy: i32 },
    /// Linux evdev keycode, so keyboard layout never matters.
    Key { code: u16, down: bool },
    /// Release every held button and key (Control just turned off).
    ReleaseAll,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FromStation {
    Hello {
        version: u32,
        hostname: String,
        width: u32,
        height: u32,
        encoder: String,
    },
    /// One H.264 Annex B access unit.
    Video { key: bool, data: Vec<u8> },
    /// One JPEG frame from the kid's camera.
    KidCam(Vec<u8>),
    /// 48 kHz mono s16le.
    KidVoice(Vec<u8>),
    /// 48 kHz stereo s16le.
    AppAudio(Vec<u8>),
    /// Something the UI should show, e.g. "camera busy".
    Status(String),
    /// Their outputs and mics as (name, description), and the ones in use.
    /// Sent on connect and whenever a device comes, goes or is chosen.
    Devices { outputs: Vec<(String, String)>, output: String, mics: Vec<(String, String)>, mic: String },
}

pub fn write_frame<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let body = postcard::to_stdvec(msg).map_err(io::Error::other)?;
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// `Ok(None)` on a clean end of stream.
pub fn read_frame<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("frame of {len} bytes")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ToStation::Input(Input::Move { x: 1, y: ABS_MAX })).unwrap();
        write_frame(&mut buf, &ToStation::Streams(Streams { screen_fps: 30, dad_cam: true, ..Default::default() })).unwrap();
        let mut r = buf.as_slice();
        assert!(matches!(read_frame(&mut r).unwrap(), Some(ToStation::Input(Input::Move { x: 1, y: ABS_MAX }))));
        assert!(matches!(read_frame(&mut r).unwrap(), Some(ToStation::Streams(Streams { screen_fps: 30, dad_cam: true, .. }))));
        assert!(read_frame::<ToStation>(&mut r).unwrap().is_none());
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut buf = ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec();
        buf.extend([0; 8]);
        assert!(read_frame::<FromStation>(&mut buf.as_slice()).is_err());
    }
}
