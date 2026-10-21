use crate::app_delay::AppDelay;
use crate::config::{edit_config_file, Config};
use crate::foreground::ForegroundWatcher;
use crate::keyboard::KeyboardListener;
use crate::painter::GdiAAPainter;
use crate::session::{
    Availability, InputSessionToken, SessionNotifications, INPUT_SESSION_EPOCH, REGISTRATION_TIMER,
};
use crate::startup::Startup;
use crate::trayicon::TrayIcon;
use crate::utils::{
    check_error, get_app_icon, get_foreground_window, get_process_start_time, get_window_user_data,
    is_iconic_window, is_running_as_admin, list_windows, set_foreground_window_while,
    set_window_user_data, AppIcon,
};

use anyhow::{anyhow, Result};
use indexmap::{IndexMap, IndexSet};
use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
    time::Instant,
};
use windows::core::{w, PCWSTR};
use windows::Win32::{
    Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::WM_MOUSELEAVE,
        Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT},
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW,
            GetWindowLongPtrW, KillTimer, LoadCursorW, PostMessageW, PostQuitMessage,
            RegisterClassW, RegisterWindowMessageW, SetTimer, SetWindowLongPtrW, TranslateMessage,
            CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWL_STYLE, HTCLIENT, IDC_ARROW, MSG,
            WINDOW_STYLE, WM_COMMAND, WM_ERASEBKGND, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCHITTEST,
            WM_RBUTTONUP, WM_TIMER, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_CAPTION, WS_EX_LAYERED,
            WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
        },
    },
};

pub const NAME: PCWSTR = w!("Window Switcher");
pub const WM_USER_TRAYICON: u32 = 6000;
pub const WM_USER_REGISTER_TRAYICON: u32 = 6001;
pub const WM_USER_SWITCH_APPS: u32 = 6010;
pub const WM_USER_SWITCH_APPS_DONE: u32 = 6011;
pub const WM_USER_SWITCH_APPS_CANCEL: u32 = 6012;
pub const WM_USER_SWITCH_WINDOWS: u32 = 6020;
pub const WM_USER_SWITCH_WINDOWS_DONE: u32 = 6021;
pub const WM_USER_RESET_INPUT_SESSION: u32 = 6030;
pub const IDM_EXIT: u32 = 1;
pub const IDM_STARTUP: u32 = 2;
pub const IDM_CONFIGURE: u32 = 3;

pub fn start(config: &Config) -> Result<()> {
    info!("start config={config:?}");
    App::start(config)
}

/// Listen to this message to recreate the tray icon since the taskbar has been recreated.
static mut WM_TASKBARCREATED: u32 = 0;

pub struct App {
    hwnd: HWND,
    is_admin: bool,
    trayicon: Option<TrayIcon>,
    startup: Startup,
    config: Config,
    switch_windows_state: SwitchWindowsState,
    fixed_order_windows: HashMap<String, Vec<isize>>,
    switch_apps_state: Option<SwitchAppsState>,
    switch_apps_presentation: Option<SwitchAppsPresentation<'static>>,
    next_presentation_id: usize,
    cached_icons: HashMap<String, CachedAppIcon>,
    painter: Rc<GdiAAPainter>,
    session_notifications: Option<SessionNotifications>,
    session_availability: Availability,
}

impl App {
    pub fn start(config: &Config) -> Result<()> {
        let hwnd = Self::create_window()?;
        let painter = Rc::new(GdiAAPainter::new(hwnd)?);

        let _foreground_watcher = ForegroundWatcher::init(&config.switch_windows_blacklist)?;
        let _keyboard_listener = KeyboardListener::init(hwnd, &config.to_hotkeys())?;

        let trayicon = match config.trayicon {
            true => Some(TrayIcon::create()),
            false => None,
        };

        let is_admin = is_running_as_admin()?;
        debug!("is_admin {is_admin}");

        let startup = Startup::init(is_admin)?;

        let mut app = App {
            hwnd,
            is_admin,
            trayicon,
            startup,
            config: config.clone(),
            switch_windows_state: SwitchWindowsState {
                cache: None,
                modifier_released: true,
            },
            fixed_order_windows: Default::default(),
            switch_apps_state: None,
            switch_apps_presentation: None,
            next_presentation_id: REGISTRATION_TIMER + 1,
            cached_icons: Default::default(),
            painter,
            session_notifications: None,
            session_availability: Availability::default(),
        };

        app.set_trayicon();

        let app_ptr = Box::into_raw(Box::new(app)) as _;
        check_error(|| set_window_user_data(hwnd, app_ptr))
            .map_err(|err| anyhow!("Failed to set window ptr, {err}"))?;

        // Registration may call into Windows: publish App first, and don't hold an
        // App reference across the call in case a notification is dispatched.
        let result = (|| {
            let notifications = SessionNotifications::new(hwnd)?;
            get_app(hwnd)?.session_notifications = Some(notifications);
            Self::eventloop()
        })();
        // Also release the registration on startup/message-loop errors or WM_QUIT
        // not sent by our Exit command. Exit itself clears the pointer first.
        if let Ok(app) = get_app(hwnd) {
            set_window_user_data(hwnd, 0);
            unsafe { drop(Box::from_raw(app)) }
        }
        result
    }

    fn eventloop() -> Result<()> {
        let mut message = MSG::default();
        loop {
            let ret = unsafe { GetMessageW(&mut message, None, 0, 0) };
            match ret.0 {
                -1 => {
                    unsafe { GetLastError() }.ok()?;
                }
                0 => break,
                _ => unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                },
            }
        }

