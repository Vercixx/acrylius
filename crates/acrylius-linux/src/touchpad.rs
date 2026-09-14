//! A virtual multitouch touchpad on `/dev/uinput`. Gestures are libinput's job:
//! this only has to look like a touchpad and report where the fingers are.

use std::fs::{File, OpenOptions};

use acrylius_core::vocab::{TouchPoint, TouchpadOp};
use input_linux::sys;
use input_linux::{
    AbsoluteAxis, AbsoluteInfo, AbsoluteInfoSetup, EventKind, InputId, InputProperty, Key,
    UInputHandle,
};

const UINPUT: &str = "/dev/uinput";
const NAME: &[u8] = b"Acrylius Touchpad";

/// Both axes, matching the wire's normalised range, so nothing is rescaled.
const MAX_POS: i32 = 65535;

pub const SLOTS: usize = 10;

/// One evdev event, without the kernel's timestamp. Keeping the frame builder
/// on this rather than on `input_event` is what makes it testable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ev {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

impl Ev {
    const fn new(kind: u16, code: u16, value: i32) -> Self {
        Self { kind, code, value }
    }

    fn to_input_event(self) -> sys::input_event {
        sys::input_event {
            time: sys::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_: self.kind,
            code: self.code,
            value: self.value,
        }
    }
}

const EV_KEY: u16 = sys::EV_KEY as u16;
const EV_ABS: u16 = sys::EV_ABS as u16;
const EV_SYN: u16 = sys::EV_SYN as u16;

/// Which kernel slot holds which phone-side finger, and the tracking ids handed
/// out so far.
#[derive(Debug)]
pub struct Slots {
    held: [Option<u8>; SLOTS],
    next_tracking: i32,
}

impl Default for Slots {
    fn default() -> Self {
        Self {
            held: [None; SLOTS],
            next_tracking: 1,
        }
    }
}

impl Slots {
    /// Reconcile against the complete set of fingers now down, returning the
    /// events for one evdev frame, `SYN_REPORT` included.
    pub fn frame(&mut self, points: &[TouchPoint]) -> Vec<Ev> {
        let mut out = Vec::new();

        // Lift first, so a finger that goes up in the same frame another comes
        // down frees its slot before the new one looks for a free one.
        for slot in 0..SLOTS {
            let Some(id) = self.held[slot] else { continue };
            if !points.iter().any(|p| p.id == id) {
                out.push(Ev::new(EV_ABS, AbsoluteAxis::MultitouchSlot as u16, slot as i32));
                out.push(Ev::new(
                    EV_ABS,
                    AbsoluteAxis::MultitouchTrackingId as u16,
                    -1,
                ));
                self.held[slot] = None;
            }
        }

        for p in points {
            let existing = self.held.iter().position(|h| *h == Some(p.id));
            let (slot, fresh) = match existing {
                Some(s) => (s, false),
                None => match self.held.iter().position(Option::is_none) {
                    Some(s) => {
                        self.held[s] = Some(p.id);
                        (s, true)
                    }
                    // More fingers than slots. The plugin refuses these, so
                    // reaching here means the device shrank, not a bad peer.
                    None => continue,
                },
            };
            out.push(Ev::new(EV_ABS, AbsoluteAxis::MultitouchSlot as u16, slot as i32));
            if fresh {
                out.push(Ev::new(
                    EV_ABS,
                    AbsoluteAxis::MultitouchTrackingId as u16,
                    self.next_tracking,
                ));
                // Never reused while the device lives: libinput tells a new
                // contact from a moved one by the id changing.
                self.next_tracking = self.next_tracking.wrapping_add(1).max(1);
            }
            out.push(Ev::new(
                EV_ABS,
                AbsoluteAxis::MultitouchPositionX as u16,
                i32::from(p.x),
            ));
            out.push(Ev::new(
                EV_ABS,
                AbsoluteAxis::MultitouchPositionY as u16,
                i32::from(p.y),
            ));
        }

        let down = self.held.iter().filter(|h| h.is_some()).count();

        // Single-touch axes track the lowest occupied slot, which is what a
        // driverless reader and libinput's fallback path both expect.
        if let Some(first) = self.held.iter().position(Option::is_some)
            && let Some(p) = points.iter().find(|p| Some(p.id) == self.held[first])
        {
            out.push(Ev::new(EV_ABS, AbsoluteAxis::X as u16, i32::from(p.x)));
            out.push(Ev::new(EV_ABS, AbsoluteAxis::Y as u16, i32::from(p.y)));
        }

        out.push(Ev::new(EV_KEY, Key::ButtonTouch as u16, i32::from(down > 0)));
        // More fingers than tool keys saturates at the highest one, rather
        // than leaving every bit clear with `BTN_TOUCH` still set.
        let capped = down.min(TOOL_KEYS.len());
        for (n, key) in TOOL_KEYS.iter().enumerate() {
            out.push(Ev::new(EV_KEY, *key as u16, i32::from(capped == n + 1)));
        }

        // Exactly one per frame: a report per slot would read as N separate
        // one-finger updates and no gesture would ever be recognised.
        out.push(Ev::new(EV_SYN, sys::SYN_REPORT as u16, 0));
        out
    }

