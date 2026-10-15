//! `gamepad::watch` against a real (virtual) gamepad: a uinput device the
//! kernel lists in `/dev/input` like any pad. Needs write access to
//! `/dev/uinput` and read access to the new event node, so it skips itself
//! as an ordinary user; run the built test binary as root to exercise it.
#![cfg(target_os = "linux")]

use std::sync::mpsc;
use std::time::Duration;

use evdev::uinput::VirtualDevice;
use evdev::{AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode, UinputAbsSetup};
use launcher_core::gamepad::{watch, PadReport};

fn wait_for(rx: &mpsc::Receiver<PadReport>, want: impl Fn(&PadReport) -> bool) -> Option<PadReport> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(r) if want(&r) => return Some(r),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

#[test]
fn a_real_pad_is_found_read_and_let_go() {
    let keys = AttributeSet::from_iter([KeyCode::BTN_SOUTH, KeyCode::BTN_EAST, KeyCode::BTN_START]);
    let stick = UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, AbsInfo::new(0, -32768, 32767, 16, 128, 0));
    let built = VirtualDevice::builder()
        .and_then(|b| b.name("Lanthorn test pad").with_keys(&keys))
        .and_then(|b| b.with_absolute_axis(&stick))
        .and_then(|b| b.build());
    let Ok(mut pad) = built else {
        eprintln!("skipping: no write access to /dev/uinput");
        return;
    };
    let (tx, rx) = mpsc::channel();
    watch(move |r| {
        let _ = tx.send(r);
    });
    if wait_for(&rx, |r| r.connected >= 1).is_none() {
        eprintln!("skipping: the new event node is not readable here");
        return;
    }
    let emit = |pad: &mut VirtualDevice, events: &[InputEvent]| pad.emit(events).unwrap();
    emit(&mut pad, &[InputEvent::new(EventType::KEY.0, KeyCode::BTN_SOUTH.0, 1)]);
    assert!(wait_for(&rx, |r| r.held == vec!["A"]).is_some(), "A held");
    emit(&mut pad, &[InputEvent::new(EventType::KEY.0, KeyCode::BTN_SOUTH.0, 0)]);
    emit(&mut pad, &[InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, -30000)]);
    assert!(wait_for(&rx, |r| r.held == vec!["UP"]).is_some(), "the stick leaning up is UP");
    emit(&mut pad, &[InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, 0)]);
    assert!(wait_for(&rx, |r| r.held.is_empty()).is_some(), "let go");
    drop(pad);
    assert!(wait_for(&rx, |r| r.connected == 0).is_some(), "unplugged");
}