        Ok(())
    }

    fn create_window() -> Result<HWND> {
        unsafe { WM_TASKBARCREATED = RegisterWindowMessageW(w!("TaskbarCreated")) };

        let hinstance = unsafe { GetModuleHandleW(None) }
            .map_err(|err| anyhow!("Failed to get current module handle, {err}"))?;

        let hcursor = unsafe { LoadCursorW(None, IDC_ARROW) }
            .map_err(|err| anyhow!("Failed to load arrow cursor, {err}"))?;

        let window_class = WNDCLASSW {
            hCursor: hcursor,
            hInstance: HINSTANCE(hinstance.0),
            lpszClassName: NAME,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(App::window_proc),
            ..Default::default()
        };

        let atom = check_error(|| unsafe { RegisterClassW(&window_class) })
            .map_err(|err| anyhow!("Failed to register class, {err}"))?;

        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                PCWSTR(atom as _),
                NAME,
                WINDOW_STYLE(0),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .map_err(|err| anyhow!("Failed to create windows, {err}"))?;

        // hide caption
        let mut style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
        style &= !WS_CAPTION.0;
        unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, style as _) };

        Ok(hwnd)
    }

    fn set_trayicon(&mut self) {
        if let Some(trayicon) = self.trayicon.as_mut() {
            match trayicon.register(self.hwnd) {
                Ok(()) => info!("trayicon registered"),
                Err(err) => {
                    if !trayicon.exist() {
                        error!("{err}, retrying in 3 second");
                        let hwnd = self.hwnd.0 as isize;
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(3));
                            let _ = unsafe {
                                PostMessageW(
                                    Some(HWND(hwnd as _)),
                                    WM_USER_REGISTER_TRAYICON,
                                    WPARAM(0),
                                    LPARAM(0),
                                )
                            };
                        });
                    }
                }
            }
        }
    }

    fn uses_fixed_window_order(&self, module_path: &str) -> bool {
        let module_path = module_path.split("::").next().unwrap_or(module_path);
        let exe = module_path
            .rsplit('\\')
            .next()
            .unwrap_or(module_path)
            .to_ascii_lowercase();
        self.config.switch_windows_fixed_order.contains(&exe)
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match Self::handle_message(hwnd, msg, wparam, lparam) {
            Ok(ret) => ret,
            Err(err) => {
                error!("{err}");
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }

    fn handle_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Result<LRESULT> {
        match msg {
            WM_WTSSESSION_CHANGE => {
                let app = get_app(hwnd)?;
                if let Some(blocked) = app.session_availability.change(wparam.0 as u32) {
                    crate::keyboard::reset_session(blocked);
                    app.reset_input_session();
                }
                return Ok(LRESULT(0));
            }
            WM_TIMER if wparam.0 == REGISTRATION_TIMER => {
                if let Some(mut notifications) = get_app(hwnd)?.session_notifications.take() {
                    let result = notifications.retry();
                    get_app(hwnd)?.session_notifications = Some(notifications);
                    result?;
                }
                return Ok(LRESULT(0));
            }
            WM_TIMER => {
                let current = get_app(hwnd)?
                    .switch_apps_presentation
                    .as_mut()
                    .is_some_and(|session| session.accept_timer(wparam.0));
                if current {
                    let _ = unsafe { KillTimer(Some(hwnd), wparam.0) };
                    // A coarse Windows tick may arrive before our Instant deadline.
                    // Re-arm even during construction, rather than losing the reveal.
                    let remaining = get_app(hwnd)?
                        .switch_apps_presentation
                        .as_ref()
                        .map_or(0, |session| session.delay.remaining_ms(Instant::now()));
                    if remaining != 0
                        && unsafe { SetTimer(Some(hwnd), wparam.0, remaining, None) } == 0
                    {
                        warn!(
                            "Failed to re-arm application UI delay: {:?}; showing immediately",
                            unsafe { GetLastError() }
                        );
                        if let Some(session) = get_app(hwnd)?.switch_apps_presentation.as_mut() {
                            session.delay.timer_failed();
                        }
                    }
                    Self::present_switch_apps(hwnd, false)?;
                }
                return Ok(LRESULT(0));
            }
            WM_USER_RESET_INPUT_SESSION => {
                get_app(hwnd)?.reset_input_session();
                return Ok(LRESULT(0));
            }
            WM_USER_TRAYICON => {
                let app = get_app(hwnd)?;
                if let Some(trayicon) = app.trayicon.as_mut() {
                    let keycode = lparam.0 as u32;
                    if keycode == WM_LBUTTONUP || keycode == WM_RBUTTONUP {
                        trayicon.show(app.startup.is_enable)?;
                    }
                }
                return Ok(LRESULT(0));
            }
            WM_USER_SWITCH_APPS => {
                debug!("message WM_USER_SWITCH_APPS");
                let started = Instant::now();
                let reverse = lparam.0 == 1;
                if get_app(hwnd)?
                    .switch_apps_presentation
                    .as_ref()
                    .is_some_and(|session| session.delay.released())
                {
                    get_app(hwnd)?.cancel_switch_app();
                }
                if let Some(session) = get_app(hwnd)?.switch_apps_presentation.as_mut() {
                    session.delay.navigate(reverse);
                    Self::present_switch_apps(hwnd, true)?;
                } else {
                    let (id, delay, token) = {
                        let app = get_app(hwnd)?;
                        let id = app.next_presentation_id;
                        app.next_presentation_id = id
                            .checked_add(1)
                            .ok_or_else(|| anyhow!("Application UI timer IDs exhausted"))?;
                        INPUT_SESSION_EPOCH.invalidate();
                        let token = INPUT_SESSION_EPOCH.capture();
                        // An arrow remains immediate even if the hook's active
                        // cycle outlived an empty/canceled UI presentation.
                        let delay = if wparam.0 != 0 {
                            0
                        } else {
                            app.config.switch_apps_ui_delay_ms
                        };
                        app.switch_apps_presentation = Some(SwitchAppsPresentation {
                            delay: AppDelay::new(id, started, delay),
                            token,
                            mouse_ready: false,
                            icon_sources: None,
                            loading_icons: false,
                        });
                        let remaining = app
                            .switch_apps_presentation
                            .as_ref()
                            .unwrap()
                            .delay
                            .remaining_ms(Instant::now());
                        (id, remaining, token)
                    };
                    if delay != 0 && unsafe { SetTimer(Some(hwnd), id, delay, None) } == 0 {
                        warn!(
                            "Failed to schedule application UI delay: {:?}; showing immediately",
                            unsafe { GetLastError() }
                        );
                        if let Some(session) = get_app(hwnd)?.switch_apps_presentation.as_mut() {
                            session.delay.timer_failed();
                        }
                    }
                    let result = Self::switch_apps(hwnd, reverse, token);
                    if token.is_current() {
                        if result.is_err() || get_app(hwnd)?.switch_apps_state.is_none() {
                            get_app(hwnd)?.cancel_switch_app();
                        } else {
                            Self::present_switch_apps(hwnd, false)?;
                        }
                    }
                    result?;
                }
            }
            WM_USER_SWITCH_APPS_DONE => {
                debug!("message WM_USER_SWITCH_APPS_DONE");
                let app = get_app(hwnd)?;
                app.do_switch_app();
            }
            WM_USER_SWITCH_APPS_CANCEL => {
                debug!("message WM_USER_SWITCH_APPS_CANCEL");
                let app = get_app(hwnd)?;
                app.cancel_switch_app();
            }
            WM_USER_SWITCH_WINDOWS => {
                debug!("message WM_USER_SWITCH_WINDOWS");
                let token = INPUT_SESSION_EPOCH.capture();
                let reverse = lparam.0 == 1;
                let (target, config, is_admin) = {
                    let app = get_app(hwnd)?;
                    let target = app
                        .switch_apps_state
                        .as_ref()
                        .and_then(|state| state.apps.get(state.keyboard_index()).map(|(_, id)| *id))
                        .unwrap_or_else(get_foreground_window);
                    (target, app.config.clone(), app.is_admin)
                };
                // Window enumeration can pump COM/Win32 messages. No App borrow
                // or cache reference may span it, and its result is session-bound.
                let windows = list_windows(
                    config.switch_windows_ignore_minimal,
                    config.switch_windows_only_current_desktop(),
                    is_admin,
                )?;
                if !token.is_current() {
                    return Ok(LRESULT(0));
                }
                let target =
                    get_app(hwnd)?.select_switch_window(target, reverse, &windows, token)?;
                if let Some(target) = target {
                    set_foreground_window_while(target, || token.is_current());
                }
                token
                    .run_if_current(|| get_app(hwnd).map(|app| app.cancel_switch_app()))
                    .transpose()?;
            }
            WM_USER_SWITCH_WINDOWS_DONE => {
                debug!("message WM_USER_SWITCH_WINDOWS_DONE");
                let app = get_app(hwnd)?;
                app.switch_windows_state.modifier_released = true;
            }
            WM_NCHITTEST => {
                return Ok(LRESULT(HTCLIENT as _));
            }
            WM_LBUTTONUP => {
                let app = get_app(hwnd)?;
                app.click(lparam);
            }
            WM_MOUSEMOVE => {
                let app = get_app(hwnd)?;
                app.hover(lparam);
            }
            WM_MOUSELEAVE => {
                let app = get_app(hwnd)?;
                app.clear_hover();
            }
            WM_COMMAND => {
                let value = wparam.0 as u32;
                let kind = ((value >> 16) & 0xffff) as u16;
                let id = value & 0xffff;
                if kind == 0 {
                    match id {
                        IDM_EXIT => {
                            if let Ok(app) = get_app(hwnd) {
                                set_window_user_data(hwnd, 0);
                                unsafe { drop(Box::from_raw(app)) }
                            }
                            unsafe { PostQuitMessage(0) }
                        }
                        IDM_STARTUP => {
                            let app = get_app(hwnd)?;
                            app.startup.toggle()?;
                        }
                        IDM_CONFIGURE => {
                            if let Err(err) = edit_config_file() {
                                alert!("{err}");
                            }
                        }
                        _ => {}
                    }
                }
            }
            WM_ERASEBKGND => {
                return Ok(LRESULT(0));
            }
            _ if msg == WM_USER_REGISTER_TRAYICON || unsafe { msg == WM_TASKBARCREATED } => {
                let app = get_app(hwnd)?;
                if msg == unsafe { WM_TASKBARCREATED } {
                    app.cancel_switch_app();
                    app.cached_icons.clear();
                }
                app.set_trayicon();
            }
            _ => {}
        }
        Ok(unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) })
    }

    fn select_switch_window(
        &mut self,
        hwnd: HWND,
        reverse: bool,
        windows: &IndexMap<String, Vec<(HWND, String)>>,
        token: InputSessionToken<'_>,
    ) -> Result<Option<HWND>> {
        if !token.is_current() {
            return Ok(None);
        }
        debug!(
            "switch windows: hwnd:{hwnd:?} reverse:{reverse} state:{:?}",
            self.switch_windows_state
        );
        let module_path = match windows
            .iter()
            .find(|(_, v)| v.iter().any(|(id, _)| *id == hwnd))
            .map(|(k, _)| k.clone())
        {
            Some(v) => v,
            None => return Ok(None),
        };
        match windows.get(&module_path) {
            None => Ok(None),
            Some(windows) => {
                let fixed_order = self.uses_fixed_window_order(&module_path);
                let mut window_ids: Vec<isize> =
                    windows.iter().map(|(id, _)| id.0 as isize).collect();

                if fixed_order {
                    let stable_order = self
                        .fixed_order_windows
                        .entry(module_path.clone())
                        .or_default();
                    merge_fixed_order(stable_order, &window_ids);
                    if let Some(rotated) = rotate_fixed_order(stable_order, hwnd.0 as isize) {
                        window_ids = rotated;
                    }
                }

                let windows_len = window_ids.len();
                if windows_len <= 1 {
                    return Ok(None);
                }
                let current_id = if fixed_order {
                    HWND(window_ids[0] as _)
                } else {
                    windows[0].0
                };
                let mut index = if fixed_order && reverse {
                    windows_len - 1
                } else {
                    1
                };
                let mut state_id = current_id;
                let mut state_windows = vec![];
                if fixed_order {
                    if let Some((cache_module_path, cache_id, cache_index, cache_windows)) =
                        self.switch_windows_state.cache.as_ref()
                    {
                        if cache_module_path == &module_path
                            && !self.switch_windows_state.modifier_released
                        {
                            state_id = *cache_id;
                            state_windows =
                                reconstruct_fixed_state_windows(cache_windows, &window_ids);
                            index = match normalized_fixed_index(
                                hwnd.0 as isize,
                                *cache_index,
                                cache_windows,
                                &state_windows,
                                reverse,
                            ) {
                                Some(index) => index,
                                None => return Ok(None),
                            };
                        }
                    }
                } else if windows_len > 2 {
                    if let Some((cache_module_path, cache_id, cache_index, cache_windows)) =
                        self.switch_windows_state.cache.as_ref()
                    {
                        if cache_module_path == &module_path {
                            if self.switch_windows_state.modifier_released {
                                if *cache_id != current_id {
                                    if let Some((i, _)) =
                                        windows.iter().enumerate().find(|(_, (v, _))| v == cache_id)
                                    {
                                        index = i;
                                    }
                                }
                            } else {
                                state_id = *cache_id;
                                let mut windows_set: IndexSet<isize> =
                                    windows.iter().map(|(v, _)| v.0 as _).collect();
                                for id in cache_windows {
                                    if windows_set.contains(id) {
                                        state_windows.push(*id);
                                        windows_set.swap_remove(id);
                                    }
                                }
                                state_windows.extend(windows_set);
                                index = if reverse {
                                    if *cache_index == 0 || *cache_index >= windows_len {
                                        windows_len - 1
                                    } else {
                                        cache_index - 1
                                    }
                                } else if *cache_index >= windows_len - 1 {
                                    0
                                } else {
                                    cache_index + 1
                                };
                            }
                        }
                    }
                }
                if state_windows.is_empty() {
                    state_windows = window_ids.clone();
                }
                let hwnd = HWND(state_windows[index] as _);
                Ok(token.run_if_current(|| {
                    self.switch_windows_state = SwitchWindowsState {
                        cache: Some((module_path.clone(), state_id, index, state_windows)),
                        modifier_released: false,
                    };
                    hwnd
                }))
            }
        }
    }

    fn switch_apps(hwnd: HWND, reverse: bool, token: InputSessionToken<'_>) -> Result<()> {
        if !token.is_current() {
            return Ok(());
        }
        let (config, is_admin) = {
            let app = get_app(hwnd)?;
            debug!(
                "switch apps: reverse:{reverse}, state:{:?}",
                app.switch_apps_state
            );
            (app.config.clone(), app.is_admin)
        };
        let windows = list_windows(
            config.switch_apps_ignore_minimal,
            config.switch_apps_only_current_desktop(),
            is_admin,
        )?;
        if !token.is_current() {
            return Ok(());
        }
        let targets: Vec<_> = windows
            .values()
            .map(|hwnds| {
                if is_iconic_window(hwnds[0].0) {
                    hwnds[hwnds.len() - 1].0
                } else {
                    hwnds[0].0
                }
            })
            .collect();
        let Some(mut state) = SwitchAppsState::from_targets(targets.clone(), reverse) else {
            return Ok(());
        };
        if !token.is_current() {
            return Ok(());
        }
        {
            let app = get_app(hwnd)?;
            if let Some(session) = app.switch_apps_presentation.as_mut() {
                state.apply_navigation(&mut session.delay, (0, 0));
                session.icon_sources = Some(
                    windows
                        .into_iter()
                        .zip(targets)
                        .map(|((module_path, hwnds), target)| AppIconSource {
                            module_path,
                            hwnds: hwnds.into_iter().map(|(hwnd, _)| hwnd).collect(),
                            target,
                        })
                        .collect(),
                );
            }
            app.switch_apps_state = Some(state);
        }
        Ok(())
    }

    fn present_switch_apps(hwnd: HWND, repaint: bool) -> Result<()> {
        let (token, work) = {
            let app = get_app(hwnd)?;
            let Some(session) = app.switch_apps_presentation.as_mut() else {
                return Ok(());
            };
            if !session.token.is_current() {
                return Ok(());
            }
            let Some(state) = app.switch_apps_state.as_mut() else {
                return Ok(());
            };
            // Nested navigation must update the live lightweight selection even
            // while WM_GETICON is pumping messages, but never start another load.
            state.apply_navigation(&mut session.delay, (0, 0));
            if session.delay.released() {
                app.do_switch_app();
                return Ok(());
            }
            if session.loading_icons {
                return Ok(());
            }
            if session.icon_sources.is_some() {
                let Some(sources) = session.begin_icon_load(Instant::now()) else {
                    return Ok(());
                };
                (session.token, Some((session.delay.id, sources)))
            } else if repaint && session.delay.accepts_mouse() {
                (session.token, None)
            } else {
                return Ok(());
            }
        };
        if let Some((id, sources)) = work {
            let _ = unsafe { KillTimer(Some(hwnd), id) };
            // Only the display path snapshots the cache or queries identities.
            // All Win32 icon work uses owned data, without an App borrow.
            Self::load_switch_app_icons(hwnd, sources, token)?;
            if !token.is_current() {
                return Ok(());
            }
            if let Some(session) = get_app(hwnd)?.switch_apps_presentation.as_mut() {
                session.loading_icons = false;
            }
        }
        let mut pointer = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pointer);
        }
        if !token.is_current() {
            return Ok(());
        }
        let (state, initialize_mouse) = {
            let app = get_app(hwnd)?;
            let Some(session) = app.switch_apps_presentation.as_mut() else {
                return Ok(());
            };
            if !session.token.is_current() || session.loading_icons {
                return Ok(());
            }
            let Some(state) = app.switch_apps_state.as_mut() else {
                return Ok(());
            };
            // Never restore a pre-load snapshot: nested input may have navigated.
            state.apply_navigation(&mut session.delay, (pointer.x, pointer.y));
            state.reset_pointer_position((pointer.x, pointer.y));
            (state.clone(), !session.mouse_ready)
        };
        // The owned Rc keeps painter storage independent of a reentrant App borrow.
        let painter = Rc::clone(&get_app(hwnd)?.painter);
        let shown = token.is_current() && painter.paint(&state, token);
        if initialize_mouse && shown && token.is_current() {
            let mut pointer = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut pointer);
            }
            if token.is_current() {
                let app = get_app(hwnd)?;
                if let Some(state) = app.switch_apps_state.as_mut() {
                    state.reset_pointer_position((pointer.x, pointer.y));
                }
                if let Some(session) = app.switch_apps_presentation.as_mut() {
                    session.mouse_ready = true;
                }
            }
        }
        Ok(())
    }

    fn load_switch_app_icons(
        hwnd: HWND,
        sources: Vec<AppIconSource>,
        token: InputSessionToken<'_>,
    ) -> Result<()> {
        if !token.is_current() {
            return Ok(());
        }
        let (config, mut cached_icons) = {
            let app = get_app(hwnd)?;
            (app.config.clone(), app.cached_icons.clone())
        };
        build_icons_while_current(
            sources.into_iter().enumerate(),
            token,
            |(index, source)| {
                let module_path = &source.module_path;
                let identities =
                    icon_identities_while_current(&source.hwnds, token, get_process_start_time);
                if !token.is_current() {
                    return Ok(None);
                }
                let icon = cached_icons
                    .get(module_path)
                    .and_then(|cached| cached.reuse(&identities));
                let icon = icon.unwrap_or_else(|| {
                    let icon = Rc::new(get_app_icon(
                        &config.switch_apps_override_icons,
                        module_path,
                        source.target,
                    ));
                    cached_icons.insert(
                        module_path.clone(),
                        CachedAppIcon {
                            icon: Rc::clone(&icon),
                            identities,
                        },
                    );
                    icon
                });
                Ok(Some((index, icon)))
            },
            |result| {
                let Some((index, icon)) = result else {
                    return Ok(());
                };
                if let Some(state) = get_app(hwnd)?.switch_apps_state.as_mut() {
                    state.apps[index].0 = Some(icon);
                }
                Ok(())
            },
        )?;
        token
            .run_if_current(|| -> Result<()> {
                let app = get_app(hwnd)?;
                app.cached_icons = cached_icons;
                debug!("switch apps, new state:{:?}", app.switch_apps_state);
                Ok(())
            })
            .transpose()?;
        Ok(())
    }

    fn accepts_mouse(&self) -> bool {
        self.painter.is_visible()
            && self
                .switch_apps_presentation
                .as_ref()
                .is_some_and(|session| session.accepts_mouse())
    }

    fn click(&mut self, lparam: LPARAM) {
        if !self.accepts_mouse() {
            return;
        }
        let pointer_position = client_pointer_position(lparam);
        let clicked_index = self.switch_apps_state.as_ref().and_then(|state| {
            self.painter
                .find_app_index_at_client(state, pointer_position)
        });
        if let Some(index) = clicked_index {
            self.do_switch_app_at(index);
        }
    }

    fn hover(&mut self, lparam: LPARAM) {
        if !self.accepts_mouse() {
            return;
        }
        let Some(state) = self.switch_apps_state.as_mut() else {
            return;
        };
        let client_position = client_pointer_position(lparam);
        let mut pointer_position = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pointer_position);
            let mut tracking = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tracking);
        }
        let hovered_index = self
            .painter
            .find_app_index_at_client(state, client_position);
        if state.update_hover((pointer_position.x, pointer_position.y), hovered_index) {
            let state = state.clone();
            let painter = Rc::clone(&self.painter);
            painter.paint(&state, INPUT_SESSION_EPOCH.capture());
        }
    }

    fn clear_hover(&mut self) {
        if !self.accepts_mouse() {
            return;
        }
        if let Some(state) = self.switch_apps_state.as_mut() {
            if state.clear_hover() {
                let state = state.clone();
                let painter = Rc::clone(&self.painter);
                painter.paint(&state, INPUT_SESSION_EPOCH.capture());
            }
        }
    }

    fn do_switch_app(&mut self) {
        if let Some(session) = self.switch_apps_presentation.as_mut() {
            if let Some(state) = self.switch_apps_state.as_mut() {
                state.apply_navigation(&mut session.delay, (0, 0));
            }
            session.delay.release();
            let _ = unsafe { KillTimer(Some(self.hwnd), session.delay.id) };
        }
        let index = self
            .switch_apps_state
            .as_ref()
            .map(SwitchAppsState::keyboard_index);
        if let Some(index) = index {
            self.do_switch_app_at(index);
        }
    }

    fn do_switch_app_at(&mut self, index: usize) {
        let target = self
            .switch_apps_state
            .as_ref()
            .and_then(|state| state.apps.get(index).map(|(_, id)| *id));
        let token = self.cancel_switch_app();
        if let Some(id) = target {
            set_foreground_window_while(id, || token.is_current());
        }
    }

    fn cancel_switch_app(&mut self) -> InputSessionToken<'static> {
        INPUT_SESSION_EPOCH.invalidate();
        let token = INPUT_SESSION_EPOCH.capture();
        if let Some(session) = self.switch_apps_presentation.take() {
            let _ = unsafe { KillTimer(Some(self.hwnd), session.delay.id) };
        }
        self.switch_apps_state = None;
        let painter = Rc::clone(&self.painter);
        painter.unpaint();
        token
    }

    fn reset_input_session(&mut self) {
        INPUT_SESSION_EPOCH.invalidate();
        self.switch_windows_state.reset();
        self.cancel_switch_app();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.cancel_switch_app();
        self.cached_icons.clear();
    }
}

