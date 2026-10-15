//! Whether the person at this computer is busy with something fullscreen
//! (ROADMAP item 5): a game, a presentation. A desktop notification then
//! waits rather than drawing over the game.
//!
//! Windows says so directly (`SHQueryUserNotificationState`). On Linux the
//! window manager says it through two standard properties: the active
//! window (`_NET_ACTIVE_WINDOW`) and whether it is fullscreen
//! (`_NET_WM_STATE_FULLSCREEN`), read with `xprop` in its own process. That
//! covers an X11 desktop, and a game under XWayland, which is how Proton and
//! most native games run on a Wayland desktop; where there is no answer it
//! is false and the desktop decides, as macOS does.

/// True while a fullscreen game or presentation has the screen.
pub fn user_is_busy() -> bool {
    imp::user_is_busy()
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::UI::Shell::{
        SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE,
        QUNS_RUNNING_D3D_FULL_SCREEN,
    };

    pub fn user_is_busy() -> bool {
        let mut state = 0;
        // SAFETY: one out-parameter, written by the call.
        let hr = unsafe { SHQueryUserNotificationState(&mut state) };
        hr >= 0 && is_busy(state)
    }

    pub fn is_busy(state: i32) -> bool {
        matches!(state, QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// In its own process on purpose: Xlib's error handler is the whole
    /// process's, GTK owns it in the launcher, and a window that closes
    /// between the two questions is an X error.
    pub fn user_is_busy() -> bool {
        if std::env::var_os("DISPLAY").is_none() {
            return false;
        }
        let Some(root) = xprop(&["-root", "_NET_ACTIVE_WINDOW"]) else { return false };
        let Some(window) = super::active_window(&root) else { return false };
        xprop(&["-id", &format!("{window:#x}"), "_NET_WM_STATE"]).is_some_and(|s| super::is_fullscreen(&s))
    }

    /// `xprop`'s answer, or `None` when it is missing, fails or takes more
    /// than a second: a notification is never held up by the question.
    fn xprop(args: &[&str]) -> Option<String> {
        let mut child =
            Command::new("xprop").args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match child.try_wait().ok()? {
                Some(status) if status.success() => break,
                Some(_) => return None,
                None if Instant::now() > deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        let mut out = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
        Some(out)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    pub fn user_is_busy() -> bool {
        false
    }
}

/// The window `xprop -root _NET_ACTIVE_WINDOW` names, if any. A window
/// manager writes `_NET_ACTIVE_WINDOW(WINDOW): window id # 0x3a00007`; zero
/// means nothing is active.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn active_window(xprop: &str) -> Option<u64> {
    let value = xprop.rsplit(['#', '=']).next()?.trim();
    let id = match value.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok()?,
        None => value.parse().ok()?,
    };
    (id != 0).then_some(id)
}

/// Whether `xprop -id <w> _NET_WM_STATE` lists fullscreen.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
fn is_fullscreen(xprop: &str) -> bool {
    xprop.split(['=', ',']).skip(1).any(|a| a.trim() == "_NET_WM_STATE_FULLSCREEN")
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn the_window_managers_words_are_read_and_nothing_else() {
        assert_eq!(active_window("_NET_ACTIVE_WINDOW(WINDOW): window id # 0x3a00007\n"), Some(0x3a00007));
        assert_eq!(active_window("_NET_ACTIVE_WINDOW(CARDINAL) = 1293\n"), Some(1293));
        assert_eq!(active_window("_NET_ACTIVE_WINDOW(WINDOW): window id # 0x0\n"), None);
        assert_eq!(active_window("_NET_ACTIVE_WINDOW:  no such atom on any window.\n"), None);
        assert!(is_fullscreen("_NET_WM_STATE(ATOM) = _NET_WM_STATE_FOCUSED, _NET_WM_STATE_FULLSCREEN\n"));
        assert!(!is_fullscreen("_NET_WM_STATE(ATOM) = _NET_WM_STATE_MAXIMIZED_VERT, _NET_WM_STATE_MAXIMIZED_HORZ\n"));
        assert!(!is_fullscreen("_NET_WM_STATE:  not found.\n"));
        // A name that only contains the word is not the flag.
        assert!(!is_fullscreen("_NET_WM_STATE(ATOM) = _NET_WM_STATE_FULLSCREEN_ISH\n"));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use windows_sys::Win32::UI::Shell::{QUNS_ACCEPTS_NOTIFICATIONS, QUNS_RUNNING_D3D_FULL_SCREEN};

    #[test]
    fn a_fullscreen_game_holds_notifications_back_and_a_free_desktop_does_not() {
        assert!(super::imp::is_busy(QUNS_RUNNING_D3D_FULL_SCREEN));
        assert!(!super::imp::is_busy(QUNS_ACCEPTS_NOTIFICATIONS));
        // Whatever this machine's state, asking must not fail or hang.
        let _ = super::user_is_busy();
    }
}
