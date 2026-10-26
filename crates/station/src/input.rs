//! Dad's mouse and keyboard, replayed through two virtual uinput devices: an
//! absolute pointer (like a VM's USB tablet) and a full keyboard. COSMIC has no
//! virtual-pointer Wayland protocol, but it reads uinput devices like real ones.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, InputEvent, KeyCode, RelativeAxisCode, UinputAbsSetup,
    uinput::VirtualDevice,
};
use tutor_proto::{ABS_MAX, Input};

const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const DETENT: i32 = 120;

/// Pointer buttons by the protocol's 1/2/3/8/9 numbering.
const BUTTONS: [(u8, KeyCode); 5] = [
    (1, KeyCode::BTN_LEFT),
    (2, KeyCode::BTN_MIDDLE),
    (3, KeyCode::BTN_RIGHT),
    (8, KeyCode::BTN_SIDE),
    (9, KeyCode::BTN_EXTRA),
];

pub struct Injector {
    pointer: VirtualDevice,
    keyboard: VirtualDevice,
    held_buttons: BTreeSet<u16>,
    held_keys: BTreeSet<u16>,
    /// Hi-res wheel remainder, for the classic one-per-detent events.
    wheel: (i32, i32),
}

impl Injector {
    pub fn new() -> Result<Self> {
        let mut buttons = AttributeSet::<KeyCode>::new();
        for (_, code) in BUTTONS {
            buttons.insert(code);
        }
        let mut wheels = AttributeSet::<RelativeAxisCode>::new();
        for code in [
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ] {
            wheels.insert(code);
        }
        let axis = |code| UinputAbsSetup::new(code, AbsInfo::new(0, 0, ABS_MAX as i32, 0, 0, 0));
        let pointer = VirtualDevice::builder()
            .context("open /dev/uinput (is this user in the input group?)")?
            .name("tutor pointer")
            .with_keys(&buttons)?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_X))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Y))?
            .with_relative_axes(&wheels)?
            .build()
            .context("create virtual pointer")?;

        let mut keys = AttributeSet::<KeyCode>::new();
        // Every keyboard key; pointer buttons start at 0x100.
        for code in 1..0x100 {
            keys.insert(KeyCode::new(code));
        }
        let keyboard = VirtualDevice::builder()?
            .name("tutor keyboard")
            .with_keys(&keys)?
            .build()
            .context("create virtual keyboard")?;

        Ok(Self { pointer, keyboard, held_buttons: BTreeSet::new(), held_keys: BTreeSet::new(), wheel: (0, 0) })
    }

    pub fn apply(&mut self, input: Input) -> Result<()> {
        match input {
            Input::Move { x, y } => self.pointer.emit(&[
                InputEvent::new(EV_ABS, AbsoluteAxisCode::ABS_X.0, x as i32),
                InputEvent::new(EV_ABS, AbsoluteAxisCode::ABS_Y.0, y as i32),
            ])?,
            Input::Button { button, down } => {
                let Some((_, code)) = BUTTONS.iter().find(|(b, _)| *b == button) else { return Ok(()) };
                if down {
                    self.held_buttons.insert(code.0);
                } else {
                    self.held_buttons.remove(&code.0);
                }
                self.pointer.emit(&[InputEvent::new(EV_KEY, code.0, down as i32)])?;
            }
            Input::Scroll { dx, dy } => {
                // Protocol: positive dy scrolls down. evdev: positive REL_WHEEL scrolls up.
                let mut events = Vec::with_capacity(4);
                if dy != 0 {
                    events.push(InputEvent::new(EV_REL, RelativeAxisCode::REL_WHEEL_HI_RES.0, -dy));
                    self.wheel.1 -= dy;
                    let detents = self.wheel.1 / DETENT;
                    if detents != 0 {
                        events.push(InputEvent::new(EV_REL, RelativeAxisCode::REL_WHEEL.0, detents));
                        self.wheel.1 -= detents * DETENT;
                    }
                }
                if dx != 0 {
                    events.push(InputEvent::new(EV_REL, RelativeAxisCode::REL_HWHEEL_HI_RES.0, dx));
                    self.wheel.0 += dx;
                    let detents = self.wheel.0 / DETENT;
                    if detents != 0 {
                        events.push(InputEvent::new(EV_REL, RelativeAxisCode::REL_HWHEEL.0, detents));
                        self.wheel.0 -= detents * DETENT;
                    }
                }
                if !events.is_empty() {
                    self.pointer.emit(&events)?;
                }
            }
            Input::Key { code, down } => {
                if code == 0 || code >= 0x100 {
                    return Ok(());
                }
                if down {
                    self.held_keys.insert(code);
                } else {
                    self.held_keys.remove(&code);
                }
                self.keyboard.emit(&[InputEvent::new(EV_KEY, code, down as i32)])?;
            }
            Input::ReleaseAll => self.release_all()?,
        }
        Ok(())
    }

    /// Wake a sleeping display without typing anything: tap Shift.
    pub fn wake(&mut self) -> Result<()> {
        let shift = KeyCode::KEY_LEFTSHIFT.0;
        self.keyboard.emit(&[InputEvent::new(EV_KEY, shift, 1)])?;
        self.keyboard.emit(&[InputEvent::new(EV_KEY, shift, 0)])?;
        Ok(())
    }

    pub fn release_all(&mut self) -> Result<()> {
        for code in std::mem::take(&mut self.held_keys) {
            self.keyboard.emit(&[InputEvent::new(EV_KEY, code, 0)])?;
        }
        for code in std::mem::take(&mut self.held_buttons) {
            self.pointer.emit(&[InputEvent::new(EV_KEY, code, 0)])?;
        }
        Ok(())
    }
}