fn get_app(hwnd: HWND) -> Result<&'static mut App> {
    unsafe {
        let ptr = check_error(|| get_window_user_data(hwnd))
            .map_err(|err| anyhow!("Failed to get window ptr, {err}"))?;
        if ptr == 0 {
            return Err(anyhow!("Application state is not available"));
        }
        let tx: &mut App = &mut *(ptr as *mut App);
        Ok(tx)
    }
}

fn client_pointer_position(lparam: LPARAM) -> (i32, i32) {
    let packed = lparam.0 as u32;
    (
        packed as u16 as i16 as i32,
        (packed >> 16) as u16 as i16 as i32,
    )
}

fn build_icons_while_current<T, U>(
    sources: impl IntoIterator<Item = T>,
    token: InputSessionToken<'_>,
    mut load: impl FnMut(T) -> Result<U>,
    mut publish: impl FnMut(U) -> Result<()>,
) -> Result<()> {
    for source in sources {
        if !token.is_current() {
            break;
        }
        let icon = load(source)?;
        if !token.is_current() {
            break;
        }
        publish(icon)?;
    }
    Ok(())
}

fn icon_identities_while_current(
    hwnds: &[HWND],
    token: InputSessionToken<'_>,
    mut query: impl FnMut(HWND) -> Option<u64>,
) -> Option<Vec<u64>> {
    let mut identities = Vec::with_capacity(hwnds.len());
    for hwnd in hwnds {
        if !token.is_current() {
            return None;
        }
        identities.push(query(*hwnd)?);
    }
    identities.sort_unstable();
    identities.dedup();
    token.is_current().then_some(identities)
}

