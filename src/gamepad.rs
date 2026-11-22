//! evdev-based gamepad watcher: finds joystick-class devices under
//! /dev/input/event*, reports activity (buttons and stick movement) and
//! notices hotplug.
//!
//! Gamepad identification mirrors what the kernel/udev does: the device must
//! expose gamepad/joystick buttons (BTN_SOUTH/BTN_GAMEPAD family or the
//! BTN_TRIGGER joystick family) *and* absolute axes (sticks, triggers or
//! hats). That reliably matches Xbox/DualSense/8BitDo/Steam pads and steering
//! wheels while excluding keyboards, mice and tablets.
//!
//! Devices are opened non-blocking and never grabbed, so games keep receiving
//! every event while we sniff them.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use evdev::{AbsoluteAxisCode, Device, EventSummary, InputEvent, KeyCode};

use crate::journal;

/// Coarse classification of one input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvKind {
    Button,
    Axis,
    Other,
}

/// Does this event kind count as "the user is playing"?
///
/// Buttons always count. Stick/trigger movement counts unless the user asked
/// for `--buttons-only` (useful for pads that stream gyro/accelerometer
/// noise while idle). Syn/Misc/rumble/switch events never count.
pub fn counts_as_activity(kind: EvKind, buttons_only: bool) -> bool {
    match kind {
        EvKind::Button => true,
        EvKind::Axis => !buttons_only,
        EvKind::Other => false,
    }
}

fn classify(ev: InputEvent) -> EvKind {
    match ev.destructure() {
        EventSummary::Key(..) => EvKind::Button,
        EventSummary::AbsoluteAxis(..) => EvKind::Axis,
        _ => EvKind::Other,
    }
}

/// Kernel-style joystick detection.
fn is_gamepad(device: &Device) -> bool {
    let has_gamepad_buttons = device.supported_keys().is_some_and(|keys| {
        keys.contains(KeyCode::BTN_SOUTH)      // = BTN_GAMEPAD (Xbox/DualSense pads)
            || keys.contains(KeyCode::BTN_TRIGGER) // = BTN_JOYSTICK (flight sticks, wheels)
    });
    let has_axes = device
        .supported_absolute_axes()
        .is_some_and(|axes| {
            axes.contains(AbsoluteAxisCode::ABS_X)
                || axes.contains(AbsoluteAxisCode::ABS_HAT0X)
        });
    has_gamepad_buttons && has_axes
}

pub struct GamepadWatcher {
    devices: BTreeMap<PathBuf, Device>,
    last_rescan: Instant,
    rescan_interval: Duration,
}

impl GamepadWatcher {
    pub fn new(rescan_interval: Duration) -> Self {
        Self {
            devices: BTreeMap::new(),
            last_rescan: Instant::now() - rescan_interval, // scan on first poll
            rescan_interval,
        }
    }

    /// Poll all known gamepads; returns true if any activity was seen since
    /// the previous call. Rescans for hotplug every `rescan_interval`.
    pub fn poll(&mut self, buttons_only: bool) -> bool {
        if self.last_rescan.elapsed() >= self.rescan_interval {
            self.rescan();
            self.last_rescan = Instant::now();
        }

        let mut activity = false;
        let mut vanished: Vec<PathBuf> = Vec::new();
        for (path, dev) in self.devices.iter_mut() {
            loop {
                match dev.fetch_events() {
                    Ok(events) => {
                        let mut seen = 0usize;
                        for ev in events {
                            seen += 1;
                            if counts_as_activity(classify(ev), buttons_only) {
                                activity = true;
                            }
                        }
                        if seen == 0 {
                            break;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.raw_os_error() == Some(libc::ENODEV) => {
                        // Device unplugged.
                        vanished.push(path.clone());
                        break;
                    }
                    Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                        vanished.push(path.clone());
                        break;
                    }
                    Err(e) => {
                        journal::warn(&format!("error reading {path:?}: {e}; ignoring this round"));
                        break;
                    }
                }
            }
        }
        for path in vanished {
            if let Some(dev) = self.devices.remove(&path) {
                journal::info(&format!(
                    "gamepad removed: {} ({path:?})",
                    dev.name().unwrap_or("unnamed")
                ));
            }
        }
        activity
    }

    fn rescan(&mut self) {
        let mut found: Vec<(PathBuf, Device)> = Vec::new();
        for (path, dev) in evdev::enumerate() {
            if is_gamepad(&dev) && !self.devices.contains_key(&path) {
                found.push((path, dev));
            }
        }
        for (path, dev) in found {
            if let Err(e) = dev.set_nonblocking(true) {
                journal::warn(&format!("cannot use {path:?} non-blocking: {e}"));
                continue;
            }
            let name = dev.name().unwrap_or("unnamed").to_string();
            journal::info(&format!("gamepad connected: {name} ({path:?})"));
            self.devices.insert(path, dev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_always_counts() {
        assert!(counts_as_activity(EvKind::Button, false));
        assert!(counts_as_activity(EvKind::Button, true));
    }

    #[test]
    fn axis_counts_unless_buttons_only() {
        assert!(counts_as_activity(EvKind::Axis, false));
        assert!(!counts_as_activity(EvKind::Axis, true));
    }

    #[test]
    fn other_never_counts() {
        assert!(!counts_as_activity(EvKind::Other, false));
        assert!(!counts_as_activity(EvKind::Other, true));
    }
}
