//! Whether the person at this computer is busy with something fullscreen
//! (ROADMAP item 5): a game, a presentation. A desktop notification then
//! waits rather than drawing over the game.
//!
//! Only Windows says so directly (`SHQueryUserNotificationState`). Linux
//! desktops and macOS already hold banners back over a fullscreen window
//! themselves, so there this answers false and the system decides.

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

#[cfg(not(windows))]
mod imp {
    pub fn user_is_busy() -> bool {
        false
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
