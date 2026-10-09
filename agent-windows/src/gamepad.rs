//! The client's gamepads on Windows: one virtual Xbox 360 pad per client
//! pad through the ViGEmBus driver, which games read through XInput (and
//! DirectInput, Raw Input and Steam) like a wired pad. Windows has no
//! virtual gamepads of its own; ViGEmBus is the driver Parsec and others
//! install, and without it the host has no pads and says so.

use std::rc::Rc;

use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};
use windowcast_host::gamepad::{GamepadSink, MAX_PADS};
use windowcast_protocol::GamepadState;

/// One session's pads, sharing one connection to the bus.
pub struct WindowsPads {
    client: Rc<Client>,
    pads: [Option<Xbox360Wired<Rc<Client>>>; MAX_PADS as usize],
}

impl WindowsPads {
    pub fn open() -> Result<Self, String> {
        let client = Client::connect().map_err(|e| match e {
            vigem_client::Error::BusNotFound => {
                "gamepads need the ViGEmBus driver, which is not installed".to_owned()
            }
            e => format!("ViGEmBus: {e}"),
        })?;
        Ok(WindowsPads {
            client: Rc::new(client),
            pads: Default::default(),
        })
    }

    /// The XInput slot (0 to 3) pad `pad` landed in, once plugged in.
    pub fn user_index(&mut self, pad: u8) -> Option<u32> {
        self.pads
            .get_mut(usize::from(pad))?
            .as_mut()?
            .get_user_index()
            .ok()
    }
}

/// The XInput report for `state`: windowcast's layout is XInput's, bit
/// for bit and axis for axis.
pub fn report(state: &GamepadState) -> XGamepad {
    XGamepad {
        buttons: XButtons::from((state.buttons & 0xffff) as u16),
        left_trigger: state.left_trigger,
        right_trigger: state.right_trigger,
        thumb_lx: state.left_x,
        thumb_ly: state.left_y,
        thumb_rx: state.right_x,
        thumb_ry: state.right_y,
    }
}

fn plug_in(client: &Rc<Client>) -> Result<Xbox360Wired<Rc<Client>>, vigem_client::Error> {
    let mut pad = Xbox360Wired::new(Rc::clone(client), TargetId::XBOX360_WIRED);
    pad.plugin()?;
    pad.wait_ready()?;
    Ok(pad)
}

impl GamepadSink for WindowsPads {
    fn set(&mut self, pad: u8, state: &GamepadState) {
        let Some(slot) = self.pads.get_mut(usize::from(pad)) else {
            return;
        };
        if slot.is_none() {
            match plug_in(&self.client) {
                Ok(target) => *slot = Some(target),
                Err(e) => {
                    eprintln!("gamepad {}: {e}", pad + 1);
                    return;
                }
            }
        }
        if let Some(target) = slot.as_mut() {
            if let Err(e) = target.update(&report(state)) {
                eprintln!("gamepad {}: {e}", pad + 1);
            }
        }
    }

    fn remove(&mut self, pad: u8) {
        if let Some(slot) = self.pads.get_mut(usize::from(pad)) {
            // Dropping the target unplugs it.
            slot.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windowcast_protocol::GamepadButtons;

    #[test]
    fn the_state_is_the_xinput_report() {
        let report = report(&GamepadState {
            buttons: GamepadButtons::A | GamepadButtons::GUIDE | GamepadButtons::DPAD_RIGHT,
            left_x: -1,
            left_y: 32767,
            right_x: -32768,
            right_y: 5,
            left_trigger: 1,
            right_trigger: 255,
        });
        assert_eq!(u16::from(report.buttons), 0x1000 | 0x0400 | 0x0008);
        assert_eq!(
            (
                report.thumb_lx,
                report.thumb_ly,
                report.thumb_rx,
                report.thumb_ry
            ),
            (-1, 32767, -32768, 5)
        );
        assert_eq!((report.left_trigger, report.right_trigger), (1, 255));
    }
}
