use crate::{
    app::{
        WM_USER_RESET_INPUT_SESSION, WM_USER_SWITCH_APPS, WM_USER_SWITCH_APPS_CANCEL,
        WM_USER_SWITCH_APPS_DONE, WM_USER_SWITCH_WINDOWS, WM_USER_SWITCH_WINDOWS_DONE,
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

// Indexed by the left/right virtual keys, not scan codes (Ctrl/Alt share scan codes).
const MODIFIER_KEYS: [u16; 8] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C];

#[derive(Default, Debug)]
struct ModifierState {
    pressed: [bool; 8],
}

impl ModifierState {
    fn observe(&mut self, vk: u32, pressed: bool) -> bool {
        if let Some(index) = MODIFIER_KEYS.iter().position(|key| *key as u32 == vk) {
            self.pressed[index] = pressed;
            true
        } else {
            false
        }
    }

    fn held(&self, scan_codes: [u32; 2]) -> bool {
        let pair = match scan_codes {
            [0x2A, 0x36] => 0,
            [0x1D, 0x1D] => 2,
            [0x38, 0x38] => 4,
            [0x5B, 0x5C] => 6,
            _ => return false,
        };
        self.pressed[pair] || self.pressed[pair + 1]
    }

    fn reconcile(&mut self, physical: [bool; 8]) -> bool {
        let stale = self
            .pressed
            .iter()
            .zip(physical)
            .any(|(held, down)| *held && !down);
        self.pressed = physical;
        stale
    }
}

#[derive(Default)]
struct KeyboardSession {
    hotkeys: Vec<Hotkey>,
    modifiers: ModifierState,
    switching_apps: bool,
    previous_keycode: u32,
    blocked: bool,
    generation: u64,
}

impl KeyboardSession {
    fn reconcile(&mut self, physical: [bool; 8]) -> bool {
        if !self.modifiers.reconcile(physical) {
            return false;
        }
        self.reset(self.blocked);
        self.modifiers.pressed = physical;
        true
    }

    fn action_for_key(
        &self,
        scan_code: u32,
        pressed: bool,
        blacklisted: bool,
    ) -> Option<(usize, isize, bool)> {
        if self.blocked || !pressed {
            return None;
        }
        for (index, hotkey) in self.hotkeys.iter().enumerate() {
            if !self.modifiers.held(hotkey.modifier) {
                continue;
            }
            if scan_code == hotkey.code {
                if hotkey.id == SWITCH_APPS_HOTKEY_ID
                    || (hotkey.id == SWITCH_WINDOWS_HOTKEY_ID && !blacklisted)
                {
                    return Some((
                        index,
                        self.modifiers.held([SCANCODE_LSHIFT, SCANCODE_RSHIFT]) as isize,
                        false,
                    ));
                }
            } else if hotkey.id == SWITCH_APPS_HOTKEY_ID {
                if scan_code == 0x01 {
                    return Some((index, 0, true));
                }
                if self.switching_apps && [0x48, 0x4b, 0x4d, 0x50].contains(&scan_code) {
                    return Some((
                        index,
                        (scan_code == 0x48 || scan_code == 0x4b) as isize,
                        false,
                    ));
                }
            }
        }
        None
    }

    fn reset(&mut self, blocked: bool) {
        self.modifiers = ModifierState::default();
        self.switching_apps = false;
        self.previous_keycode = 0;
        self.blocked = blocked;
        self.generation = self.generation.wrapping_add(1);
    }
}

static KEYBOARD_STATE: LazyLock<Mutex<KeyboardSession>> =
    LazyLock::new(|| Mutex::new(KeyboardSession::default()));
static WIN_MASK_STATE: WinMaskState = WinMaskState::new();
static mut WINDOW: HWND = HWND(0 as _);

/// Called on the hook's installing (UI) thread. Never sends input or commits a selection.
pub fn reset_session(blocked: bool) {
    KEYBOARD_STATE.lock().reset(blocked);
    WIN_MASK_STATE.initialize();
}

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

        *KEYBOARD_STATE.lock() = KeyboardSession {
            hotkeys: hotkeys.iter().map(|hotkey| (*hotkey).clone()).collect(),
            ..Default::default()
        };

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

fn hotkey_uses_win(hotkey: &Hotkey) -> bool {
    hotkey.modifier == [SCANCODE_LWIN, SCANCODE_RWIN]
}

fn app_navigation_source(scan_code: u32, hotkey: &Hotkey) -> usize {
    usize::from(hotkey.id == SWITCH_APPS_HOTKEY_ID && scan_code != hotkey.code)
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
    if code < 0 {
        return CallNextHookEx(None, code, w_param, l_param);
    }
    let kbd_data: &KBDLLHOOKSTRUCT = &*(l_param.0 as *const _);
    if kbd_data.dwExtraInfo == WIN_MASK_INPUT_TAG {
        return CallNextHookEx(None, code, w_param, l_param);
    }
    debug!("keyboard {kbd_data:?}");
    let scan_code = kbd_data.scanCode;
    let is_key_pressed = kbd_data.flags.0 & LLKHF_UP.0 == 0;
    let mut keyboard_state = KEYBOARD_STATE.lock();
    if keyboard_state.blocked {
        drop(keyboard_state);
        return CallNextHookEx(None, code, w_param, l_param);
    }
    let previous_modifiers = keyboard_state.modifiers.pressed;
    let is_modifier = keyboard_state
        .modifiers
        .observe(kbd_data.vkCode, is_key_pressed);

    // A low-level hook runs before Windows updates the current key's async state.
    // Only sample on non-modifiers; otherwise a fresh modifier-down looks released.
    if !is_modifier {
        let physical = MODIFIER_KEYS.map(|key| GetAsyncKeyState(key as i32) < 0);
        if keyboard_state.reconcile(physical) {
            drop(keyboard_state);
            WIN_MASK_STATE.initialize();
            send_message_timeout(WINDOW, WM_USER_RESET_INPUT_SESSION, WPARAM(0), LPARAM(0));
            return CallNextHookEx(None, code, w_param, l_param);
        }
    }
    if physical_win_state(scan_code, kbd_data.flags.0).is_some() {
        WIN_MASK_STATE.observe_physical_win(
            keyboard_state
                .modifiers
                .held([SCANCODE_LWIN, SCANCODE_RWIN]),
        );
    }
    let mut send_done_hotkeys: IndexSet<u32> = IndexSet::new();
    let mut send_action_message: Option<(u32, isize, bool)> = None;
    let mut send_win_mask = false;
    let mut navigation_source = 0;

    if is_modifier && !is_key_pressed {
        let before = ModifierState {
            pressed: previous_modifiers,
        };
        for hotkey in &keyboard_state.hotkeys {
            if before.held(hotkey.modifier)
                && !keyboard_state.modifiers.held(hotkey.modifier)
                && keyboard_state.previous_keycode == hotkey.code
            {
                send_done_hotkeys.insert(hotkey.id);
            }
        }
    }
    if !is_modifier {
        if let Some((index, reverse, cancel)) =
            keyboard_state.action_for_key(scan_code, is_key_pressed, IS_FOREGROUND_IN_BLACKLIST)
        {
            let hotkey = &keyboard_state.hotkeys[index];
            navigation_source = app_navigation_source(scan_code, hotkey);
            if scan_code == hotkey.code {
                send_win_mask = should_send_win_mask(
                    &WIN_MASK_STATE,
                    hotkey,
                    GetAsyncKeyState(VK_CONTROL.0 as i32) < 0,
                );
            }
            send_action_message = Some((hotkey.id, reverse, cancel));
            if scan_code == hotkey.code || cancel {
                keyboard_state.previous_keycode = scan_code;
            }
        }
    }
    if send_done_hotkeys.contains(&SWITCH_APPS_HOTKEY_ID) {
        keyboard_state.switching_apps = false;
    }
    if let Some((id, _, cancel)) = send_action_message {
        keyboard_state.switching_apps = id == SWITCH_APPS_HOTKEY_ID && !cancel;
    }
    let generation = keyboard_state.generation;
    drop(keyboard_state);

    if send_win_mask {
        send_win_mask_input();
    }

    for id in send_done_hotkeys {
        if KEYBOARD_STATE.lock().generation != generation {
            return CallNextHookEx(None, code, w_param, l_param);
        }
        if id == SWITCH_APPS_HOTKEY_ID {
            send_message_timeout(WINDOW, WM_USER_SWITCH_APPS_DONE, WPARAM(0), LPARAM(0));
        } else if id == SWITCH_WINDOWS_HOTKEY_ID {
            send_message_timeout(WINDOW, WM_USER_SWITCH_WINDOWS_DONE, WPARAM(0), LPARAM(0));
        }
    }

    if let Some((id, reverse, is_cancel)) = send_action_message {
        // SendInput/SendMessageTimeout can dispatch a session notification reentrantly.
        // A reset invalidates all actions prepared before that notification.
        if KEYBOARD_STATE.lock().generation != generation {
            return CallNextHookEx(None, code, w_param, l_param);
        }
        if id == SWITCH_APPS_HOTKEY_ID {
            if is_cancel {
                send_message_timeout(WINDOW, WM_USER_SWITCH_APPS_CANCEL, WPARAM(0), LPARAM(0));
            } else {
                send_message_timeout(
                    WINDOW,
                    WM_USER_SWITCH_APPS,
                    WPARAM(navigation_source),
                    LPARAM(reverse),
                );
            }
            return LRESULT(1);
        } else if id == SWITCH_WINDOWS_HOTKEY_ID {
            send_message_timeout(WINDOW, WM_USER_SWITCH_WINDOWS, WPARAM(0), LPARAM(reverse));
            return LRESULT(1);
        }
    }
    CallNextHookEx(None, code, w_param, l_param)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_message_marks_arrows_but_not_primary_hotkey_or_shift_reverse() {
        let mut hotkey = hotkey_with_modifier([0x38, 0x38]);
        hotkey.id = SWITCH_APPS_HOTKEY_ID;
        assert_eq!(app_navigation_source(hotkey.code, &hotkey), 0);
        for scan in [0x48, 0x4b, 0x4d, 0x50] {
            assert_eq!(app_navigation_source(scan, &hotkey), 1);
        }
        hotkey.id = SWITCH_WINDOWS_HOTKEY_ID;
        assert_eq!(app_navigation_source(0x48, &hotkey), 0);
    }

    #[test]
    fn app_arrows_only_navigate_an_active_session_and_preserve_direction() {
        let mut hotkey = hotkey_with_modifier([0x38, 0x38]);
        hotkey.id = SWITCH_APPS_HOTKEY_ID;
        let mut state = KeyboardSession {
            hotkeys: vec![hotkey],
            ..Default::default()
        };
        state.modifiers.observe(0xA4, true);
        for (scan, reverse) in [(0x48, 1), (0x4b, 1), (0x4d, 0), (0x50, 0)] {
            assert_eq!(state.action_for_key(scan, true, false), None);
            state.switching_apps = true;
            assert_eq!(
                state.action_for_key(scan, true, false),
                Some((0, reverse, false))
            );
            state.switching_apps = false;
        }
        assert_eq!(state.action_for_key(0x01, true, false), Some((0, 0, true)));
    }

    #[test]
    fn reset_invalidates_actions_collected_before_reentrant_message_or_input() {
        let mut state = KeyboardSession::default();
        let generation = state.generation;
        state.reset(false);
        assert_ne!(state.generation, generation);
    }

    #[test]
    fn normal_hotkey_and_shift_reverse_require_live_modifiers_and_unblocked_session() {
        let mut state = KeyboardSession {
            hotkeys: vec![hotkey_with_modifier([0x5B, 0x5C])],
            ..Default::default()
        };
        assert_eq!(state.action_for_key(0x29, true, false), None);
        state.modifiers.observe(0x5B, true);
        assert_eq!(state.action_for_key(0x29, true, false), Some((0, 0, false)));
        assert_eq!(state.action_for_key(0x29, false, false), None);
        assert_eq!(state.action_for_key(0x29, true, true), None);
        state.modifiers.observe(0xA1, true);
        assert_eq!(state.action_for_key(0x29, true, false), Some((0, 1, false)));
        state.blocked = true;
        assert_eq!(state.action_for_key(0x29, true, false), None);
    }

    #[test]
    fn missing_release_cancels_old_session_without_a_new_action() {
        let mut state = KeyboardSession {
            hotkeys: vec![hotkey_with_modifier([0x5B, 0x5C])],
            switching_apps: true,
            previous_keycode: 0x29,
            ..Default::default()
        };
        state.modifiers.observe(0x5B, true);
        assert!(state.reconcile([false; 8]));
        assert!(!state.switching_apps);
        assert_eq!(state.previous_keycode, 0);
        assert_eq!(state.action_for_key(0x29, true, false), None);
        state.modifiers.observe(0x5C, true);
        let mut physical = [false; 8];
        physical[7] = true;
        assert!(!state.reconcile(physical));
        assert_eq!(state.action_for_key(0x29, true, false), Some((0, 0, false)));
    }

    #[test]
    fn session_reset_discards_selection_and_modifiers_but_preserves_hotkeys() {
        let mut state = KeyboardSession {
            hotkeys: vec![hotkey_with_modifier([0x5B, 0x5C])],
            switching_apps: true,
            previous_keycode: 0x29,
            ..Default::default()
        };
        state.modifiers.pressed = [true; 8];
        state.reset(true);
        assert_eq!(state.modifiers.pressed, [false; 8]);
        assert!(!state.switching_apps);
        assert_eq!(state.previous_keycode, 0);
        assert!(state.blocked);
        assert_eq!(state.hotkeys.len(), 1);
        state.reset(false);
        assert!(!state.blocked);
    }

    #[test]
    fn missing_release_is_repaired_before_main_key_and_hotkey_can_recover() {
        for (vk, scans) in [
            (0x5B, [0x5B, 0x5C]),
            (0xA2, [0x1D, 0x1D]),
            (0xA4, [0x38, 0x38]),
        ] {
            let mut state = ModifierState::default();
            assert!(state.observe(vk, true));
            assert!(state.held(scans));
            assert!(state.reconcile([false; 8]));
            assert!(!state.held(scans));
            state.observe(vk, true);
            let mut physical = [false; 8];
            physical[MODIFIER_KEYS
                .iter()
                .position(|key| *key as u32 == vk)
                .unwrap()] = true;
            assert!(!state.reconcile(physical));
            assert!(state.held(scans));
        }
    }

    #[test]
    fn releasing_one_side_keeps_other_modifier_and_shift_pressed() {
        for (left, right, scans) in [
            (0xA0, 0xA1, [0x2A, 0x36]),
            (0xA2, 0xA3, [0x1D, 0x1D]),
            (0xA4, 0xA5, [0x38, 0x38]),
            (0x5B, 0x5C, [0x5B, 0x5C]),
        ] {
            let mut state = ModifierState::default();
            state.observe(left, true);
            state.observe(right, true);
            state.observe(left, false);
            assert!(state.held(scans));
            state.observe(right, false);
            assert!(!state.held(scans));
        }
    }

    #[test]
    fn stale_shift_is_repaired_and_non_modifier_does_not_change_state() {
        let mut state = ModifierState::default();
        state.observe(0xA1, true);
        assert!(!state.observe(0x09, true));
        assert!(state.reconcile([false; 8]));
        assert!(!state.held([0x2A, 0x36]));
    }

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
