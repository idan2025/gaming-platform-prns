//! Gamepads read by the launcher itself (`ROADMAP.md` item 10), for the
//! webviews that do not read them.
//!
//! Big screen mode is driven through the page's Gamepad API, and on Linux
//! WebKitGTK answers it only when it was built with libmanette, which a
//! Steam Deck's or a distribution's build may not be. So on Linux the
//! launcher reads `/dev/input/event*` itself and tells the page which
//! buttons are held, by the same names the page uses (`app.js` `PAD`). The
//! page takes the union with what the webview reports, so a pad seen both
//! ways is one pad, and a press is never counted twice.
//!
//! Only reading, never grabbing: the game and Steam still get every event.
//! A device is a gamepad if it has a south face button (`BTN_SOUTH`), the
//! kernel's word for A; keyboards and mice are never opened.

use std::collections::BTreeSet;

/// A gamepad's buttons by the names the page uses.
pub fn button_name(code: u16) -> Option<&'static str> {
    Some(match code {
        0x130 => "A",     // BTN_SOUTH
        0x131 => "B",     // BTN_EAST
        0x133 => "X",     // BTN_X (xpad's X, the left face button)
        0x134 => "Y",     // BTN_Y
        0x136 => "LB",    // BTN_TL
        0x137 => "RB",    // BTN_TR
        0x13a => "BACK",  // BTN_SELECT
        0x13b => "START", // BTN_START
        0x220 => "UP",    // BTN_DPAD_UP
        0x221 => "DOWN",
        0x222 => "LEFT",
        0x223 => "RIGHT",
        _ => return None,
    })
}

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;

/// How far a stick must lean to count as a direction, as the page's own
/// threshold (`app.js`, 0.6).
const STICK_THRESHOLD: f32 = 0.6;

/// One pad's state: the buttons held and where its stick and d-pad hat lean.
#[derive(Debug, Default, Clone)]
pub struct PadState {
    buttons: BTreeSet<&'static str>,
    /// Left stick and hat, each -1, 0 or 1 per axis.
    stick: (i8, i8),
    hat: (i8, i8),
}

impl PadState {
    pub fn key(&mut self, code: u16, value: i32) {
        if let Some(name) = button_name(code) {
            // 1 is a press and 2 an autorepeat; 0 lets go.
            if value == 0 {
                self.buttons.remove(name);
            } else {
                self.buttons.insert(name);
            }
        }
    }

    /// An absolute axis moved. `min` and `max` are the device's range for it.
    pub fn axis(&mut self, code: u16, value: i32, min: i32, max: i32) {
        let lean = |v: i32| -> i8 {
            if max <= min {
                return 0;
            }
            let centre = (min as f32 + max as f32) / 2.0;
            let half = (max as f32 - min as f32) / 2.0;
            let x = (v as f32 - centre) / half;
            if x <= -STICK_THRESHOLD {
                -1
            } else if x >= STICK_THRESHOLD {
                1
            } else {
                0
            }
        };
        match code {
            ABS_X => self.stick.0 = lean(value),
            ABS_Y => self.stick.1 = lean(value),
            ABS_HAT0X => self.hat.0 = value.signum() as i8,
            ABS_HAT0Y => self.hat.1 = value.signum() as i8,
            _ => {}
        }
    }

    /// Everything held, d-pad directions from the hat and the stick included.
    pub fn held(&self) -> BTreeSet<&'static str> {
        let mut out = self.buttons.clone();
        for (x, y) in [self.stick, self.hat] {
            if x < 0 {
                out.insert("LEFT");
            }
            if x > 0 {
                out.insert("RIGHT");
            }
            if y < 0 {
                out.insert("UP");
            }
            if y > 0 {
                out.insert("DOWN");
            }
        }
        out
    }
}

/// What the page is told: how many pads are connected and what they hold.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PadReport {
    pub connected: usize,
    pub held: Vec<&'static str>,
}

