use crate::app::SwitchAppsState;
use crate::utils::{
    check_error, draw_normalized_icon, get_moinitor_rect, is_light_theme, is_win11,
    normalized_hicon_rect,
};

use anyhow::{Context, Result};
use std::ffi::c_void;
use windows::Win32::{
    Foundation::{COLORREF, HWND, POINT, RECT, SIZE},
    Graphics::{
        Gdi::{
            BitBlt, CreateCompatibleDC, CreateDIBSection, CreateRoundRectRgn, CreateSolidBrush,
            DeleteDC, DeleteObject, FillRect, FillRgn, GetDC, ReleaseDC, SelectObject,
            SetStretchBltMode, StretchBlt, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER,
            BLENDFUNCTION, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, RGBQUAD, SRCCOPY,
        },
        GdiPlus::{
            FillModeAlternate, GdipAddPathArc, GdipClosePathFigure, GdipCreateFromHDC,
            GdipCreatePath, GdipCreatePen1, GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePath,
            GdipDeletePen, GdipFillPath, GdipFillRectangle, GdipGetPenBrushFill,
            GdipSetInterpolationMode, GdipSetSmoothingMode, GdiplusShutdown, GdiplusStartup,
            GdiplusStartupInput, GpBrush, GpGraphics, GpPath, GpPen,
            InterpolationModeHighQualityBicubic, SmoothingModeAntiAlias, Unit,
        },
    },
    UI::{
        HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::SetFocus,
        WindowsAndMessaging::{
            DrawIconEx, GetClientRect, ShowWindow, UpdateLayeredWindow, DI_NORMAL, SW_HIDE,
            SW_SHOW, ULW_ALPHA,
        },
    },
};

pub const BG_DARK_COLOR: u32 = 0x4c4c4c;
pub const FG_DARK_COLOR: u32 = 0x3b3b3b;
pub const BG_LIGHT_COLOR: u32 = 0xe0e0e0;
pub const FG_LIGHT_COLOR: u32 = 0xf2f2f2;
pub const ALPHA_MASK: u32 = 0xff000000;
pub const ICON_SIZE_BASE: i32 = 64;
pub const WINDOW_BORDER_SIZE_BASE: i32 = 10;
pub const ICON_BORDER_SIZE_BASE: i32 = 4;
pub const SCALE_FACTOR: i32 = 6;

unsafe fn create_top_down_dib(hdc: HDC, width: i32, height: i32) -> Option<(HBITMAP, *mut u8)> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let bitmap_info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            ..Default::default()
        },
        bmiColors: [RGBQUAD::default()],
    };
    let mut bits = std::ptr::null_mut::<c_void>();
    let bitmap =
        CreateDIBSection(Some(hdc), &bitmap_info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(bitmap.into());
        return None;
    }
    std::slice::from_raw_parts_mut(bits.cast::<u8>(), (width * height * 4) as usize).fill(0);
    Some((bitmap, bits.cast()))
}

fn make_surface_opaque(bits: *mut u8, width: i32, height: i32) {
    if bits.is_null() || width <= 0 || height <= 0 {
        return;
    }
    unsafe {
        let pixels = std::slice::from_raw_parts_mut(bits, (width * height * 4) as usize);
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = u8::MAX;
        }
    }
}

// GDI Antialiasing Painter
pub struct GdiAAPainter {
    token: usize,
    hwnd: HWND,
    hdc_screen: HDC,
    rounded_corner: bool,
    show: bool,
}

impl GdiAAPainter {
    pub fn new(hwnd: HWND) -> Result<Self> {
        let startup_input = GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let mut token: usize = 0;
        check_error(|| unsafe { GdiplusStartup(&mut token, &startup_input, std::ptr::null_mut()) })
            .context("Failed to initialize GDI+")?;

        let hdc_screen = unsafe { GetDC(Some(hwnd)) };
        let rounded_corner = is_win11();

        Ok(Self {
            token,
            hwnd,
            hdc_screen,
            rounded_corner,
            show: false,
        })
    }

