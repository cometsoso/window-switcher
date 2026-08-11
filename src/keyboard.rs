use crate::{
    app::{
        WM_USER_SWITCH_APPS, WM_USER_SWITCH_APPS_CANCEL, WM_USER_SWITCH_APPS_DONE,
        WM_USER_SWITCH_WINDOWS, WM_USER_SWITCH_WINDOWS_DONE,
    },
    config::{Hotkey, SWITCH_APPS_HOTKEY_ID, SWITCH_WINDOWS_HOTKEY_ID},
    foreground::IS_FOREGROUND_IN_BLACKLIST,
};

use anyhow::{anyhow, Result};
use indexmap::IndexSet;
use parking_lot::Mutex;
use std::{
    mem::size_of,
    sync::{
        atomic::{AtomicBool, Ordering},
        LazyLock,
    },
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
            KEYEVENTF_KEYUP, SCANCODE_LSHIFT, SCANCODE_LWIN, SCANCODE_RSHIFT, SCANCODE_RWIN,
            VK_CONTROL,
        },
        WindowsAndMessaging::{
            CallNextHookEx, SendMessageTimeoutW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK,
            KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLKHF_UP, SMTO_ABORTIFHUNG, WH_KEYBOARD_LL,
        },
    },
};

const WIN_MASK_INPUT_TAG: usize = 0x5753_4D4B;

static KEYBOARD_STATE: LazyLock<Mutex<Vec<HotKeyState>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static WIN_MASK_STATE: WinMaskState = WinMaskState::new();
static mut WINDOW: HWND = HWND(0 as _);
static mut IS_SHIFT_PRESSED: bool = false;
static mut IS_SWITCHING_APPS: bool = false;
static mut PREVIOUS_KEYCODE: u32 = 0;

struct WinMaskState {
    sent: AtomicBool,
    physical_win_pressed: AtomicBool,
}

impl WinMaskState {
    const fn new() -> Self {
        Self {
            sent: AtomicBool::new(false),
            physical_win_pressed: AtomicBool::new(false),
        }
    }

    fn claim(&self, uses_win: bool) -> bool {
        uses_win && !self.sent.swap(true, Ordering::Relaxed)
    }

    fn reset(&self) {
        self.sent.store(false, Ordering::Relaxed);
    }

    fn initialize(&self) {
        self.physical_win_pressed.store(false, Ordering::Relaxed);
        self.reset();
    }

    fn observe_physical_win(&self, is_pressed: bool) {
        if is_pressed {
            if !self.physical_win_pressed.swap(true, Ordering::Relaxed) {
                self.reset();
            }
        } else {
            self.physical_win_pressed.store(false, Ordering::Relaxed);
            self.reset();
        }
    }
}

#[derive(Debug)]
pub struct KeyboardListener {
    hook: HHOOK,
}

impl KeyboardListener {
    pub fn init(hwnd: HWND, hotkeys: &[&Hotkey]) -> Result<Self> {
        unsafe { WINDOW = hwnd }
        WIN_MASK_STATE.initialize();

        let keyboard_state = hotkeys
            .iter()
            .map(|hotkey| HotKeyState {
                hotkey: (*hotkey).clone(),
                is_modifier_pressed: false,
            })
            .collect();
        *KEYBOARD_STATE.lock() = keyboard_state;

        let hook = unsafe {
            let hinstance = { GetModuleHandleW(None) }
                .map_err(|err| anyhow!("Failed to get module handle, {err}"))?;
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_proc),
                Some(hinstance.into()),
                0,
            )
        }
        .map_err(|err| anyhow!("Failed to set windows hook, {err}"))?;
        info!("keyboard listener start");

        Ok(Self { hook })
    }
}

impl Drop for KeyboardListener {
    fn drop(&mut self) {
        debug!("keyboard listener destroyed");
        if !self.hook.is_invalid() {
            let _ = unsafe { UnhookWindowsHookEx(self.hook) };
        }
    }
}

#[derive(Debug)]
struct HotKeyState {
    hotkey: Hotkey,
    is_modifier_pressed: bool,
}

fn hotkey_uses_win(hotkey: &Hotkey) -> bool {
    hotkey.modifier == [SCANCODE_LWIN, SCANCODE_RWIN]
}

