use crate::config::{edit_config_file, Config};
use crate::foreground::ForegroundWatcher;
use crate::keyboard::KeyboardListener;
use crate::painter::GdiAAPainter;
use crate::startup::Startup;
use crate::trayicon::TrayIcon;
use crate::utils::{
    check_error, get_app_icon, get_foreground_window, get_process_start_time, get_window_user_data,
    is_iconic_window, is_running_as_admin, list_windows, set_foreground_window,
    set_window_user_data, AppIcon,
};

use anyhow::{anyhow, Result};
use indexmap::IndexSet;
use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
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
            GetWindowLongPtrW, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW,
            RegisterWindowMessageW, SetWindowLongPtrW, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
            CW_USEDEFAULT, GWL_STYLE, HTCLIENT, IDC_ARROW, MSG, WINDOW_STYLE, WM_COMMAND,
            WM_ERASEBKGND, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCHITTEST, WM_RBUTTONUP, WNDCLASSW,
            WS_CAPTION, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
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
    cached_icons: HashMap<String, CachedAppIcon>,
    painter: GdiAAPainter,
}

impl App {
    pub fn start(config: &Config) -> Result<()> {
        let hwnd = Self::create_window()?;
        let painter = GdiAAPainter::new(hwnd)?;

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
            cached_icons: Default::default(),
            painter,
        };

        app.set_trayicon();

        let app_ptr = Box::into_raw(Box::new(app)) as _;
        check_error(|| set_window_user_data(hwnd, app_ptr))
            .map_err(|err| anyhow!("Failed to set window ptr, {err}"))?;

        Self::eventloop()
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
                let app = get_app(hwnd)?;
                let reverse = lparam.0 == 1;
                let starting = app.switch_apps_state.is_none();
                app.switch_apps(reverse)?;
                if let Some(state) = &app.switch_apps_state {
                    app.painter.paint(state);
                }
                if starting {
                    app.reset_pointer_position();
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
                let app = get_app(hwnd)?;
                let reverse = lparam.0 == 1;
                let hwnd = app
                    .switch_apps_state
                    .as_ref()
                    .and_then(|state| state.apps.get(state.keyboard_index()).map(|(_, id)| *id))
                    .unwrap_or_else(get_foreground_window);
                app.switch_windows(hwnd, reverse)?;
                app.cancel_switch_app();
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

    fn switch_windows(&mut self, hwnd: HWND, reverse: bool) -> Result<bool> {
        let windows = list_windows(
            self.config.switch_windows_ignore_minimal,
            self.config.switch_windows_only_current_desktop(),
            self.is_admin,
        )?;
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
            None => return Ok(false),
        };
        match windows.get(&module_path) {
            None => Ok(false),
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
                    return Ok(false);
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
                                None => return Ok(false),
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
                self.switch_windows_state = SwitchWindowsState {
                    cache: Some((module_path.clone(), state_id, index, state_windows)),
                    modifier_released: false,
                };
                set_foreground_window(hwnd);

                Ok(true)
            }
        }
    }

    fn switch_apps(&mut self, reverse: bool) -> Result<()> {
        debug!(
            "switch apps: reverse:{reverse}, state:{:?}",
            self.switch_apps_state
        );
        if let Some(state) = self.switch_apps_state.as_mut() {
            let mut pointer_position = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut pointer_position);
            }
            state.navigate(reverse, (pointer_position.x, pointer_position.y));
            debug!("switch apps: new index:{}", state.keyboard_index());
            return Ok(());
        }
        let windows = list_windows(
            self.config.switch_apps_ignore_minimal,
            self.config.switch_apps_only_current_desktop(),
            self.is_admin,
        )?;
        let mut apps = vec![];
        for (module_path, hwnds) in windows.iter() {
            let module_hwnd = if is_iconic_window(hwnds[0].0) {
                hwnds[hwnds.len() - 1].0
            } else {
                hwnds[0].0
            };
            let identities = hwnds
                .iter()
                .map(|(hwnd, _)| get_process_start_time(*hwnd))
                .collect::<Option<Vec<_>>>()
                .map(|mut identities| {
                    identities.sort_unstable();
                    identities.dedup();
                    identities
                });
            let icon = self
                .cached_icons
                .get(module_path)
                .filter(|cached| {
                    identities.is_some() && cached.identities.as_ref() == identities.as_ref()
                })
                .map(|cached| Rc::clone(&cached.icon));
            let icon = icon.unwrap_or_else(|| {
                let icon = Rc::new(get_app_icon(
                    &self.config.switch_apps_override_icons,
                    module_path,
                    module_hwnd,
                ));
                self.cached_icons.insert(
                    module_path.clone(),
                    CachedAppIcon {
                        icon: Rc::clone(&icon),
                        identities,
                    },
                );
                icon
            });
            apps.push((icon, module_hwnd));
        }
        let num_apps = apps.len() as i32;
        if num_apps == 0 {
            return Ok(());
        }

        let index = if apps.len() == 1 {
            0
        } else if reverse {
            apps.len() - 1
        } else {
            1
        };

        let mut pointer_position = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pointer_position);
        }
        let state = SwitchAppsState {
            apps,
            interaction: SwitchAppsInteractionState::new(
                index,
                (pointer_position.x, pointer_position.y),
            ),
        };
        self.switch_apps_state = Some(state);
        debug!("switch apps, new state:{:?}", self.switch_apps_state);
        Ok(())
    }

    fn click(&mut self, lparam: LPARAM) {
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
            self.painter.paint(state);
        }
    }

    fn clear_hover(&mut self) {
        if let Some(state) = self.switch_apps_state.as_mut() {
            if state.clear_hover() {
                self.painter.paint(state);
            }
        }
    }

    fn reset_pointer_position(&mut self) {
        let Some(state) = self.switch_apps_state.as_mut() else {
            return;
        };
        let mut pointer_position = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pointer_position);
        }
        state.reset_pointer_position((pointer_position.x, pointer_position.y));
    }

    fn do_switch_app(&mut self) {
        let index = self
            .switch_apps_state
            .as_ref()
            .map(SwitchAppsState::keyboard_index);
        if let Some(index) = index {
            self.do_switch_app_at(index);
        }
    }

    fn do_switch_app_at(&mut self, index: usize) {
        if let Some(state) = self.switch_apps_state.take() {
            if let Some((_, id)) = state.apps.get(index) {
                set_foreground_window(*id);
            }
            self.painter.unpaint(state);
        }
    }

    fn cancel_switch_app(&mut self) {
        if let Some(state) = self.switch_apps_state.take() {
            self.painter.unpaint(state);
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.cached_icons.clear();
    }
}