    pub fn paint(&mut self, state: &SwitchAppsState) {
        let dpi_scale = get_dpi_scale(self.hwnd);
        let icon_size_max = (ICON_SIZE_BASE as f64 * dpi_scale) as i32;
        let border_size = (WINDOW_BORDER_SIZE_BASE as f64 * dpi_scale) as i32;
        let icon_border = (ICON_BORDER_SIZE_BASE as f64 * dpi_scale) as i32;

        let Coordinate {
            x,
            y,
            width,
            height,
            icon_size,
            item_size,
        } = Coordinate::new(
            state.apps.len() as i32,
            icon_size_max,
            border_size,
            icon_border,
        );

        let corner_radius = if self.rounded_corner {
            item_size / 4
        } else {
            0
        };

        let hwnd = self.hwnd;
        let hdc_screen = self.hdc_screen;

        let (fg_color, bg_color) = theme_color(is_light_theme());

        unsafe {
            let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
            let Some((bitmap_mem, _bits_mem)) = create_top_down_dib(hdc_screen, width, height)
            else {
                let _ = DeleteDC(hdc_mem);
                return;
            };
            SelectObject(hdc_mem, bitmap_mem.into());

            let mut graphics = GpGraphics::default();
            let mut graphics_ptr: *mut GpGraphics = &mut graphics;
            GdipCreateFromHDC(hdc_mem, &mut graphics_ptr as _);
            GdipSetSmoothingMode(graphics_ptr, SmoothingModeAntiAlias);
            GdipSetInterpolationMode(graphics_ptr, InterpolationModeHighQualityBicubic);

            let mut bg_pen = GpPen::default();
            let mut bg_pen_ptr: *mut GpPen = &mut bg_pen;
            GdipCreatePen1(ALPHA_MASK | bg_color, 0.0, Unit(0), &mut bg_pen_ptr as _);

            let mut bg_brush = GpBrush::default();
            let mut bg_brush_ptr: *mut GpBrush = &mut bg_brush;
            GdipGetPenBrushFill(bg_pen_ptr, &mut bg_brush_ptr as _);

            if self.rounded_corner {
                draw_round_rect(
                    graphics_ptr,
                    bg_brush_ptr,
                    0.0,
                    0.0,
                    width as f32,
                    height as f32,
                    corner_radius as f32,
                );
            } else {
                GdipFillRectangle(
                    graphics_ptr,
                    bg_brush_ptr,
                    0.0,
                    0.0,
                    width as f32,
                    height as f32,
                );
            }
            GdipDeleteBrush(bg_brush_ptr);
            GdipDeletePen(bg_pen_ptr);
            GdipDeleteGraphics(graphics_ptr);

            let icons_width = item_size * state.apps.len() as i32;
            let icons_height = item_size;
            let bitmap_icons = draw_icons(
                state,
                hdc_screen,
                icon_size,
                icon_border,
                icons_width,
                icons_height,
                corner_radius,
                fg_color,
                bg_color,
            );
            if bitmap_icons.is_invalid() {
                let _ = DeleteDC(hdc_mem);
                let _ = DeleteObject(bitmap_mem.into());
                return;
            }

            let hdc_icons = CreateCompatibleDC(Some(hdc_screen));
            SelectObject(hdc_icons, bitmap_icons.into());
            let _ = BitBlt(
                hdc_mem,
                border_size,
                border_size,
                icons_width,
                icons_height,
                Some(hdc_icons),
                0,
                0,
                SRCCOPY,
            );
            let _ = DeleteDC(hdc_icons);

            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as _,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as _,
                ..Default::default()
            };
            let _ = UpdateLayeredWindow(
                hwnd,
                Some(hdc_screen),
                Some(&POINT { x, y }),
                Some(&SIZE {
                    cx: width,
                    cy: height,
                }),
                Some(hdc_mem),
                Some(&POINT::default()),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );

            let _ = DeleteDC(hdc_mem);
            let _ = DeleteObject(bitmap_icons.into());
            let _ = DeleteObject(bitmap_mem.into());
        }