#[derive(Debug)]
struct SwitchWindowsState {
    cache: Option<(String, HWND, usize, Vec<isize>)>,
    modifier_released: bool,
}

impl SwitchWindowsState {
    fn reset(&mut self) {
        self.cache = None;
        self.modifier_released = true;
    }
}

struct SwitchAppsPresentation<'a> {
    delay: AppDelay,
    token: InputSessionToken<'a>,
    mouse_ready: bool,
    icon_sources: Option<Vec<AppIconSource>>,
    loading_icons: bool,
}

impl SwitchAppsPresentation<'_> {
    fn begin_icon_load(&mut self, now: Instant) -> Option<Vec<AppIconSource>> {
        if !self.token.is_current()
            || self.loading_icons
            || self.icon_sources.is_none()
            || !self.delay.show(now, true)
        {
            return None;
        }
        self.loading_icons = true;
        self.icon_sources.take()
    }

    fn accept_timer(&mut self, id: usize) -> bool {
        self.token.is_current() && self.delay.timer(id)
    }

    fn accepts_mouse(&self) -> bool {
        self.token.is_current()
            && !self.loading_icons
            && self.mouse_ready
            && self.delay.accepts_mouse()
    }
}

#[derive(Clone)]
struct AppIconSource {
    module_path: String,
    hwnds: Vec<HWND>,
    target: HWND,
}