    /// Events lifting every finger, for a stream that ended or a link that died.
    pub fn release_all(&mut self) -> Vec<Ev> {
        self.frame(&[])
    }
}

const TOOL_KEYS: [Key; 5] = [
    Key::ButtonToolFinger,
    Key::ButtonToolDoubleTap,
    Key::ButtonToolTripleTap,
    Key::ButtonToolQuadtap,
    Key::ButtonToolQuintTap,
];

/// Whether this machine can host the device at all. Opens and closes rather than
/// stat-ing, because permission is the usual reason it cannot, not absence.
#[must_use]
pub fn available() -> bool {
    OpenOptions::new().write(true).open(UINPUT).is_ok()
}

pub struct Device {
    handle: UInputHandle<File>,
    slots: Slots,
}

impl Device {
    pub fn create(w_mm: u16, h_mm: u16) -> std::io::Result<Self> {
        let file = OpenOptions::new().write(true).open(UINPUT)?;
        let handle = UInputHandle::new(file);

        handle.set_evbit(EventKind::Synchronize)?;
        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Absolute)?;

        handle.set_keybit(Key::ButtonTouch)?;
        for key in TOOL_KEYS {
            handle.set_keybit(key)?;
        }

        // No `ButtonLeft`, deliberately: libinput enables tap-to-click by
        // default exactly when a touchpad has no physical button.
        handle.set_propbit(InputProperty::Pointer)?;

        for axis in [
            AbsoluteAxis::X,
            AbsoluteAxis::Y,
            AbsoluteAxis::MultitouchSlot,
            AbsoluteAxis::MultitouchTrackingId,
            AbsoluteAxis::MultitouchPositionX,
            AbsoluteAxis::MultitouchPositionY,
        ] {
            handle.set_absbit(axis)?;
        }

        // Resolution is the only place the surface's real size enters, and
        // libinput measures scroll and pinch thresholds in millimetres.
        let res_x = MAX_POS / i32::from(w_mm.max(1));
        let res_y = MAX_POS / i32::from(h_mm.max(1));
        let pos = |axis, resolution| AbsoluteInfoSetup {
            axis,
            info: AbsoluteInfo {
                maximum: MAX_POS,
                resolution,
                ..AbsoluteInfo::default()
            },
        };
        let abs = [
            pos(AbsoluteAxis::X, res_x),
            pos(AbsoluteAxis::Y, res_y),
            pos(AbsoluteAxis::MultitouchPositionX, res_x),
            pos(AbsoluteAxis::MultitouchPositionY, res_y),
            AbsoluteInfoSetup {
                axis: AbsoluteAxis::MultitouchSlot,
                info: AbsoluteInfo {
                    maximum: SLOTS as i32 - 1,
                    ..AbsoluteInfo::default()
                },
            },
            AbsoluteInfoSetup {
                axis: AbsoluteAxis::MultitouchTrackingId,
                info: AbsoluteInfo {
                    maximum: MAX_POS,
                    ..AbsoluteInfo::default()
                },
            },
        ];

        let id = InputId {
            bustype: sys::BUS_VIRTUAL,
            vendor: 0xac71,
            product: 0x0001,
            version: 1,
        };
        handle.create(&id, NAME, 0, &abs)?;