fn should_send_win_mask(state: &WinMaskState, hotkey: &Hotkey, control_is_pressed: bool) -> bool {
    state.claim(hotkey_uses_win(hotkey)) && !control_is_pressed
}

fn physical_win_state(scan_code: u32, flags: u32) -> Option<bool> {
    if ![SCANCODE_LWIN, SCANCODE_RWIN].contains(&scan_code) || flags & LLKHF_INJECTED.0 != 0 {
        return None;
    }

    Some(flags & LLKHF_UP.0 == 0)
}

fn mask_needs_key_up_recovery(sent: u32) -> bool {
    sent == 1
}

unsafe fn send_win_mask_input() {
    let key_down = KEYBDINPUT {
        wVk: VK_CONTROL,
        wScan: 0,
        dwFlags: Default::default(),
        time: 0,
        dwExtraInfo: WIN_MASK_INPUT_TAG,
    };
    let key_up = KEYBDINPUT {
        dwFlags: KEYEVENTF_KEYUP,
        ..key_down
    };
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: key_down },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: key_up },
        },
    ];

    let sent = SendInput(&inputs, size_of::<INPUT>() as i32);
    if mask_needs_key_up_recovery(sent) {
        let recovered = SendInput(&inputs[1..], size_of::<INPUT>() as i32);
        if recovered != 1 {
            warn!("failed to recover Windows-key mask Ctrl release");
        }
    }
    if sent != inputs.len() as u32 {
        warn!(
            "failed to send complete Windows-key mask input: sent {sent}/{}",
            inputs.len()
        );
    }
}

unsafe fn send_message_timeout(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) {
    let mut result: usize = 0;
    let _ = SendMessageTimeoutW(
        hwnd,
        msg,
        wparam,
        lparam,
        SMTO_ABORTIFHUNG,
        500,
        Some(&mut result as *mut _ as *mut _),
    );
}

