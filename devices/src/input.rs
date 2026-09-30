//! Input events on their way from the host window to the PS/2 devices.
//!
//! The UI thread pushes events; the VM thread takes them between vCPU runs
//! and feeds them to the board ([`crate::pc::Pc::input`]). Consecutive mouse
//! motion with the same buttons is merged, so a busy mouse can't flood the
//! queue while keeping every button change in order.

use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// One key event as scan code set 2 bytes (see [`crate::keymap`]).
    Key(Vec<u8>),
    /// Relative motion in PS/2 conventions: `dx` right, `dy` up, `dz` wheel
    /// (positive = towards the user), `buttons` bit 0 left, 1 right,
    /// 2 middle, 3/4 side buttons (held after this event).
    Mouse { dx: i32, dy: i32, dz: i32, buttons: u8 },
}

#[derive(Default)]
pub struct InputQueue {
    events: Mutex<Vec<InputEvent>>,
}

impl InputQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, ev: InputEvent) {
        let mut q = self.events.lock().unwrap();
        if let (Some(InputEvent::Mouse { dx, dy, dz, buttons }), InputEvent::Mouse { dx: x, dy: y, dz: z, buttons: b }) =
            (q.last_mut(), &ev)
        {
            if buttons == b {
                *dx += x;
                *dy += y;
                *dz += z;
                return;
            }
        }
        q.push(ev);
    }

    /// Everything queued so far, oldest first.
    pub fn take(&self) -> Vec<InputEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_motion_but_keeps_button_changes_in_order() {
        let q = InputQueue::new();
        q.push(InputEvent::Mouse { dx: 1, dy: 2, dz: 0, buttons: 0 });
        q.push(InputEvent::Mouse { dx: 3, dy: -1, dz: 1, buttons: 0 });
        q.push(InputEvent::Key(vec![0x1c]));
        q.push(InputEvent::Mouse { dx: 1, dy: 0, dz: 0, buttons: 0 });
        q.push(InputEvent::Mouse { dx: 0, dy: 0, dz: 0, buttons: 1 });
        q.push(InputEvent::Mouse { dx: 5, dy: 5, dz: 0, buttons: 1 });
        assert_eq!(
            q.take(),
            [
                InputEvent::Mouse { dx: 4, dy: 1, dz: 1, buttons: 0 },
                InputEvent::Key(vec![0x1c]),
                InputEvent::Mouse { dx: 1, dy: 0, dz: 0, buttons: 0 },
                InputEvent::Mouse { dx: 5, dy: 5, dz: 0, buttons: 1 },
            ]
        );
        assert!(q.take().is_empty());
    }
}
