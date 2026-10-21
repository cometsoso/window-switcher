use anyhow::{anyhow, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use windows::Win32::{
    Foundation::HWND,
    System::RemoteDesktop::{
        WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
    },
    System::Rpc::RPC_S_INVALID_BINDING,
    UI::WindowsAndMessaging::{
        KillTimer, SetTimer, WTS_CONSOLE_CONNECT, WTS_CONSOLE_DISCONNECT, WTS_REMOTE_CONNECT,
        WTS_REMOTE_DISCONNECT, WTS_SESSION_LOCK, WTS_SESSION_LOGOFF, WTS_SESSION_LOGON,
        WTS_SESSION_UNLOCK,
    },
};

pub const REGISTRATION_TIMER: usize = 0x5754_5301;

/// Independent of App's storage and mutable borrows across Win32 callbacks.
pub struct InputSessionEpoch(AtomicU64);

impl InputSessionEpoch {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn capture(&self) -> InputSessionToken<'_> {
        InputSessionToken {
            epoch: self,
            generation: self.0.load(Ordering::SeqCst),
        }
    }

    pub fn invalidate(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy)]
pub struct InputSessionToken<'a> {
    epoch: &'a InputSessionEpoch,
    generation: u64,
}

impl InputSessionToken<'_> {
    pub fn generation(self) -> u64 {
        self.generation
    }
    pub fn is_current(self) -> bool {
        self.epoch.0.load(Ordering::SeqCst) == self.generation
    }

    pub fn run_if_current<T>(self, action: impl FnOnce() -> T) -> Option<T> {
        self.is_current().then(action)
    }
}

pub static INPUT_SESSION_EPOCH: InputSessionEpoch = InputSessionEpoch::new();

#[derive(Default)]
pub struct Availability {
    locked: bool,
    disconnected: bool,
}

impl Availability {
    pub fn change(&mut self, event: u32) -> Option<bool> {
        let suspended = transition(event)?;
        match event {
            WTS_SESSION_LOCK | WTS_SESSION_UNLOCK | WTS_SESSION_LOGOFF | WTS_SESSION_LOGON => {
                self.locked = suspended
            }
            _ => self.disconnected = suspended,
        }
        Some(self.locked || self.disconnected)
    }
}

/// Owns registration and its startup retry timer on the window's UI thread.
pub struct SessionNotifications {
    hwnd: HWND,
    registered: bool,
}

impl SessionNotifications {
    pub fn new(hwnd: HWND) -> Result<Self> {
        let mut notifications = Self {
            hwnd,
            registered: false,
        };
        notifications.retry()?;
        Ok(notifications)
    }

    pub fn retry(&mut self) -> Result<()> {
        if self.registered {
            return Ok(());
        }
        match unsafe { WTSRegisterSessionNotification(self.hwnd, NOTIFY_FOR_THIS_SESSION) } {
            Ok(()) => {
                self.registered = true;
                let _ = unsafe { KillTimer(Some(self.hwnd), REGISTRATION_TIMER) };
                info!("session notifications registered");
                Ok(())
            }
            Err(err)
                if err.code()
                    == windows::core::HRESULT::from_win32(RPC_S_INVALID_BINDING.0 as u32) =>
            {
                // At early logon Remote Desktop Services can still be starting.
                // Retry on the message loop without blocking it or leaking a worker thread.
                warn!("session notification service not ready: {err}; retrying in 3 seconds");
                if unsafe { SetTimer(Some(self.hwnd), REGISTRATION_TIMER, 3000, None) } == 0 {
                    return Err(anyhow!(
                        "Failed to schedule session notification registration retry"
                    ));
                }
                Ok(())
            }
            Err(err) => Err(anyhow!("Failed to register session notifications: {err}")),
        }
    }
}

impl Drop for SessionNotifications {
    fn drop(&mut self) {
        let _ = unsafe { KillTimer(Some(self.hwnd), REGISTRATION_TIMER) };
        if self.registered {
            if let Err(err) = unsafe { WTSUnRegisterSessionNotification(self.hwnd) } {
                warn!("Failed to unregister session notifications: {err}");
            }
        }
    }
}

/// Some(true) suspends hotkeys; Some(false) resumes with a fresh session.
pub fn transition(event: u32) -> Option<bool> {
    match event {
        WTS_CONSOLE_DISCONNECT | WTS_REMOTE_DISCONNECT | WTS_SESSION_LOGOFF | WTS_SESSION_LOCK => {
            Some(true)
        }
        WTS_CONSOLE_CONNECT | WTS_REMOTE_CONNECT | WTS_SESSION_LOGON | WTS_SESSION_UNLOCK => {
            Some(false)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_does_not_enable_hotkeys_while_locked() {
        let mut availability = Availability::default();
        assert_eq!(availability.change(WTS_SESSION_LOCK), Some(true));
        assert_eq!(availability.change(WTS_REMOTE_DISCONNECT), Some(true));
        assert_eq!(availability.change(WTS_REMOTE_CONNECT), Some(true));
        assert_eq!(availability.change(WTS_SESSION_UNLOCK), Some(false));
        assert_eq!(availability.change(WTS_CONSOLE_DISCONNECT), Some(true));
        assert_eq!(availability.change(WTS_SESSION_UNLOCK), Some(true));
        assert_eq!(availability.change(WTS_CONSOLE_CONNECT), Some(false));
        assert_eq!(availability.change(9), None);
    }

    #[test]
    fn lock_and_disconnect_suspend_unlock_and_reconnect_reset() {
        // WTS_CONSOLE/REMOTE_CONNECT, DISCONNECT, LOGON/OFF, LOCK/UNLOCK.
        for event in [2, 4, 6, 7] {
            assert_eq!(transition(event), Some(true));
        }
        for event in [1, 3, 5, 8] {
            assert_eq!(transition(event), Some(false));
        }
        for event in [0, 9, 10, 11, u32::MAX] {
            assert_eq!(transition(event), None);
        }
    }
}