#[derive(Debug, Clone)]
pub struct SwitchAppsState {
    pub apps: Vec<(Option<Rc<AppIcon>>, HWND)>,
    interaction: SwitchAppsInteractionState,
}

impl SwitchAppsState {
    fn from_targets(targets: Vec<HWND>, reverse: bool) -> Option<Self> {
        if targets.is_empty() {
            return None;
        }
        let index = if targets.len() == 1 {
            0
        } else if reverse {
            targets.len() - 1
        } else {
            1
        };
        Some(Self {
            apps: targets.into_iter().map(|target| (None, target)).collect(),
            interaction: SwitchAppsInteractionState::new(index, (0, 0)),
        })
    }

    fn apply_navigation(&mut self, delay: &mut AppDelay, pointer: (i32, i32)) {
        for reverse in delay.take_navigation() {
            self.navigate(reverse, pointer);
        }
    }
    pub fn displayed_index(&self) -> usize {
        self.interaction.displayed_index()
    }

    fn keyboard_index(&self) -> usize {
        self.interaction.keyboard_index()
    }

    fn navigate(&mut self, reverse: bool, pointer_position: (i32, i32)) {
        self.interaction
            .navigate(self.apps.len(), reverse, pointer_position);
    }

    fn update_hover(&mut self, pointer_position: (i32, i32), hovered_index: Option<usize>) -> bool {
        self.interaction
            .update_hover(pointer_position, hovered_index)
    }

    fn clear_hover(&mut self) -> bool {
        self.interaction.clear_hover()
    }

    fn reset_pointer_position(&mut self, pointer_position: (i32, i32)) {
        self.interaction.reset_pointer_position(pointer_position);
    }
}

#[derive(Debug, Clone)]
struct SwitchAppsInteractionState {
    keyboard_index: usize,
    hovered_index: Option<usize>,
    pointer_position: (i32, i32),
}

impl SwitchAppsInteractionState {
    fn new(keyboard_index: usize, pointer_position: (i32, i32)) -> Self {
        Self {
            keyboard_index,
            hovered_index: None,
            pointer_position,
        }
    }

    fn keyboard_index(&self) -> usize {
        self.keyboard_index
    }

    fn hovered_index(&self) -> Option<usize> {
        self.hovered_index
    }

    fn displayed_index(&self) -> usize {
        self.hovered_index.unwrap_or(self.keyboard_index)
    }

    fn update_hover(&mut self, pointer_position: (i32, i32), hovered_index: Option<usize>) -> bool {
        if self.pointer_position == pointer_position {
            return false;
        }
        self.pointer_position = pointer_position;
        if self.hovered_index == hovered_index {
            return false;
        }
        if let Some(index) = hovered_index {
            self.keyboard_index = index;
        }
        self.hovered_index = hovered_index;
        true
    }

    fn clear_hover(&mut self) -> bool {
        if self.hovered_index().is_none() {
            return false;
        }
        self.hovered_index = None;
        true
    }

    fn reset_pointer_position(&mut self, pointer_position: (i32, i32)) {
        self.pointer_position = pointer_position;
        self.hovered_index = None;
    }

    fn navigate(&mut self, app_count: usize, reverse: bool, pointer_position: (i32, i32)) {
        self.pointer_position = pointer_position;
        if app_count == 0 {
            self.hovered_index = None;
            return;
        }
        self.keyboard_index = if reverse {
            if self.keyboard_index == 0 {
                app_count - 1
            } else {
                self.keyboard_index - 1
            }
        } else if self.keyboard_index >= app_count - 1 {
            0
        } else {
            self.keyboard_index + 1
        };
        self.hovered_index = None;
    }
}

#[derive(Clone)]
struct CachedAppIcon {
    icon: Rc<AppIcon>,
    identities: Option<Vec<u64>>,
}

impl CachedAppIcon {
    fn reuse(&self, identities: &Option<Vec<u64>>) -> Option<Rc<AppIcon>> {
        (identities.is_some() && self.identities.as_ref() == identities.as_ref())
            .then(|| Rc::clone(&self.icon))
    }
}

fn merge_fixed_order(order: &mut Vec<isize>, visible: &[isize]) {
    let visible_set: HashSet<isize> = visible.iter().copied().collect();
    order.retain(|id| visible_set.contains(id));

    for id in visible {
        if !order.contains(id) {
            order.push(*id);
        }
    }
}