fn get_app(hwnd: HWND) -> Result<&'static mut App> {
    unsafe {
        let ptr = check_error(|| get_window_user_data(hwnd))
            .map_err(|err| anyhow!("Failed to get window ptr, {err}"))?;
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

#[derive(Debug)]
struct SwitchWindowsState {
    cache: Option<(String, HWND, usize, Vec<isize>)>,
    modifier_released: bool,
}

#[derive(Debug)]
pub struct SwitchAppsState {
    pub apps: Vec<(Rc<AppIcon>, HWND)>,
    interaction: SwitchAppsInteractionState,
}

impl SwitchAppsState {
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

#[derive(Debug)]
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

struct CachedAppIcon {
    icon: Rc<AppIcon>,
    identities: Option<Vec<u64>>,
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
    fn app_switch_interaction_starts_without_hover() {
        let state = SwitchAppsInteractionState::new(1, (10, 20));

        assert_eq!(state.keyboard_index(), 1);
        assert_eq!(state.hovered_index(), None);
        assert_eq!(state.displayed_index(), 1);
    }

    #[test]
    fn app_switch_hover_changes_display_without_changing_keyboard_index() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));

        assert!(state.update_hover((11, 20), Some(4)));
        assert_eq!(state.keyboard_index(), 1);
        assert_eq!(state.hovered_index(), Some(4));
        assert_eq!(state.displayed_index(), 4);
        assert!(!state.update_hover((11, 20), Some(2)));
    }

    #[test]
    fn app_switch_forward_navigation_continues_from_keyboard_index() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));
        state.update_hover((11, 20), Some(4));

        state.navigate(5, false, (11, 20));

        assert_eq!(state.keyboard_index(), 2);
        assert_eq!(state.hovered_index(), None);
        assert_eq!(state.displayed_index(), 2);
    }

    #[test]
    fn app_switch_pointer_movement_restores_hover_after_keyboard_navigation() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));
        state.update_hover((11, 20), Some(4));
        state.navigate(5, false, (11, 20));

        assert!(state.update_hover((12, 20), Some(4)));
        assert_eq!(state.keyboard_index(), 2);
        assert_eq!(state.hovered_index(), Some(4));
        assert_eq!(state.displayed_index(), 4);
    }

    #[test]
    fn app_switch_keyboard_navigation_resets_pointer_baseline() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));
        state.update_hover((11, 20), Some(4));

        state.navigate(5, false, (30, 40));

        assert_eq!(state.keyboard_index(), 2);
        assert_eq!(state.hovered_index(), None);
        assert!(!state.update_hover((30, 40), Some(4)));
    }

    #[test]
    fn app_switch_reverse_navigation_wraps_from_keyboard_index() {
        let mut state = SwitchAppsInteractionState::new(0, (10, 20));
        state.update_hover((11, 20), Some(2));

        state.navigate(4, true, (11, 20));

        assert_eq!(state.keyboard_index(), 3);
        assert_eq!(state.hovered_index(), None);
        assert_eq!(state.displayed_index(), 3);
    }

    #[test]
    fn app_switch_modifier_release_keeps_keyboard_target_while_hovered() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));
        state.update_hover((11, 20), Some(3));

        assert_eq!(state.keyboard_index(), 1);
    }

    #[test]
    fn app_switch_mouse_leave_clears_hover_without_changing_keyboard_index() {
        let mut state = SwitchAppsInteractionState::new(1, (10, 20));
        state.update_hover((11, 20), Some(3));

        assert!(state.clear_hover());
        assert_eq!(state.keyboard_index(), 1);
        assert_eq!(state.hovered_index(), None);
        assert_eq!(state.displayed_index(), 1);
        assert!(!state.clear_hover());
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