        if self.show {
            return;
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetFocus(Some(self.hwnd));
        }
        self.show = true;
    }

    pub fn unpaint(&mut self, _state: SwitchAppsState) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.show = false;
    }

    pub fn find_app_index_at_client(
        &self,
        state: &SwitchAppsState,
        pointer_position: (i32, i32),
    ) -> Option<usize> {
        let dpi_scale = get_dpi_scale(self.hwnd);
        let border_size = (WINDOW_BORDER_SIZE_BASE as f64 * dpi_scale) as i32;
        let mut client_rect = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut client_rect);
        }
        app_item_index_in_panel(
            pointer_position,
            client_rect.right - client_rect.left,
            border_size,
            state.apps.len(),
        )
    }
}

fn app_item_index_in_panel(
    pointer_position: (i32, i32),
    panel_width: i32,
    border_size: i32,
    app_count: usize,
) -> Option<usize> {
    if app_count == 0 {
        return None;
    }
    let item_size = (panel_width - border_size * 2) / app_count as i32;
    app_item_index_at(pointer_position, (0, 0), border_size, item_size, app_count)
}

fn app_item_index_at(
    pointer_position: (i32, i32),
    panel_origin: (i32, i32),
    border_size: i32,
    item_size: i32,
    app_count: usize,
) -> Option<usize> {
    if item_size <= 0 {
        return None;
    }
    let x = pointer_position.0 - panel_origin.0 - border_size;
    let y = pointer_position.1 - panel_origin.1 - border_size;
    if x < 0 || y < 0 || y >= item_size {
        return None;
    }
    let index = (x / item_size) as usize;
    (index < app_count).then_some(index)
}

impl Drop for GdiAAPainter {
    fn drop(&mut self) {
        unsafe {
            ReleaseDC(Some(self.hwnd), self.hdc_screen);
            GdiplusShutdown(self.token);
        }
    }
}

const fn theme_color(light_theme: bool) -> (u32, u32) {
    match light_theme {
        true => (FG_LIGHT_COLOR, BG_LIGHT_COLOR),
        false => (FG_DARK_COLOR, BG_DARK_COLOR),
    }
}