        Ok(Self {
            handle,
            slots: Slots::default(),
        })
    }

    fn emit(&self, events: &[Ev]) -> std::io::Result<()> {
        let raw: Vec<sys::input_event> = events.iter().map(|e| e.to_input_event()).collect();
        self.handle.write(&raw)?;
        Ok(())
    }

    pub fn apply(&mut self, points: &[TouchPoint]) -> std::io::Result<()> {
        let events = self.slots.frame(points);
        self.emit(&events)
    }

    pub fn release_all(&mut self) -> std::io::Result<()> {
        let events = self.slots.release_all();
        self.emit(&events)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.release_all();
        let _ = self.handle.dev_destroy();
    }
}

/// Whether an op stamped `order` beats the highest order applied so far.
/// A free function so the off-by-one is testable without a real device.
#[must_use]
pub fn should_apply(last_applied: u32, order: u32) -> bool {
    order > last_applied
}

/// Owns the device across effects, each run in its own task and able to
/// reorder; the lock plus `last_applied` is what makes that safe.
#[derive(Default)]
pub struct TouchpadEffector {
    state: tokio::sync::Mutex<State>,
}

#[derive(Default)]
struct State {
    open: Option<Device>,
    last_applied: u32,
}

impl TouchpadEffector {
    pub async fn run(&self, order: u32, op: TouchpadOp) -> Result<(), String> {
        let mut state = self.state.lock().await;
        if !should_apply(state.last_applied, order) {
            return Ok(());
        }
        state.last_applied = order;
        match op {
            TouchpadOp::Begin { w_mm, h_mm } => {
                state.open = Some(Device::create(w_mm, h_mm).map_err(|e| e.to_string())?);
            }
            TouchpadOp::Frame { points } => {
                // A frame that beat its own `begin` here, or one after `end`:
                // the next frame carries the whole set again.
                if let Some(device) = state.open.as_mut() {
                    device.apply(&points).map_err(|e| e.to_string())?;
                }
            }
            TouchpadOp::End => {
                state.open = None;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(id: u8, x: u16, y: u16) -> TouchPoint {
        TouchPoint { id, x, y }
    }

    fn abs(events: &[Ev], code: AbsoluteAxis) -> Vec<i32> {
        events
            .iter()
            .filter(|e| e.kind == EV_ABS && e.code == code as u16)
            .map(|e| e.value)
            .collect()
    }

    fn key(events: &[Ev], code: Key) -> Vec<i32> {
        events
            .iter()
            .filter(|e| e.kind == EV_KEY && e.code == code as u16)
            .map(|e| e.value)
            .collect()
    }

    #[test]
    fn a_new_finger_takes_the_lowest_free_slot_with_a_fresh_tracking_id() {
        let mut s = Slots::default();
        let f = s.frame(&[point(7, 100, 200)]);
        assert_eq!(abs(&f, AbsoluteAxis::MultitouchSlot), vec![0]);
        assert_eq!(abs(&f, AbsoluteAxis::MultitouchTrackingId), vec![1]);
        assert_eq!(abs(&f, AbsoluteAxis::MultitouchPositionX), vec![100]);
        assert_eq!(abs(&f, AbsoluteAxis::MultitouchPositionY), vec![200]);
    }

    #[test]
    fn a_finger_that_stays_keeps_its_slot_and_gets_no_new_tracking_id() {
        let mut s = Slots::default();
        s.frame(&[point(7, 100, 200)]);
        let f = s.frame(&[point(7, 150, 200)]);
        assert_eq!(abs(&f, AbsoluteAxis::MultitouchSlot), vec![0]);
        assert!(abs(&f, AbsoluteAxis::MultitouchTrackingId).is_empty());
        assert_eq!(abs(&f, AbsoluteAxis::MultitouchPositionX), vec![150]);
    }

    #[test]
    fn a_finger_missing_from_the_next_frame_lifts_its_slot() {
        let mut s = Slots::default();
        s.frame(&[point(7, 100, 200), point(8, 300, 400)]);
        let f = s.frame(&[point(8, 300, 400)]);
        assert_eq!(abs(&f, AbsoluteAxis::MultitouchTrackingId), vec![-1]);
        assert_eq!(abs(&f, AbsoluteAxis::MultitouchSlot), vec![0, 1]);
        assert_eq!(s.held, [None, Some(8), None, None, None, None, None, None, None, None]);
    }

    #[test]
    fn an_empty_frame_lifts_everything_and_clears_btn_touch() {
        let mut s = Slots::default();
        s.frame(&[point(1, 10, 10), point(2, 20, 20)]);
        let f = s.frame(&[]);
        assert_eq!(abs(&f, AbsoluteAxis::MultitouchTrackingId), vec![-1, -1]);
        assert_eq!(key(&f, Key::ButtonTouch), vec![0]);
        assert!(s.held.iter().all(Option::is_none));
    }

    #[test]
    fn the_finger_count_picks_exactly_one_tool_key() {
        let mut s = Slots::default();
        let f = s.frame(&[point(1, 1, 1), point(2, 2, 2), point(3, 3, 3)]);
        assert_eq!(key(&f, Key::ButtonToolFinger), vec![0]);
        assert_eq!(key(&f, Key::ButtonToolDoubleTap), vec![0]);
        assert_eq!(key(&f, Key::ButtonToolTripleTap), vec![1]);
        assert_eq!(key(&f, Key::ButtonToolQuadtap), vec![0]);
    }

    #[test]
    fn tracking_ids_are_never_reused_while_the_device_lives() {
        let mut s = Slots::default();
        s.frame(&[point(1, 1, 1)]);
        s.frame(&[]);
        let f = s.frame(&[point(1, 1, 1)]);
        assert_eq!(abs(&f, AbsoluteAxis::MultitouchTrackingId), vec![2]);
    }

    #[test]
    fn a_lift_and_a_touch_in_one_frame_let_the_new_finger_reuse_the_slot() {
        let mut s = Slots::default();
        s.frame(&[point(1, 1, 1)]);
        let f = s.frame(&[point(2, 5, 5)]);
        assert_eq!(abs(&f, AbsoluteAxis::MultitouchSlot), vec![0, 0]);
        assert_eq!(abs(&f, AbsoluteAxis::MultitouchTrackingId), vec![-1, 2]);
        assert_eq!(s.held[0], Some(2));
    }

    #[test]
    fn every_frame_ends_in_exactly_one_report() {
        let mut s = Slots::default();
        let f = s.frame(&[point(1, 1, 1), point(2, 2, 2)]);
        let syns = f.iter().filter(|e| e.kind == EV_SYN).count();
        assert_eq!(syns, 1);
        assert_eq!(f.last().unwrap().kind, EV_SYN);
    }

    #[test]
    fn ten_fingers_fill_every_slot_and_an_eleventh_is_dropped() {
        let mut s = Slots::default();
        let points: Vec<_> = (0..11).map(|i| point(i, 0, 0)).collect();
        s.frame(&points);
        assert!(s.held.iter().all(Option::is_some));
        assert!(!s.held.contains(&Some(10)));
    }

    #[test]
    fn the_single_touch_axes_follow_the_lowest_occupied_slot() {
        let mut s = Slots::default();
        s.frame(&[point(1, 100, 100), point(2, 900, 900)]);
        let f = s.frame(&[point(2, 900, 900)]);
        assert_eq!(abs(&f, AbsoluteAxis::X), vec![900]);
        assert_eq!(abs(&f, AbsoluteAxis::Y), vec![900]);
    }

    #[test]
    fn more_fingers_than_tool_keys_saturates_at_the_last_one() {
        let mut s = Slots::default();
        let points: Vec<_> = (0..7).map(|i| point(i, 0, 0)).collect();
        let f = s.frame(&points);
        assert_eq!(key(&f, Key::ButtonToolQuintTap), vec![1]);
        assert_eq!(key(&f, Key::ButtonToolQuadtap), vec![0]);
        // BTN_TOUCH stays asserted even past what any tool key can name.
        assert_eq!(key(&f, Key::ButtonTouch), vec![1]);
    }

    #[test]
    fn should_apply_only_lets_a_strictly_newer_order_through() {
        assert!(should_apply(0, 1));
        assert!(should_apply(5, 6));
        assert!(!should_apply(5, 5));
        assert!(!should_apply(5, 4));
    }

    #[test]
    fn release_all_is_the_same_as_an_empty_frame() {
        let mut s = Slots::default();
        s.frame(&[point(1, 1, 1)]);
        let f = s.release_all();
        assert_eq!(key(&f, Key::ButtonTouch), vec![0]);
        assert!(s.held.iter().all(Option::is_none));
    }
}