unsafe extern "system" fn keyboard_proc(code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    let kbd_data: &KBDLLHOOKSTRUCT = &*(l_param.0 as *const _);
    if kbd_data.dwExtraInfo == WIN_MASK_INPUT_TAG {
        return CallNextHookEx(None, code, w_param, l_param);
    }
    debug!("keyboard {kbd_data:?}");
    let mut is_modifier = false;
    let scan_code = kbd_data.scanCode;
    let is_key_pressed = || kbd_data.flags.0 & LLKHF_UP.0 == 0;
    if let Some(is_pressed) = physical_win_state(scan_code, kbd_data.flags.0) {
        WIN_MASK_STATE.observe_physical_win(is_pressed);
    }
    if [SCANCODE_LSHIFT, SCANCODE_RSHIFT].contains(&scan_code) {
        IS_SHIFT_PRESSED = is_key_pressed();
    }
    let mut keyboard_state = KEYBOARD_STATE.lock();
    let mut send_done_hotkeys: IndexSet<u32> = IndexSet::new();
    let mut send_action_message: Option<(u32, isize, bool)> = None;
    let mut send_win_mask = false;

    for state in keyboard_state.iter_mut() {
        if state.hotkey.modifier.contains(&scan_code) {
            is_modifier = true;
            if is_key_pressed() {
                state.is_modifier_pressed = true;
            } else {
                state.is_modifier_pressed = false;
                if PREVIOUS_KEYCODE == state.hotkey.code {
                    send_done_hotkeys.insert(state.hotkey.id);
                }
            }
        }
    }
    if !is_modifier {
        for state in keyboard_state.iter_mut() {
            if is_key_pressed() && state.is_modifier_pressed {
                let id = state.hotkey.id;
                if scan_code == state.hotkey.code {
                    let reverse = if IS_SHIFT_PRESSED { 1 } else { 0 };
                    if id == SWITCH_APPS_HOTKEY_ID
                        || (id == SWITCH_WINDOWS_HOTKEY_ID && !IS_FOREGROUND_IN_BLACKLIST)
                    {
                        send_win_mask = should_send_win_mask(
                            &WIN_MASK_STATE,
                            &state.hotkey,
                            GetAsyncKeyState(VK_CONTROL.0 as i32) < 0,
                        );
                        send_action_message = Some((id, reverse, false));
                        PREVIOUS_KEYCODE = scan_code;
                        break;
                    };
                } else if id == SWITCH_APPS_HOTKEY_ID {
                    if scan_code == 0x01 {
                        // escape key
                        send_action_message = Some((id, 0, true));
                        PREVIOUS_KEYCODE = scan_code;
                        break;
                    } else if [0x48, 0x4b, 0x4d, 0x50].contains(&scan_code) && IS_SWITCHING_APPS {
                        // arrow keys
                        let reverse = if scan_code == 0x48 || scan_code == 0x4b {
                            1
                        } else {
                            0
                        };
                        send_action_message = Some((id, reverse, false));
                        break;
                    }
                }
            }
        }
    }
    drop(keyboard_state);

    if send_win_mask {
        send_win_mask_input();
    }

    for id in send_done_hotkeys {
        if id == SWITCH_APPS_HOTKEY_ID {
            send_message_timeout(WINDOW, WM_USER_SWITCH_APPS_DONE, WPARAM(0), LPARAM(0));
            IS_SWITCHING_APPS = false;
        } else if id == SWITCH_WINDOWS_HOTKEY_ID {
            send_message_timeout(WINDOW, WM_USER_SWITCH_WINDOWS_DONE, WPARAM(0), LPARAM(0));
        }
    }

    if let Some((id, reverse, is_cancel)) = send_action_message {
        if id == SWITCH_APPS_HOTKEY_ID {
            if is_cancel {
                send_message_timeout(WINDOW, WM_USER_SWITCH_APPS_CANCEL, WPARAM(0), LPARAM(0));
                IS_SWITCHING_APPS = false;
            } else {
                send_message_timeout(WINDOW, WM_USER_SWITCH_APPS, WPARAM(0), LPARAM(reverse));
                IS_SWITCHING_APPS = true;
            }
            return LRESULT(1);
        } else if id == SWITCH_WINDOWS_HOTKEY_ID {
            send_message_timeout(WINDOW, WM_USER_SWITCH_WINDOWS, WPARAM(0), LPARAM(reverse));
            IS_SWITCHING_APPS = false;
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, w_param, l_param)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hotkey_with_modifier(modifier: [u32; 2]) -> Hotkey {
        Hotkey {
            id: SWITCH_WINDOWS_HOTKEY_ID,
            name: "test".to_string(),
            modifier,
            code: 0x29,
        }
    }

    #[test]
    fn identifies_windows_modifier() {
        let hotkey = hotkey_with_modifier([SCANCODE_LWIN, SCANCODE_RWIN]);

        assert!(hotkey_uses_win(&hotkey));
    }

    #[test]
    fn claims_only_once_until_windows_key_is_released() {
        let state = WinMaskState::new();

        assert!(state.claim(true));
        assert!(!state.claim(true));

        state.reset();

        assert!(state.claim(true));
    }

    #[test]
    fn non_windows_modifier_does_not_claim_cycle() {
        let state = WinMaskState::new();
        let alt_hotkey = hotkey_with_modifier([0x38, 0x38]);

        assert!(!state.claim(hotkey_uses_win(&alt_hotkey)));
        assert!(state.claim(true));
    }

    #[test]
    fn held_control_claims_cycle_without_sending_mask() {
        let state = WinMaskState::new();
        let hotkey = hotkey_with_modifier([SCANCODE_LWIN, SCANCODE_RWIN]);

        assert!(!should_send_win_mask(&state, &hotkey, true));
        assert!(!should_send_win_mask(&state, &hotkey, false));
    }

    #[test]
    fn identifies_physical_windows_key_state() {
        assert_eq!(physical_win_state(SCANCODE_LWIN, LLKHF_UP.0), Some(false));
        assert_eq!(
            physical_win_state(SCANCODE_RWIN, LLKHF_UP.0 | LLKHF_INJECTED.0),
            None
        );
        assert_eq!(physical_win_state(SCANCODE_LWIN, 0), Some(true));
    }

    #[test]
    fn partial_mask_delivery_requires_control_release_recovery() {
        assert!(!mask_needs_key_up_recovery(0));
        assert!(mask_needs_key_up_recovery(1));
        assert!(!mask_needs_key_up_recovery(2));
    }

    #[test]
    fn new_physical_windows_cycle_clears_injected_stale_claim() {
        let state = WinMaskState::new();
        assert!(state.claim(true));

        state.observe_physical_win(true);

        assert!(state.claim(true));
    }
}