unsafe fn draw_round_rect(
    graphic_ptr: *mut GpGraphics,
    brush_ptr: *mut GpBrush,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    corner_radius: f32,
) {
    unsafe {
        let mut path = GpPath::default();
        let mut path_ptr: *mut GpPath = &mut path;
        GdipCreatePath(FillModeAlternate, &mut path_ptr as _);
        GdipAddPathArc(
            path_ptr,
            left,
            top,
            corner_radius,
            corner_radius,
            180.0,
            90.0,
        );
        GdipAddPathArc(
            path_ptr,
            right - corner_radius,
            top,
            corner_radius,
            corner_radius,
            270.0,
            90.0,
        );
        GdipAddPathArc(
            path_ptr,
            right - corner_radius,
            bottom - corner_radius,
            corner_radius,
            corner_radius,
            0.0,
            90.0,
        );
        GdipAddPathArc(
            path_ptr,
            left,
            bottom - corner_radius,
            corner_radius,
            corner_radius,
            90.0,
            90.0,
        );
        GdipClosePathFigure(path_ptr);
        GdipFillPath(graphic_ptr, brush_ptr, path_ptr);
        GdipDeletePath(path_ptr);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_icons(
    state: &SwitchAppsState,
    hdc_screen: HDC,
    icon_size: i32,
    icon_border: i32,
    width: i32,
    height: i32,
    corner_radius: i32,
    fg_color: u32,
    bg_color: u32,
) -> HBITMAP {
    let scaled_width = width * SCALE_FACTOR;
    let scaled_height = height * SCALE_FACTOR;
    let scaled_corner_radius = corner_radius * SCALE_FACTOR;
    let scaled_border_size = icon_border * SCALE_FACTOR;
    let scaled_icon_inner_size = icon_size * SCALE_FACTOR;
    let scaled_icon_outer_size = scaled_icon_inner_size + scaled_border_size * 2;

    unsafe {
        let hdc_tmp = CreateCompatibleDC(Some(hdc_screen));
        let Some((bitmap_tmp, bits_tmp)) = create_top_down_dib(hdc_screen, width, height) else {
            let _ = DeleteDC(hdc_tmp);
            return HBITMAP::default();
        };
        SelectObject(hdc_tmp, bitmap_tmp.into());

        let hdc_scaled = CreateCompatibleDC(Some(hdc_screen));
        let Some((bitmap_scaled, _bits_scaled)) =
            create_top_down_dib(hdc_screen, scaled_width, scaled_height)
        else {
            let _ = DeleteDC(hdc_tmp);
            let _ = DeleteObject(bitmap_tmp.into());
            let _ = DeleteDC(hdc_scaled);
            return HBITMAP::default();
        };
        SelectObject(hdc_scaled, bitmap_scaled.into());

        let fg_brush = CreateSolidBrush(COLORREF(fg_color));
        let bg_brush = CreateSolidBrush(COLORREF(bg_color));

        let mut icon_graphics_ptr: *mut GpGraphics = std::ptr::null_mut();
        let icon_graphics_ready = GdipCreateFromHDC(hdc_scaled, &mut icon_graphics_ptr as _).0 == 0;
        if icon_graphics_ready {
            GdipSetInterpolationMode(icon_graphics_ptr, InterpolationModeHighQualityBicubic);
        }

        let rect = RECT {
            left: 0,
            top: 0,
            right: scaled_width,
            bottom: scaled_height,
        };

        FillRect(hdc_scaled, &rect, bg_brush);

        let displayed_index = state.displayed_index();
        for (i, (icon, _)) in state.apps.iter().enumerate() {
            // draw the box for selected icon
            if i == displayed_index {
                let left = scaled_icon_outer_size * (i as i32);
                let top = 0;
                let right = left + scaled_icon_outer_size;
                let bottom = top + scaled_icon_outer_size;
                let rgn = CreateRoundRectRgn(
                    left,
                    top,
                    right,
                    bottom,
                    scaled_corner_radius,
                    scaled_corner_radius,
                );
                let _ = FillRgn(hdc_scaled, rgn, fg_brush);
                let _ = DeleteObject(rgn.into());
            }

            let Some(candidate) = icon.selected(scaled_icon_inner_size) else {
                continue;
            };
            let cx = scaled_border_size + scaled_icon_outer_size * i as i32;
            let normalized = icon_graphics_ready
                && candidate.has_bitmap()
                && draw_normalized_icon(
                    icon_graphics_ptr,
                    candidate,
                    cx as f32,
                    scaled_border_size as f32,
                    scaled_icon_inner_size,
                );
            if !normalized {
                if candidate.has_bitmap() {
                    debug!("normalized app icon failed at index {i}, using direct DrawIconEx");
                }
                let (x, y, width, height) = normalized_hicon_rect(
                    candidate,
                    cx,
                    scaled_border_size,
                    scaled_icon_inner_size,
                );
                let _ = DrawIconEx(
                    hdc_scaled,
                    x,
                    y,
                    candidate.hicon,
                    width,
                    height,
                    0,
                    None,
                    DI_NORMAL,
                );
            }
        }
        if icon_graphics_ready {
            GdipDeleteGraphics(icon_graphics_ptr);
        }

        SetStretchBltMode(hdc_tmp, HALFTONE);
        let _ = StretchBlt(
            hdc_tmp,
            0,
            0,
            width,
            height,
            Some(hdc_scaled),
            0,
            0,
            scaled_width,
            scaled_height,
            SRCCOPY,
        );
        make_surface_opaque(bits_tmp, width, height);

        let _ = DeleteObject(fg_brush.into());
        let _ = DeleteObject(bg_brush.into());
        let _ = DeleteDC(hdc_scaled);
        let _ = DeleteObject(bitmap_scaled.into());
        let _ = DeleteDC(hdc_tmp);

        bitmap_tmp
    }
}

fn get_dpi_scale(hwnd: HWND) -> f64 {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        if dpi == 0 {
            1.0
        } else {
            dpi as f64 / 96.0
        }
    }
}

struct Coordinate {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    icon_size: i32,
    item_size: i32,
}

impl Coordinate {
    fn new(num_apps: i32, icon_size_max: i32, border_size: i32, icon_border: i32) -> Self {
        let monitor_rect = get_moinitor_rect();
        let monitor_width = monitor_rect.right - monitor_rect.left;
        let monitor_height = monitor_rect.bottom - monitor_rect.top;

        let available_icon_size = (monitor_width - 2 * border_size) / num_apps - icon_border * 2;
        let min_icon_size = (32.0 * (icon_size_max as f64 / ICON_SIZE_BASE as f64)) as i32;
        let icon_size = if available_icon_size >= min_icon_size {
            available_icon_size.min(icon_size_max)
        } else {
            available_icon_size.max(1)
        };

        let item_size = icon_size + icon_border * 2;
        let width = item_size * num_apps + border_size * 2;
        let height = item_size + border_size * 2;
        let x = monitor_rect.left + (monitor_width - width) / 2;
        let y = monitor_rect.top + (monitor_height - height) / 2;

        Self {
            x,
            y,
            width,
            height,
            icon_size,
            item_size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_item_hit_test_uses_full_item_rectangles_and_excludes_panel_border() {
        let origin = (100, 50);

        assert_eq!(app_item_index_at((110, 60), origin, 10, 20, 3), Some(0));
        assert_eq!(app_item_index_at((129, 79), origin, 10, 20, 3), Some(0));
        assert_eq!(app_item_index_at((130, 60), origin, 10, 20, 3), Some(1));
        assert_eq!(app_item_index_at((109, 60), origin, 10, 20, 3), None);
        assert_eq!(app_item_index_at((110, 80), origin, 10, 20, 3), None);
        assert_eq!(app_item_index_at((170, 60), origin, 10, 20, 3), None);
    }

    #[test]
    fn app_item_hit_test_derives_item_size_from_rendered_panel_width() {
        assert_eq!(app_item_index_in_panel((10, 10), 80, 10, 3), Some(0));
        assert_eq!(app_item_index_in_panel((49, 29), 80, 10, 3), Some(1));
        assert_eq!(app_item_index_in_panel((69, 29), 80, 10, 3), Some(2));
        assert_eq!(app_item_index_in_panel((70, 10), 80, 10, 3), None);
        assert_eq!(app_item_index_in_panel((10, 30), 80, 10, 3), None);
    }

    #[test]
    fn gdiplus_hdc_surface_uses_premultiplied_alpha() {
        unsafe {
            let startup_input = GdiplusStartupInput {
                GdiplusVersion: 1,
                ..Default::default()
            };
            let mut token = 0;
            assert_eq!(
                GdiplusStartup(&mut token, &startup_input, std::ptr::null_mut()).0,
                0
            );

            let screen_dc = GetDC(None);
            let memory_dc = CreateCompatibleDC(Some(screen_dc));
            let (bitmap, bits) = create_top_down_dib(screen_dc, 1, 1).unwrap();
            SelectObject(memory_dc, bitmap.into());

            let mut graphics = std::ptr::null_mut();
            assert_eq!(GdipCreateFromHDC(memory_dc, &mut graphics).0, 0);
            let mut pen = std::ptr::null_mut();
            assert_eq!(GdipCreatePen1(0x80ff0000, 0.0, Unit(0), &mut pen).0, 0);
            let mut brush = std::ptr::null_mut();
            assert_eq!(GdipGetPenBrushFill(pen, &mut brush).0, 0);
            assert_eq!(GdipFillRectangle(graphics, brush, 0.0, 0.0, 1.0, 1.0).0, 0);

            GdipDeleteBrush(brush);
            GdipDeletePen(pen);
            GdipDeleteGraphics(graphics);

            let pixel = std::slice::from_raw_parts(bits, 4);
            assert_eq!(pixel, [0, 0, 128, 128]);

            let _ = DeleteDC(memory_dc);
            let _ = DeleteObject(bitmap.into());
            let _ = ReleaseDC(None, screen_dc);
            GdiplusShutdown(token);
        }
    }
}