/// Read every gamepad on this machine on a thread of its own, and call
/// `report` whenever what is connected or held changes. Linux only; a
/// no-op elsewhere, where the webview reads gamepads itself.
pub fn watch<F: Fn(PadReport) + Send + 'static>(report: F) {
    #[cfg(target_os = "linux")]
    {
        let _ = std::thread::Builder::new().name("gamepads".into()).spawn(move || linux::run(report));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = report;
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{PadReport, PadState};
    use evdev::{Device, EventType, KeyCode};
    use std::collections::{BTreeSet, HashMap};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const RESCAN: Duration = Duration::from_secs(2);
    const TICK: Duration = Duration::from_millis(16);

    struct Pad {
        device: Device,
        state: PadState,
        /// Each absolute axis's range, read once when the pad is opened.
        ranges: HashMap<u16, (i32, i32)>,
    }

    fn is_gamepad(device: &Device) -> bool {
        device.supported_keys().is_some_and(|k| k.contains(KeyCode::BTN_SOUTH))
    }

    fn open(path: &PathBuf) -> Option<Pad> {
        let device = Device::open(path).ok()?;
        if !is_gamepad(&device) {
            return None;
        }
        device.set_nonblocking(true).ok()?;
        let ranges = device
            .get_absinfo()
            .map(|it| it.map(|(axis, info)| (axis.0, (info.minimum(), info.maximum()))).collect())
            .unwrap_or_default();
        Some(Pad { device, state: PadState::default(), ranges })
    }

    pub fn run<F: Fn(PadReport)>(report: F) {
        let mut pads: HashMap<PathBuf, Pad> = HashMap::new();
        let mut last = PadReport { connected: 0, held: Vec::new() };
        let mut scanned: Option<Instant> = None;
        loop {
            if scanned.is_none_or(|t| t.elapsed() >= RESCAN) {
                scanned = Some(Instant::now());
                if let Ok(dir) = std::fs::read_dir("/dev/input") {
                    for entry in dir.flatten() {
                        let path = entry.path();
                        let is_event = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("event"));
                        if is_event && !pads.contains_key(&path) {
                            if let Some(pad) = open(&path) {
                                tracing::info!(path = %path.display(), name = ?pad.device.name(), "gamepad found");
                                pads.insert(path, pad);
                            }
                        }
                    }
                }
            }
            let mut gone = Vec::new();
            for (path, pad) in pads.iter_mut() {
                match pad.device.fetch_events() {
                    Ok(events) => {
                        for e in events {
                            if e.event_type() == EventType::KEY {
                                pad.state.key(e.code(), e.value());
                            } else if e.event_type() == EventType::ABSOLUTE {
                                let (min, max) = pad.ranges.get(&e.code()).copied().unwrap_or((-1, 1));
                                pad.state.axis(e.code(), e.value(), min, max);
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    // Unplugged, or no longer ours to read.
                    Err(_) => gone.push(path.clone()),
                }
            }
            for path in gone {
                pads.remove(&path);
            }
            let held: BTreeSet<&'static str> = pads.values().flat_map(|p| p.state.held()).collect();
            let now = PadReport { connected: pads.len(), held: held.into_iter().collect() };
            if now != last {
                report(now.clone());
                last = now;
            }
            std::thread::sleep(TICK);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_sticks_and_the_hat_become_the_pages_names() {
        let mut pad = PadState::default();
        pad.key(0x130, 1);
        pad.key(0x13b, 1);
        assert_eq!(pad.held().into_iter().collect::<Vec<_>>(), vec!["A", "START"]);
        pad.key(0x130, 0);
        pad.key(0x13b, 2); // autorepeat is still held
        assert_eq!(pad.held().into_iter().collect::<Vec<_>>(), vec!["START"]);

        // A Steam Deck's stick is -32768..32767; a lean past 0.6 is a direction.
        let mut pad = PadState::default();
        pad.axis(ABS_Y, -30000, -32768, 32767);
        assert!(pad.held().contains("UP"));
        pad.axis(ABS_Y, -10000, -32768, 32767);
        assert!(pad.held().is_empty(), "a small lean is no press");
        // An 8-bit pad's centre is 128, not 0.
        pad.axis(ABS_X, 128, 0, 255);
        assert!(pad.held().is_empty());
        pad.axis(ABS_X, 250, 0, 255);
        assert!(pad.held().contains("RIGHT"));

        let mut pad = PadState::default();
        pad.axis(ABS_HAT0Y, 1, -1, 1);
        assert_eq!(pad.held().into_iter().collect::<Vec<_>>(), vec!["DOWN"]);
        // Keys a gamepad does not have are not invented.
        pad.key(0x1e, 1); // KEY_A on a keyboard
        assert_eq!(pad.held().len(), 1);
    }
}