fn rotate_fixed_order(order: &[isize], current: isize) -> Option<Vec<isize>> {
    let start = order.iter().position(|id| *id == current)?;
    let mut rotated = Vec::with_capacity(order.len());
    rotated.extend_from_slice(&order[start..]);
    rotated.extend_from_slice(&order[..start]);
    Some(rotated)
}

fn reconstruct_fixed_state_windows(cache_windows: &[isize], window_ids: &[isize]) -> Vec<isize> {
    let current_ids: HashSet<isize> = window_ids.iter().copied().collect();
    let mut state_windows = cache_windows
        .iter()
        .copied()
        .filter(|id| current_ids.contains(id))
        .collect::<Vec<_>>();

    for id in window_ids {
        if !state_windows.contains(id) {
            state_windows.push(*id);
        }
    }

    state_windows
}

fn next_fixed_index(index: usize, len: usize, reverse: bool) -> usize {
    if reverse {
        if index == 0 {
            len - 1
        } else {
            index - 1
        }
    } else if index >= len - 1 {
        0
    } else {
        index + 1
    }
}

fn normalized_fixed_index(
    current: isize,
    cache_index: usize,
    cache_windows: &[isize],
    state_windows: &[isize],
    reverse: bool,
) -> Option<usize> {
    let len = state_windows.len();
    if len < 2 {
        return None;
    }
    let anchor = state_windows
        .iter()
        .position(|id| *id == current)
        .or_else(|| {
            cache_windows
                .get(cache_index)
                .and_then(|id| state_windows.iter().position(|current_id| current_id == id))
        })
        .unwrap_or(cache_index % len);
    Some(next_fixed_index(anchor, len, reverse))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lightweight_round_does_not_prepare_icons_before_the_presentation_gate() {
        // Guard the Win32 wiring as well as the model tests below: enumeration
        // must not start display-only work during a hidden quick-switch round.
        let source = include_str!("app.rs");
        let switch = source
            .split("    fn switch_apps(hwnd:")
            .nth(1)
            .unwrap()
            .split("    fn present_switch_apps(")
            .next()
            .unwrap();
        for eager_work in [
            "build_icons_while_current(",
            "get_process_start_time(",
            "get_app_icon(",
            "cached_icons.clone()",
        ] {
            assert!(
                !switch.contains(eager_work),
                "hidden round eagerly calls {eager_work}"
            );
        }
    }

    fn icon_session<'a>(
        epoch: &'a crate::session::InputSessionEpoch,
        now: Instant,
        delay: u32,
    ) -> SwitchAppsPresentation<'a> {
        SwitchAppsPresentation {
            delay: AppDelay::new(10, now, delay),
            token: epoch.capture(),
            mouse_ready: false,
            icon_sources: Some(vec![AppIconSource {
                module_path: "test.exe".into(),
                hwnds: vec![HWND(1 as _)],
                target: HWND(1 as _),
            }]),
            loading_icons: false,
        }
    }

    #[test]
    fn hidden_and_quick_release_do_not_start_any_icon_work() {
        let epoch = crate::session::InputSessionEpoch::new();
        let now = Instant::now();
        let mut session = icon_session(&epoch, now, 150);
        let mut loads = 0;
        for elapsed in [0, 50, 149] {
            if session
                .begin_icon_load(now + std::time::Duration::from_millis(elapsed))
                .is_some()
            {
                loads += 1;
            }
        }
        session.delay.release();
        if session
            .begin_icon_load(now + std::time::Duration::from_millis(200))
            .is_some()
        {
            loads += 1;
        }
        assert_eq!(loads, 0);
        assert!(session.icon_sources.is_some());
        assert!(!session.accepts_mouse());
    }

    #[test]
    fn deadline_second_operation_zero_and_timer_failure_start_icons_exactly_once() {
        let epoch = crate::session::InputSessionEpoch::new();
        let now = Instant::now();
        for trigger in 0..5 {
            let mut session = icon_session(&epoch, now, if trigger == 3 { 0 } else { 150 });
            let at = if trigger == 0 {
                now + std::time::Duration::from_millis(150)
            } else {
                now
            };
            if trigger == 1 {
                session.delay.navigate(false);
            }
            if trigger == 2 {
                session.delay.navigate(true);
            }
            if trigger == 4 {
                session.delay.timer_failed();
            }
            assert_eq!(session.begin_icon_load(at).unwrap().len(), 1);
            assert!(session.loading_icons);
            assert!(!session.accepts_mouse());
            assert!(session.begin_icon_load(at).is_none());
            session.loading_icons = false;
            assert!(session.begin_icon_load(at).is_none());
        }
    }

    #[test]
    fn navigation_during_loading_is_applied_once_without_restarting_or_replacing_selection() {
        let epoch = crate::session::InputSessionEpoch::new();
        let now = Instant::now();
        let session = std::cell::RefCell::new(icon_session(&epoch, now, 0));
        let state = std::cell::RefCell::new(
            SwitchAppsState::from_targets(vec![HWND(1 as _), HWND(2 as _), HWND(3 as _)], false)
                .unwrap(),
        );
        assert!(session.borrow_mut().begin_icon_load(now).is_some());
        let mut loads = 0;
        build_icons_while_current(
            0..2,
            epoch.capture(),
            |_| {
                loads += 1;
                let mut session = session.borrow_mut();
                session.delay.navigate(false);
                state
                    .borrow_mut()
                    .apply_navigation(&mut session.delay, (0, 0));
                assert!(session.begin_icon_load(now).is_none());
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(loads, 2);
        assert_eq!(state.borrow().keyboard_index(), 0); // not the initial selection (1)
        state
            .borrow_mut()
            .apply_navigation(&mut session.borrow_mut().delay, (0, 0));
        assert_eq!(state.borrow().keyboard_index(), 0);
    }

    #[test]
    fn invalidated_load_cannot_publish_to_a_new_round_or_load_remaining_sources() {
        let epoch = crate::session::InputSessionEpoch::new();
        let now = Instant::now();
        for reason in ["release", "escape", "lock", "new round"] {
            let mut old = icon_session(&epoch, now, 0);
            let mut not_started = icon_session(&epoch, now, 0);
            let sources = old.begin_icon_load(now).unwrap();
            let mut loads = 0;
            let mut published = 0;
            build_icons_while_current(
                sources.into_iter().cycle().take(3),
                old.token,
                |_| {
                    loads += 1;
                    epoch.invalidate();
                    Ok(())
                },
                |_| {
                    published += 1;
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!((loads, published), (1, 0), "{reason}");
            assert!(old.begin_icon_load(now).is_none());
            assert!(not_started.begin_icon_load(now).is_none());
            assert!(icon_session(&epoch, now, 0).begin_icon_load(now).is_some());
        }
    }

    #[test]
    fn identity_queries_stop_immediately_if_release_or_reset_reenters() {
        let epoch = crate::session::InputSessionEpoch::new();
        for hwnds in [vec![HWND(1 as _)], vec![HWND(1 as _), HWND(2 as _)]] {
            let mut queries = 0;
            let identities = icon_identities_while_current(&hwnds, epoch.capture(), |_| {
                queries += 1;
                epoch.invalidate();
                Some(42)
            });
            assert_eq!(queries, 1);
            assert!(identities.is_none());
        }
    }

    #[test]
    fn empty_enumeration_and_release_before_targets_cannot_start_display_preparation() {
        let epoch = crate::session::InputSessionEpoch::new();
        let now = Instant::now();
        let mut session = icon_session(&epoch, now, 0);
        let sources = session.icon_sources.take();
        assert!(session.begin_icon_load(now).is_none());
        assert!(!session.delay.accepts_mouse());
        assert!(SwitchAppsState::from_targets(vec![], false).is_none());
        // A timer/navigation can reenter enumeration, but must not consume the
        // display gate until lightweight targets have actually been published.
        session.delay.navigate(false);
        session.icon_sources = sources;
        assert!(session.begin_icon_load(now).is_some());
        let mut released = icon_session(&epoch, now, 0);
        let sources = released.icon_sources.take();
        released.delay.release();
        released.icon_sources = sources;
        assert!(released.begin_icon_load(now).is_none());
    }

    #[test]
    fn process_identities_remain_sorted_unique_and_unknown_identities_are_not_reused() {
        let epoch = crate::session::InputSessionEpoch::new();
        let hwnds = [HWND(3 as _), HWND(1 as _), HWND(3 as _)];
        assert_eq!(
            icon_identities_while_current(&hwnds, epoch.capture(), |hwnd| Some(hwnd.0 as u64)),
            Some(vec![1, 3])
        );
        assert_eq!(
            icon_identities_while_current(&hwnds, epoch.capture(), |_| None),
            None
        );
    }

    #[test]
    fn eligible_round_reuses_cached_icon_only_for_verified_same_processes() {
        // Empty/nonexistent source produces a harmless AppIcon fallback; these
        // tests compare Rc ownership, not platform-specific icon pixels.
        let icon = Rc::new(get_app_icon(&IndexMap::new(), "", HWND::default()));
        let cached = CachedAppIcon {
            icon: Rc::clone(&icon),
            identities: Some(vec![1, 3]),
        };
        assert!(Rc::ptr_eq(&cached.reuse(&Some(vec![1, 3])).unwrap(), &icon));
        assert!(cached.reuse(&Some(vec![1, 4])).is_none());
        assert!(cached.reuse(&None).is_none());
        let unknown = CachedAppIcon {
            icon,
            identities: None,
        };
        assert!(unknown.reuse(&None).is_none());
        assert!(unknown.reuse(&Some(vec![1, 3])).is_none());
    }

    #[test]
    fn review_release_during_first_icon_commits_lightweight_selection_and_stops_remaining_icons() {
        use std::cell::RefCell;
        let epoch = crate::session::InputSessionEpoch::new();
        let token = epoch.capture();
        for reverse in [false, true] {
            let state = RefCell::new(
                SwitchAppsState::from_targets(
                    vec![HWND(1 as _), HWND(2 as _), HWND(3 as _)],
                    reverse,
                )
                .unwrap(),
            );
            let mut loaded = 0;
            let mut published = 0;
            let mut committed = None;
            let token = if reverse { epoch.capture() } else { token };
            build_icons_while_current(
                0..3,
                token,
                |_| {
                    loaded += 1;
                    let mut state = state.borrow_mut();
                    let mut delay = AppDelay::new(10, Instant::now(), 150);
                    delay.navigate(false);
                    delay.navigate(false);
                    delay.navigate(true);
                    state.apply_navigation(&mut delay, (0, 0));
                    state.apply_navigation(&mut delay, (0, 0)); // never double-apply
                    committed = Some(state.apps[state.keyboard_index()].1);
                    epoch.invalidate(); // release commits and cancels the builder immediately
                    Ok(())
                },
                |_| {
                    published += 1;
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(loaded, 1);
            assert_eq!(published, 0);
            assert_eq!(committed, Some(HWND(if reverse { 1 } else { 3 } as _)));
        }
        assert!(SwitchAppsState::from_targets(vec![], false).is_none());
        let mut single = SwitchAppsState::from_targets(vec![HWND(1 as _)], true).unwrap();
        single.navigate(true, (0, 0));
        assert_eq!(single.keyboard_index(), 0);
    }

    #[test]
    fn session_reset_and_queued_old_timers_cannot_reveal_a_new_round() {
        let epoch = crate::session::InputSessionEpoch::new();
        let now = Instant::now();
        let mut old = SwitchAppsPresentation {
            delay: AppDelay::new(10, now, 150),
            token: epoch.capture(),
            mouse_ready: false,
            icon_sources: None,
            loading_icons: false,
        };
        epoch.invalidate(); // lock, unlock, disconnect, or stale-modifier reset
        assert!(!old.accept_timer(10));
        assert!(!old.accepts_mouse());
        let mut new = SwitchAppsPresentation {
            delay: AppDelay::new(11, now, 150),
            token: epoch.capture(),
            mouse_ready: false,
            icon_sources: None,
            loading_icons: false,
        };
        assert!(!new.accept_timer(10)); // KillTimer does not purge a queued message
        assert!(new.accept_timer(11));
        assert!(new
            .delay
            .show(now + std::time::Duration::from_millis(150), true));
        new.mouse_ready = true;
        assert!(new.accepts_mouse());
        epoch.invalidate();
        assert!(!new.accepts_mouse());
    }

    #[test]
    fn hidden_and_showing_before_pointer_baseline_ignore_all_mouse_actions() {
        let epoch = crate::session::InputSessionEpoch::new();
        let now = Instant::now();
        let mut session = SwitchAppsPresentation {
            delay: AppDelay::new(10, now, 150),
            token: epoch.capture(),
            mouse_ready: false,
            icon_sources: None,
            loading_icons: false,
        };
        let mut interaction = SwitchAppsInteractionState::new(1, (0, 0));
        let mut clicks = 0;
        for showing in [false, true] {
            if showing {
                assert!(session
                    .delay
                    .show(now + std::time::Duration::from_millis(150), true));
            }
            if session.accepts_mouse() {
                interaction.update_hover((50, 60), Some(3));
                interaction.clear_hover();
                clicks += 1;
            }
            assert_eq!(interaction.keyboard_index(), 1);
            assert_eq!(clicks, 0);
        }
        interaction.reset_pointer_position((50, 60));
        session.mouse_ready = true;
        assert!(session.accepts_mouse());
        assert!(!interaction.update_hover((50, 60), Some(3)));
        assert!(interaction.update_hover((51, 60), Some(3)));
    }

    #[test]
    fn reset_during_icon_query_prevents_old_overlay_publication_and_paint() {
        let epoch = crate::session::InputSessionEpoch::new();
        let token = epoch.capture();
        let mut selection = Some(1);
        let mut paints = 0;
        // Simulate WM_GETICON pumping lock+unlock while the first overlay builds.
        epoch.invalidate();
        selection.take();
        epoch.invalidate();
        let published = token.run_if_current(|| selection = Some(2));
        token.run_if_current(|| paints += 1);
        assert!(published.is_none());
        assert_eq!(selection, None);
        assert_eq!(paints, 0);
        assert!(!token.is_current());
        assert!(epoch
            .capture()
            .run_if_current(|| selection = Some(3))
            .is_some());
        assert_eq!(selection, Some(3));
    }

    #[test]
    fn reset_during_window_enumeration_prevents_old_cache_and_activation() {
        let epoch = crate::session::InputSessionEpoch::new();
        let token = epoch.capture();
        let mut state = SwitchWindowsState {
            cache: Some(("old".into(), HWND::default(), 0, vec![1, 2])),
            modifier_released: false,
        };
        let mut activated = None;
        // Simulate a stale-modifier reset while enumeration pumps messages.
        epoch.invalidate();
        state.reset();
        token.run_if_current(|| {
            state.cache = Some(("candidate".into(), HWND::default(), 1, vec![1, 2]));
            state.modifier_released = false;
        });
        token.run_if_current(|| activated = Some(2));
        assert!(state.cache.is_none());
        assert!(state.modifier_released);
        assert_eq!(activated, None);
    }

    #[test]
    fn reset_after_publication_still_prevents_paint_or_activation() {
        let epoch = crate::session::InputSessionEpoch::new();
        let token = epoch.capture();
        assert_eq!(token.run_if_current(|| 1), Some(1));
        epoch.invalidate();
        assert_eq!(token.run_if_current(|| "paint"), None);
        assert_eq!(token.run_if_current(|| "activate"), None);
    }

    #[test]
    fn session_reset_clears_temporary_window_cycle() {
        let mut state = SwitchWindowsState {
            cache: Some(("test".into(), HWND::default(), 1, vec![1, 2])),
            modifier_released: false,
        };
        state.reset();
        assert!(state.cache.is_none());
        assert!(state.modifier_released);
    }

    #[test]
    fn app_switch_interaction_starts_without_hover() {
        let state = SwitchAppsInteractionState::new(1, (10, 20));

        assert_eq!(state.keyboard_index(), 1);
        assert_eq!(state.hovered_index(), None);
        assert_eq!(state.displayed_index(), 1);
    }

    #[test]
    fn app_switch_hover_updates_keyboard_selection() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));

        assert!(state.update_hover((11, 20), Some(4)));
        assert_eq!(state.keyboard_index(), 4);
        assert_eq!(state.hovered_index(), Some(4));
        assert_eq!(state.displayed_index(), 4);
        assert!(!state.update_hover((11, 20), Some(2)));
    }

    #[test]
    fn app_switch_forward_navigation_continues_from_hovered_selection() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));
        state.update_hover((11, 20), Some(4));

        state.navigate(5, false, (11, 20));

        assert_eq!(state.keyboard_index(), 0);
        assert_eq!(state.hovered_index(), None);
        assert_eq!(state.displayed_index(), 0);
    }

    #[test]
    fn app_switch_pointer_movement_restores_hover_after_keyboard_navigation() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));
        state.update_hover((11, 20), Some(4));
        state.navigate(5, false, (11, 20));

        assert_eq!(state.keyboard_index(), 0);
        assert!(state.update_hover((12, 20), Some(4)));
        assert_eq!(state.keyboard_index(), 4);
        assert_eq!(state.hovered_index(), Some(4));
        assert_eq!(state.displayed_index(), 4);
    }

    #[test]
    fn app_switch_keyboard_navigation_resets_pointer_baseline() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));
        state.update_hover((11, 20), Some(4));

        state.navigate(5, false, (30, 40));

        assert_eq!(state.keyboard_index(), 0);
        assert_eq!(state.hovered_index(), None);
        assert!(!state.update_hover((30, 40), Some(4)));
        assert_eq!(state.keyboard_index(), 0);
    }

    #[test]
    fn app_switch_reverse_navigation_continues_from_hovered_selection() {
        let mut state = SwitchAppsInteractionState::new(0, (10, 20));
        state.update_hover((11, 20), Some(2));

        state.navigate(4, true, (11, 20));

        assert_eq!(state.keyboard_index(), 1);
        assert_eq!(state.hovered_index(), None);
        assert_eq!(state.displayed_index(), 1);
    }

    #[test]
    fn app_switch_modifier_release_uses_hovered_target() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));
        state.update_hover((11, 20), Some(3));

        assert_eq!(state.keyboard_index(), 3);
    }

    #[test]
    fn app_switch_mouse_leave_keeps_last_hovered_selection() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));
        state.update_hover((11, 20), Some(3));

        assert!(state.clear_hover());
        assert_eq!(state.keyboard_index(), 3);
        assert_eq!(state.hovered_index(), None);
        assert_eq!(state.displayed_index(), 3);
        assert!(!state.clear_hover());
    }

    #[test]
    fn app_switch_pointer_moving_to_blank_keeps_last_hovered_selection() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));
        state.update_hover((11, 20), Some(3));

        assert!(state.update_hover((12, 20), None));
        assert_eq!(state.keyboard_index(), 3);
        assert_eq!(state.hovered_index(), None);
        assert_eq!(state.displayed_index(), 3);
    }

    #[test]
    fn mouse_message_position_preserves_signed_client_coordinates() {
        let x = 7i16;
        let y = -5i16;
        let packed = u32::from(x as u16) | (u32::from(y as u16) << 16);

        assert_eq!(client_pointer_position(LPARAM(packed as isize)), (7, -5));
    }

    #[test]
    fn app_switch_display_resets_pointer_baseline_without_activating_hover() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));

        state.reset_pointer_position((15, 25));

        assert_eq!(state.hovered_index(), None);
        assert!(!state.update_hover((15, 25), Some(3)));
        assert!(state.update_hover((16, 25), Some(3)));
    }

    #[test]
    fn merge_fixed_order_keeps_existing_positions_and_appends_new_windows() {
        let mut order = vec![1, 2, 3];

        merge_fixed_order(&mut order, &[3, 1, 4]);

        assert_eq!(order, vec![1, 3, 4]);
    }

    #[test]
    fn merge_fixed_order_removes_windows_not_in_current_group() {
        let mut order = vec![1, 2, 3];

        merge_fixed_order(&mut order, &[2, 3]);

        assert_eq!(order, vec![2, 3]);
    }

    #[test]
    fn rotate_fixed_order_starts_at_current_window() {
        assert_eq!(rotate_fixed_order(&[1, 2, 3], 2), Some(vec![2, 3, 1]));
    }

    #[test]
    fn rotate_fixed_order_returns_none_for_unknown_window() {
        assert_eq!(rotate_fixed_order(&[1, 2, 3], 9), None);
    }

    #[test]
    fn next_fixed_index_supports_forward_and_reverse_cycles() {
        assert_eq!(next_fixed_index(0, 3, false), 1);
        assert_eq!(next_fixed_index(2, 3, false), 0);
        assert_eq!(next_fixed_index(0, 3, true), 2);
        assert_eq!(next_fixed_index(2, 3, true), 1);
    }

    #[test]
    fn fixed_state_windows_preserves_cached_order_and_appends_stable_new_windows() {
        assert_eq!(
            reconstruct_fixed_state_windows(&[4, 1], &[1, 2, 3, 4]),
            vec![4, 1, 2, 3]
        );
    }

    #[test]
    fn normalized_fixed_index_wraps_stale_indices_and_rejects_short_cycles() {
        assert_eq!(
            normalized_fixed_index(3, 2, &[1, 2, 3], &[2, 3], false),
            Some(0)
        );
        assert_eq!(
            normalized_fixed_index(3, 2, &[1, 2, 3], &[2, 3], true),
            Some(0)
        );
        assert_eq!(
            normalized_fixed_index(9, 2, &[1, 2, 3], &[2, 3], false),
            Some(0)
        );
        assert_eq!(
            normalized_fixed_index(9, 2, &[1, 2, 4], &[2, 3], false),
            Some(1)
        );
        assert_eq!(normalized_fixed_index(1, 0, &[1], &[1], false), None);
    }
}
