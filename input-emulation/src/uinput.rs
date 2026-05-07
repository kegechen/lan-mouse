//! uinput-based emulation backend.
//!
//! Necessary on KDE Plasma 5 / dde-kwin Wayland sessions where neither libei
//! nor wlroots virtual input protocols are available and where XTest events
//! sent into Xwayland do not reach the wayland compositor's pointer.

use async_trait::async_trait;
use uinput::event::{controller, keyboard, relative};
use uinput::Device;

use input_event::{
    Event, KeyboardEvent, PointerEvent, BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT,
};

use crate::error::{EmulationError, UinputEmulationCreationError};

use super::{Emulation, EmulationHandle};

// raw evdev codes
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

const EVDEV_BTN_LEFT: u16 = 0x110;
const EVDEV_BTN_RIGHT: u16 = 0x111;
const EVDEV_BTN_MIDDLE: u16 = 0x112;
const EVDEV_BTN_FORWARD: u16 = 0x115;
const EVDEV_BTN_BACK: u16 = 0x116;

pub(crate) struct UinputEmulation {
    device: Device,
}

unsafe impl Send for UinputEmulation {}

impl UinputEmulation {
    pub(crate) fn new() -> Result<Self, UinputEmulationCreationError> {
        let device = uinput::default()?
            .name("lan-mouse-uinput")?
            .event(controller::Mouse::Left)?
            .event(controller::Mouse::Right)?
            .event(controller::Mouse::Middle)?
            .event(controller::Mouse::Side)?
            .event(controller::Mouse::Extra)?
            .event(controller::Mouse::Forward)?
            .event(controller::Mouse::Back)?
            .event(controller::Mouse::Task)?
            .event(relative::Position::X)?
            .event(relative::Position::Y)?
            .event(relative::Wheel::Vertical)?
            .event(relative::Wheel::Horizontal)?
            .event(keyboard::Keyboard::All)?
            .create()?;
        // give udev a moment to enumerate the new device before the first event
        std::thread::sleep(std::time::Duration::from_millis(200));
        log::info!("uinput emulation device created: lan-mouse-uinput");
        Ok(Self { device })
    }

    fn motion(&mut self, dx: i32, dy: i32) {
        if dx != 0 {
            let _ = self.device.write(EV_REL as i32, REL_X as i32, dx);
        }
        if dy != 0 {
            let _ = self.device.write(EV_REL as i32, REL_Y as i32, dy);
        }
        let _ = self.device.synchronize();
    }

    fn button(&mut self, button: u32, state: u32) {
        let code = match button {
            BTN_LEFT => EVDEV_BTN_LEFT,
            BTN_RIGHT => EVDEV_BTN_RIGHT,
            BTN_MIDDLE => EVDEV_BTN_MIDDLE,
            BTN_BACK => EVDEV_BTN_BACK,
            BTN_FORWARD => EVDEV_BTN_FORWARD,
            _ => EVDEV_BTN_LEFT,
        };
        let _ = self
            .device
            .write(EV_KEY as i32, code as i32, state as i32);
        let _ = self.device.synchronize();
    }

    fn scroll(&mut self, axis: u8, value: f64) {
        let ticks = if value.abs() >= 60.0 {
            (value / 120.0).round() as i32
        } else if value > 0.0 {
            1
        } else if value < 0.0 {
            -1
        } else {
            0
        };
        if ticks == 0 {
            return;
        }
        let evdev_value = -ticks;
        let code = if axis == 1 { REL_HWHEEL } else { REL_WHEEL };
        let _ = self
            .device
            .write(EV_REL as i32, code as i32, evdev_value);
        let _ = self.device.synchronize();
    }

    fn key(&mut self, key: u32, state: u8) {
        // lan-mouse `key` is a Linux evdev keycode already.
        let _ = self
            .device
            .write(EV_KEY as i32, key as i32, state as i32);
        let _ = self.device.synchronize();
    }
}

#[async_trait]
impl Emulation for UinputEmulation {
    async fn consume(&mut self, event: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(pe) => match pe {
                PointerEvent::Motion { dx, dy, .. } => self.motion(dx as i32, dy as i32),
                PointerEvent::Button { button, state, .. } => self.button(button, state),
                PointerEvent::Axis { axis, value, .. } => self.scroll(axis, value),
                PointerEvent::AxisDiscrete120 { axis, value } => self.scroll(axis, value as f64),
            },
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => self.key(key, state),
            _ => {}
        }
        Ok(())
    }

    async fn create(&mut self, _: EmulationHandle) {}
    async fn destroy(&mut self, _: EmulationHandle) {}
    async fn terminate(&mut self) {}
}
