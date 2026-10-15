//! `desktop::user_is_busy` on Linux, against a real X server (Xvfb): the
//! active window being fullscreen holds notifications back, and anything
//! else lets them through. Skips itself where Xvfb or xprop is missing.
#![cfg(all(unix, not(target_os = "macos")))]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn have(program: &str) -> bool {
    Command::new("sh").args(["-c", &format!("command -v {program}")]).stdout(Stdio::null()).status().is_ok_and(|s| s.success())
}

struct Xvfb(std::process::Child);

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn xprop(display: &str, args: &[&str]) -> bool {
    Command::new("xprop").env("DISPLAY", display).args(args).status().is_ok_and(|s| s.success())
}

#[test]
fn a_fullscreen_active_window_holds_notifications_back() {
    if !have("Xvfb") || !have("xprop") {
        eprintln!("skipping: no Xvfb or xprop");
        return;
    }
    let display = format!(":{}", 90 + std::process::id() % 100);
    // -noreset: properties set between clients must survive.
    let _server = Xvfb(
        Command::new("Xvfb").args([&display, "-noreset", "-nolisten", "tcp"]).stderr(Stdio::null()).spawn().unwrap(),
    );
    let up = Instant::now() + Duration::from_secs(10);
    while !Command::new("xprop").env("DISPLAY", &display).args(["-root", "-len", "0"]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()) {
        assert!(Instant::now() < up, "Xvfb never came up");
        std::thread::sleep(Duration::from_millis(100));
    }
    // SAFETY of set_var: this test binary runs this one test only.
    std::env::set_var("DISPLAY", &display);
    assert!(!launcher_core::desktop::user_is_busy(), "no active window");

    // The root window stands in for a game's: it is the one window an X
    // server always has.
    let out = Command::new("xwininfo").env("DISPLAY", &display).arg("-root").output();
    let Some(id) = out.ok().and_then(|o| {
        String::from_utf8_lossy(&o.stdout).lines().find_map(|l| l.split("Window id: ").nth(1)?.split_whitespace().next().map(str::to_string))
    }) else {
        eprintln!("skipping: no xwininfo");
        return;
    };
    assert!(xprop(&display, &["-root", "-f", "_NET_ACTIVE_WINDOW", "32x", " window id # $0", "-set", "_NET_ACTIVE_WINDOW", &id]));
    assert!(xprop(&display, &["-root", "-f", "_NET_WM_STATE", "32a", " = $0+", "-set", "_NET_WM_STATE", "_NET_WM_STATE_MAXIMIZED_VERT"]));
    assert!(!launcher_core::desktop::user_is_busy(), "maximized is not fullscreen");
    assert!(xprop(&display, &["-root", "-f", "_NET_WM_STATE", "32a", " = $0+", "-set", "_NET_WM_STATE", "_NET_WM_STATE_FULLSCREEN"]));
    assert!(launcher_core::desktop::user_is_busy(), "a fullscreen game has the screen");

    // A window that is gone by the time it is asked about is not busy, and
    // the question does not fail.
    assert!(xprop(&display, &["-root", "-f", "_NET_ACTIVE_WINDOW", "32x", " window id # $0", "-set", "_NET_ACTIVE_WINDOW", "0x7777777"]));
    assert!(!launcher_core::desktop::user_is_busy());
}
