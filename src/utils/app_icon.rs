use super::to_wstring;

use std::{
    fs::{self, File},
    io::BufReader,
    mem,
    path::{Path, PathBuf},
    time,
};

use indexmap::IndexMap;
use windows::Win32::Graphics::GdiPlus::{
    BitmapData, GdipBitmapLockBits, GdipBitmapUnlockBits, GdipCreateBitmapFromFile,
    GdipCreateBitmapFromScan0, GdipCreateHICONFromBitmap, GdipDisposeImage, GdipDrawImageRectRect,
    GdipGetImageHeight, GdipGetImageWidth, GpBitmap, GpGraphics, GpImage, ImageLockModeRead,
    ImageLockModeWrite, Ok as GdiPlusOk, Rect, UnitPixel,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        Graphics::Gdi::{
            CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC,
            BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, RGBQUAD,
        },
        Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
        UI::{
            Controls::IImageList,
            Shell::{SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_SYSICONINDEX},
            WindowsAndMessaging::{
                CopyIcon, DestroyIcon, GetIconInfo, LoadIconW, LoadImageW, SendMessageTimeoutW,
                GCLP_HICON, HICON, ICONINFO, ICON_BIG, ICON_SMALL2, IDI_APPLICATION, IMAGE_ICON,
                LR_DEFAULTCOLOR, LR_LOADFROMFILE, SMTO_ABORTIFHUNG, WM_GETICON,
            },
        },
    },
};
use xml::reader::XmlEvent;
use xml::EventReader;

const ALPHA_THRESHOLD: u8 = 8;
const VISIBLE_CONTENT_PERCENT: f32 = 0.70;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PixelBounds {
    canvas_width: i32,
    canvas_height: i32,
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
}

impl PixelBounds {
    fn full(width: i32, height: i32) -> Self {
        Self {
            canvas_width: width,
            canvas_height: height,
            min_x: 0,
            min_y: 0,
            max_x: width - 1,
            max_y: height - 1,
        }
    }

    fn width(self) -> i32 {
        self.max_x - self.min_x + 1
    }

    fn height(self) -> i32 {
        self.max_y - self.min_y + 1
    }

    fn is_full(self) -> bool {
        self.min_x == 0
            && self.min_y == 0
            && self.max_x == self.canvas_width - 1
            && self.max_y == self.canvas_height - 1
    }
}

pub struct IconCandidate {
    pub(crate) hicon: HICON,
    pub width: i32,
    pub height: i32,
    bitmap: Option<*mut GpBitmap>,
    bounds: PixelBounds,
}

impl IconCandidate {
    pub(crate) fn has_bitmap(&self) -> bool {
        self.bitmap.is_some()
    }
}

pub struct AppIcon {
    candidates: Vec<IconCandidate>,
}

impl std::fmt::Debug for AppIcon {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppIcon")
            .field("candidate_count", &self.candidates.len())
            .finish()
    }
}

/// Draws a cached canonical bitmap into a GDI+ graphics context.
///
/// # Safety
/// `graphics` must be a valid GDI+ graphics pointer for the duration of the call.
pub unsafe fn draw_normalized_icon(
    graphics: *mut GpGraphics,
    candidate: &IconCandidate,
    x: f32,
    y: f32,
    target_size: i32,
) -> bool {
    unsafe {
        let Some(bitmap) = candidate.bitmap else {
            return false;
        };
        let (dest_x, dest_y, dest_width, dest_height) =
            normalized_canvas_rect(candidate, x, y, target_size);
        let result = GdipDrawImageRectRect(
            graphics,
            bitmap as *mut GpImage,
            dest_x,
            dest_y,
            dest_width,
            dest_height,
            0.0,
            0.0,
            candidate.width as f32,
            candidate.height as f32,
            UnitPixel,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
        );
        result == GdiPlusOk
    }
}

fn normalized_canvas_rect(
    candidate: &IconCandidate,
    x: f32,
    y: f32,
    target_size: i32,
) -> (f32, f32, f32, f32) {
    let bounds = candidate.bounds;
    let (_, _, visible_width, visible_height) = normalized_rect(bounds, target_size);
    let scale = visible_width.max(visible_height).max(1) as f32
        / bounds.width().max(bounds.height()).max(1) as f32;
    let bounds_center_x = (bounds.min_x + bounds.max_x + 1) as f32 / 2.0;
    let bounds_center_y = (bounds.min_y + bounds.max_y + 1) as f32 / 2.0;
    (
        x + target_size as f32 / 2.0 - bounds_center_x * scale,
        y + target_size as f32 / 2.0 - bounds_center_y * scale,
        candidate.width as f32 * scale,
        candidate.height as f32 * scale,
    )
}

pub(crate) fn normalized_hicon_rect(
    candidate: &IconCandidate,
    x: i32,
    y: i32,
    target_size: i32,
) -> (i32, i32, i32, i32) {
    let (x, y, width, height) =
        normalized_canvas_rect(candidate, x as f32, y as f32, target_size);
    (
        x.round() as i32,
        y.round() as i32,
        (width.round() as i32).max(1),
        (height.round() as i32).max(1),
    )
}

unsafe fn canonicalize_bitmap(
    source: *mut GpBitmap,
    fallback_bounds: Option<PixelBounds>,
) -> Option<(*mut GpBitmap, i32, i32, PixelBounds)> {
    let image = source as *mut GpImage;
    let mut width = 0;
    let mut height = 0;
    if GdipGetImageWidth(image, &mut width) != GdiPlusOk
        || GdipGetImageHeight(image, &mut height) != GdiPlusOk
    {
        return None;
    }
    let width = i32::try_from(width).ok()?.max(1);
    let height = i32::try_from(height).ok()?.max(1);

    let mut pixels = lock_bitmap_pixels(source, width, height, ImageLockModeRead.0 as u32)?;
    premultiply_alpha(&mut pixels);
    let bounds = select_bounds(
        alpha_bounds(width, height, &pixels),
        fallback_bounds,
        width,
        height,
        &pixels,
    );

    let mut canonical = std::ptr::null_mut();
    if GdipCreateBitmapFromScan0(
        width,
        height,
        width * 4,
        PIXEL_FORMAT_32BPP_PARGB,
        None,
        &mut canonical,
    ) != GdiPlusOk
    {
        return None;
    }

    if !write_bitmap_pixels(canonical, width, height, &pixels) {
        let _ = GdipDisposeImage(canonical as *mut GpImage);
        return None;
    }

    Some((canonical, width, height, bounds))
}

unsafe fn lock_bitmap_pixels(
    bitmap: *mut GpBitmap,
    width: i32,
    height: i32,
    lock_mode: u32,
) -> Option<Vec<u8>> {
    let rect = Rect {
        X: 0,
        Y: 0,
        Width: width,
        Height: height,
    };
    let mut data = BitmapData::default();
    let status = GdipBitmapLockBits(bitmap, &rect, lock_mode, PIXEL_FORMAT_32BPP_ARGB, &mut data);
    if status != GdiPlusOk {
        return None;
    }
    if data.Scan0.is_null() || data.Stride == 0 {
        let _ = GdipBitmapUnlockBits(bitmap, &mut data);
        return None;
    }

    let mut pixels = vec![0; (width * height * 4) as usize];
    for y in 0..height as isize {
        let source = data.Scan0.cast::<u8>().offset(y * data.Stride as isize);
        let source = std::slice::from_raw_parts(source, width as usize * 4);
        let offset = y as usize * width as usize * 4;
        pixels[offset..offset + source.len()].copy_from_slice(source);
    }
    let _ = GdipBitmapUnlockBits(bitmap, &mut data);
    Some(pixels)
}

unsafe fn write_bitmap_pixels(
    bitmap: *mut GpBitmap,
    width: i32,
    height: i32,
    pixels: &[u8],
) -> bool {
    let rect = Rect {
        X: 0,
        Y: 0,
        Width: width,
        Height: height,
    };
    let mut data = BitmapData::default();
    let status = GdipBitmapLockBits(
        bitmap,
        &rect,
        ImageLockModeWrite.0 as u32,
        PIXEL_FORMAT_32BPP_PARGB,
        &mut data,
    );
    if status != GdiPlusOk {
        return false;
    }
    if data.Scan0.is_null() || data.Stride == 0 {
        let _ = GdipBitmapUnlockBits(bitmap, &mut data);
        return false;
    }

    let row_len = width as usize * 4;
    let valid = pixels.len() >= row_len * height as usize;
    if valid {
        for y in 0..height as isize {
            let destination = data.Scan0.cast::<u8>().offset(y * data.Stride as isize);
            let destination = std::slice::from_raw_parts_mut(destination, row_len);
            let offset = y as usize * row_len;
            destination.copy_from_slice(&pixels[offset..offset + row_len]);
        }
    }
    let _ = GdipBitmapUnlockBits(bitmap, &mut data);
    valid
}

impl AppIcon {
    fn new() -> Self {
        Self {
            candidates: Vec::new(),
        }
    }

    fn push(&mut self, hicon: HICON) {
        if hicon.is_invalid() {
            return;
        }
        let (width, height) = get_icon_dimensions(hicon).unwrap_or((256, 256));
        let bounds = get_icon_bounds(hicon).map(|bounds| PixelBounds {
            canvas_width: bounds.canvas_width,
            canvas_height: bounds.canvas_height,
            min_x: bounds.min_x,
            min_y: bounds.min_y,
            max_x: bounds.max_x,
            max_y: bounds.max_y,
        });
        self.push_candidate(hicon, None, width, height, bounds);
    }

    fn push_candidate(
        &mut self,
        hicon: HICON,
        canonical: Option<(*mut GpBitmap, i32, i32, PixelBounds)>,
        fallback_width: i32,
        fallback_height: i32,
        fallback_bounds: Option<PixelBounds>,
    ) {
        let (bitmap, width, height, bounds) = canonical
            .map(|(bitmap, width, height, bounds)| (Some(bitmap), width, height, bounds))
            .unwrap_or_else(|| {
                let width = fallback_width.max(1);
                let height = fallback_height.max(1);
                (
                    None,
                    width,
                    height,
                    fallback_bounds.unwrap_or_else(|| PixelBounds::full(width, height)),
                )
            });
        self.candidates.push(IconCandidate {
            hicon,
            width,
            height,
            bitmap,
            bounds,
        });
    }

    fn from_bitmap(bitmap: *mut GpBitmap, hicon: HICON) -> Self {
        let mut icon = Self::new();
        let fallback_dimensions = get_icon_dimensions(hicon).unwrap_or((256, 256));
        let canonical = unsafe { canonicalize_bitmap(bitmap, None) };
        unsafe {
            let _ = GdipDisposeImage(bitmap as *mut GpImage);
        }
        icon.push_candidate(
            hicon,
            canonical,
            fallback_dimensions.0,
            fallback_dimensions.1,
            None,
        );
        icon
    }

    fn from_hicon(hicon: HICON) -> Self {
        let mut icon = Self::new();
        icon.push(hicon);
        icon
    }

    pub fn selected(&self, target_size: i32) -> Option<&IconCandidate> {
        let target_size = target_size.max(1);
        let desired_visible_size =
            (target_size as f32 * VISIBLE_CONTENT_PERCENT).round().max(1.0) as i32;
        self.candidates
            .iter()
            .filter(|candidate| {
                candidate.bounds.width().max(candidate.bounds.height()) >= desired_visible_size
            })
            .min_by_key(|candidate| candidate.bounds.width().max(candidate.bounds.height()))
            .or_else(|| {
                self.candidates
                    .iter()
                    .max_by_key(|candidate| candidate.bounds.width().max(candidate.bounds.height()))
            })
    }
}

impl Drop for AppIcon {
    fn drop(&mut self) {
        for candidate in self.candidates.drain(..) {
            unsafe { destroy_candidate(candidate) }
        }
    }
}

unsafe fn destroy_candidate(candidate: IconCandidate) {
    let _ = DestroyIcon(candidate.hicon);
    if let Some(bitmap) = candidate.bitmap {
        let _ = GdipDisposeImage(bitmap as *mut GpImage);
    }
}

pub fn get_app_icon(
    override_icons: &IndexMap<String, String>,
    module_path: &str,
    hwnd: HWND,
) -> AppIcon {
    let module_path_lc = module_path.to_lowercase();
    if let Some((_, v)) = override_icons
        .iter()
        .find(|(k, _)| module_path_lc.contains(&k.to_lowercase()))
    {
        let mut override_path = PathBuf::from(v);
        if !override_path.is_absolute() {
            if let Some(module_dir) = Path::new(module_path).parent() {
                override_path = module_dir.join(override_path);
            }
        }
        if let Some(icon) = load_image_as_hicon(override_path) {
            return icon;
        }
    }

    if let Some(icon) = get_pwa_icon_from_lnk(module_path) {
        return icon;
    }

    if let Some(icon) = get_browser_profile_icon(module_path) {
        return icon;
    }

    if module_path.starts_with("C:\\Program Files\\WindowsApps") {
        if let Some(icon) =
            get_appx_logo_paths(module_path).and_then(|paths| load_images_as_hicon(&paths))
        {
            return icon;
        }
    }

    let base_path = module_path.split("::").next().unwrap_or(module_path);
    get_exe_icon(base_path)
        .or_else(|| get_window_icon(hwnd).map(AppIcon::from_hicon))
        .unwrap_or_else(fallback_icon)
}

fn get_appx_logo_paths(module_path: &str) -> Option<Vec<PathBuf>> {
    let module_path = PathBuf::from(module_path);
    let executable = module_path.file_name()?.to_string_lossy();
    let module_dir = module_path.parent()?;
    let logo_value = read_appx_logo_value(module_dir, Some(&executable))?;
    resolve_appx_logo_paths(module_dir, &logo_value)
}

fn get_appx_logo_paths_from_dir(package_dir: &Path) -> Option<Vec<PathBuf>> {
    let logo_value = read_appx_logo_value(package_dir, None)?;
    resolve_appx_logo_paths(package_dir, &logo_value)
}

fn read_appx_logo_value(manifest_dir: &Path, executable: Option<&str>) -> Option<String> {
    let manifest_path = manifest_dir.join("AppxManifest.xml");
    let manifest_file = File::open(manifest_path).ok()?;
    let manifest_file = BufReader::new(manifest_file);
    let reader = EventReader::new(manifest_file);
    let mut logo_value = None;
    let mut matched = executable.is_none();
    let mut paths = vec![];
    let mut depth = 0;
    for e in reader {
        match e {
            Ok(XmlEvent::StartElement {
                name, attributes, ..
            }) => {
                if paths.len() == depth {
                    paths.push(name.local_name.clone())
                }
                let xpath = paths.join("/");
                if xpath == "Package/Applications/Application" {
                    if let Some(exe) = executable {
                        matched = attributes
                            .iter()
                            .any(|v| v.name.local_name == "Executable" && v.value == exe);
                    }
                } else if xpath == "Package/Applications/Application/VisualElements" && matched {
                    if let Some(value) = attributes
                        .iter()
                        .find(|v| {
                            ["Square44x44Logo", "Square30x30Logo", "SmallLogo"]
                                .contains(&v.name.local_name.as_str())
                        })
                        .map(|v| v.value.clone())
                    {
                        logo_value = Some(value);
                        break;
                    }
                }
                depth += 1;
            }
            Ok(XmlEvent::EndElement { .. }) => {
                if paths.len() == depth {
                    paths.pop();
                }
                depth -= 1;
            }
            Err(_) => break,
            _ => {}
        }
    }
    logo_value
}

fn resolve_appx_logo_paths(base_dir: &Path, logo_value: &str) -> Option<Vec<PathBuf>> {
    let logo_path = base_dir.join(logo_value);
    let extension = format!(".{}", logo_path.extension()?.to_string_lossy());
    let logo_path = logo_path.display().to_string();
    let prefix = &logo_path[0..(logo_path.len() - extension.len())];
    let paths = ["targetsize-256", "targetsize-128", "scale-200", "scale-100"]
        .iter()
        .filter_map(|size| {
            let logo_path = PathBuf::from(format!("{prefix}.{size}{extension}"));
            logo_path.exists().then_some(logo_path)
        })
        .collect::<Vec<_>>();
    (!paths.is_empty()).then_some(paths)
}

fn load_images_as_hicon(paths: &[PathBuf]) -> Option<AppIcon> {
    let mut result = AppIcon::new();
    for path in paths {
        let Some(mut icon) = load_image_as_hicon(path) else {
            continue;
        };
        result.candidates.append(&mut icon.candidates);
    }
    (!result.candidates.is_empty()).then_some(result)
}

fn ico_frame_dimensions(data: &[u8]) -> Option<Vec<(i32, i32)>> {
    let header = data.get(..6)?;
    let reserved = u16::from_le_bytes([header[0], header[1]]);
    let image_type = u16::from_le_bytes([header[2], header[3]]);
    let count = usize::from(u16::from_le_bytes([header[4], header[5]]));
    if reserved != 0 || image_type != 1 || count == 0 {
        return None;
    }

    let directory_len = count.checked_mul(16)?.checked_add(6)?;
    let directory = data.get(6..directory_len)?;
    let mut dimensions = Vec::with_capacity(count);
    for entry in directory.chunks_exact(16) {
        let width = if entry[0] == 0 {
            256
        } else {
            i32::from(entry[0])
        };
        let height = if entry[1] == 0 {
            256
        } else {
            i32::from(entry[1])
        };
        if !dimensions.contains(&(width, height)) {
            dimensions.push((width, height));
        }
    }
    Some(dimensions)
}

pub fn load_image_as_hicon<T: AsRef<Path>>(image_path: T) -> Option<AppIcon> {
    let image_path = image_path.as_ref();
    if !image_path.exists() {
        return None;
    }
    if image_path
        .extension()
        .and_then(|v| v.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ico"))
    {
        let dimensions = ico_frame_dimensions(&fs::read(image_path).ok()?)?;
        let icon_path = to_wstring(image_path.to_string_lossy().as_ref());
        let mut result = AppIcon::new();
        for (width, height) in dimensions {
            let icon = unsafe {
                LoadImageW(
                    None,
                    PCWSTR(icon_path.as_ptr()),
                    IMAGE_ICON,
                    width,
                    height,
                    LR_LOADFROMFILE | LR_DEFAULTCOLOR,
                )
            }
            .ok()
            .map(|v| HICON(v.0));
            if let Some(icon) = icon {
                result.push(icon);
            }
        }
        if result.candidates.len() > 1 {
            let mut unique = Vec::with_capacity(result.candidates.len());
            for candidate in result.candidates.drain(..) {
                if unique.iter().any(|existing: &IconCandidate| {
                    existing.width == candidate.width && existing.height == candidate.height
                }) {
                    unsafe { destroy_candidate(candidate) }
                } else {
                    unique.push(candidate);
                }
            }
            result.candidates = unique;
        }
        (!result.candidates.is_empty()).then_some(result)
    } else {
        load_raster_as_hicon(image_path)
    }
}

fn load_raster_as_hicon(image_path: &Path) -> Option<AppIcon> {
    let path = to_wstring(image_path.to_string_lossy().as_ref());
    unsafe {
        let mut bitmap_ptr: *mut GpBitmap = std::ptr::null_mut();
        if GdipCreateBitmapFromFile(PCWSTR(path.as_ptr()), &mut bitmap_ptr) != GdiPlusOk {
            return None;
        }

        sanitize_bitmap_alpha(bitmap_ptr);

        let mut hicon = HICON::default();
        let result = GdipCreateHICONFromBitmap(bitmap_ptr, &mut hicon);
        if result != GdiPlusOk || hicon.is_invalid() {
            GdipDisposeImage(bitmap_ptr as *mut GpImage);
            return None;
        }

        Some(AppIcon::from_bitmap(bitmap_ptr, hicon))
    }
}

fn fallback_icon() -> AppIcon {
    let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default();
    AppIcon::from_hicon(unsafe { CopyIcon(icon) }.unwrap_or_default())
}

const PIXEL_FORMAT_32BPP_ARGB: i32 = 0x26200a;
const PIXEL_FORMAT_32BPP_PARGB: i32 = 0x0e200b;

fn clear_fully_transparent_rgb(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        if pixel[3] == 0 {
            pixel[..3].fill(0);
        }
    }
}

fn premultiply_alpha(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
}

unsafe fn sanitize_bitmap_alpha(bitmap: *mut GpBitmap) {
    let image = bitmap as *mut GpImage;
    let mut width = 0;
    let mut height = 0;
    if GdipGetImageWidth(image, &mut width) != GdiPlusOk
        || GdipGetImageHeight(image, &mut height) != GdiPlusOk
        || width == 0
        || height == 0
    {
        return;
    }

    let rect = Rect {
        X: 0,
        Y: 0,
        Width: width as i32,
        Height: height as i32,
    };
    let mut data = BitmapData::default();
    let lock_mode = (ImageLockModeRead.0 | ImageLockModeWrite.0) as u32;
    if GdipBitmapLockBits(bitmap, &rect, lock_mode, PIXEL_FORMAT_32BPP_ARGB, &mut data) != GdiPlusOk
    {
        return;
    }

    if !data.Scan0.is_null() && data.Stride != 0 {
        for y in 0..height as isize {
            let row = data.Scan0.cast::<u8>().offset(y * data.Stride as isize);
            let pixels = std::slice::from_raw_parts_mut(row, width as usize * 4);
            clear_fully_transparent_rgb(pixels);
        }
    }

    let _ = GdipBitmapUnlockBits(bitmap, &mut data);
}

fn alpha_bounds(width: i32, height: i32, pixels: &[u8]) -> Option<PixelBounds> {
    if width <= 0 || height <= 0 || pixels.len() < (width * height * 4) as usize {
        return None;
    }

    let mut bounds = PixelBounds {
        canvas_width: width,
        canvas_height: height,
        min_x: width,
        min_y: height,
        max_x: -1,
        max_y: -1,
    };
    for y in 0..height {
        for x in 0..width {
            let alpha = pixels[((y * width + x) * 4 + 3) as usize];
            if alpha > ALPHA_THRESHOLD {
                bounds.min_x = bounds.min_x.min(x);
                bounds.min_y = bounds.min_y.min(y);
                bounds.max_x = bounds.max_x.max(x);
                bounds.max_y = bounds.max_y.max(y);
            }
        }
    }
    (bounds.max_x >= 0).then_some(bounds)
}

fn select_bounds(
    alpha: Option<PixelBounds>,
    mask: Option<PixelBounds>,
    width: i32,
    height: i32,
    pixels: &[u8],
) -> PixelBounds {
    let alpha_has_semi_transparent = pixels
        .chunks_exact(4)
        .any(|pixel| pixel[3] > ALPHA_THRESHOLD && pixel[3] < u8::MAX);
    alpha
        .filter(|bounds| !bounds.is_full() || alpha_has_semi_transparent)
        .or(mask)
        .or(alpha)
        .unwrap_or_else(|| PixelBounds::full(width, height))
}

fn get_icon_dimensions(hicon: HICON) -> Option<(i32, i32)> {
    unsafe {
        let mut info = std::mem::zeroed::<ICONINFO>();
        GetIconInfo(hicon, &mut info).ok()?;
        let _color = BitmapGuard(info.hbmColor);
        let _mask = BitmapGuard(info.hbmMask);
        let bitmap = if !info.hbmColor.is_invalid() {
            info.hbmColor
        } else {
            info.hbmMask
        };
        let mut dimensions = BITMAP::default();
        if GetObjectW(
            bitmap.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut dimensions as *mut _ as *mut _),
        ) == 0
        {
            return None;
        }
        let height = if !info.hbmColor.is_invalid() {
            dimensions.bmHeight
        } else {
            dimensions.bmHeight / 2
        };
        Some((dimensions.bmWidth, height))
    }
}

fn mask_bounds(width: i32, height: i32, visible: &[bool]) -> Option<PixelBounds> {
    if width <= 0 || height <= 0 || visible.len() != (width * height) as usize {
        return None;
    }
    let count = visible.iter().filter(|pixel| **pixel).count();
    if count == 0 {
        return None;
    }
    if count == visible.len() {
        return Some(PixelBounds::full(width, height));
    }

    let mut bounds = PixelBounds {
        canvas_width: width,
        canvas_height: height,
        min_x: width,
        min_y: height,
        max_x: -1,
        max_y: -1,
    };
    for y in 0..height {
        for x in 0..width {
            if visible[(y * width + x) as usize] {
                bounds.min_x = bounds.min_x.min(x);
                bounds.min_y = bounds.min_y.min(y);
                bounds.max_x = bounds.max_x.max(x);
                bounds.max_y = bounds.max_y.max(y);
            }
        }
    }
    (bounds.max_x >= 0).then_some(bounds)
}

fn monochrome_mask_visibility(width: i32, height: i32, pixels: &[u8]) -> Option<Vec<bool>> {
    if width <= 0 || height <= 0 {
        return None;
    }
    let pixel_count = (width * height) as usize;
    let half_len = pixel_count.checked_mul(4)?;
    if pixels.len() != half_len.checked_mul(2)? {
        return None;
    }
    let (and_mask, xor_mask) = pixels.split_at(half_len);
    Some(
        and_mask
            .chunks_exact(4)
            .zip(xor_mask.chunks_exact(4))
            .map(|(and_pixel, xor_pixel)| {
                let and_is_set = and_pixel[..3].iter().any(|channel| *channel != 0);
                let xor_is_set = xor_pixel[..3].iter().any(|channel| *channel != 0);
                !and_is_set || xor_is_set
            })
            .collect(),
    )
}

fn normalized_rect(bounds: PixelBounds, target_size: i32) -> (i32, i32, i32, i32) {
    let scale = normalized_scale(bounds, target_size);
    let width = (bounds.width() as f32 * scale).round() as i32;
    let height = (bounds.height() as f32 * scale).round() as i32;
    (
        (target_size - width) / 2,
        (target_size - height) / 2,
        width.max(1),
        height.max(1),
    )
}

fn normalized_scale(bounds: PixelBounds, target_size: i32) -> f32 {
    target_size.max(1) as f32 * VISIBLE_CONTENT_PERCENT
        / bounds.width().max(bounds.height()).max(1) as f32
}

pub fn get_window_icon(hwnd: HWND) -> Option<HICON> {
    let mut result: usize = 0;
    let ret = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETICON,
            WPARAM(ICON_BIG as _),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            250,
            Some(&mut result),
        )
    };
    if ret.0 != 0 && result != 0 {
        return unsafe { CopyIcon(HICON(result as _)) }.ok();
    }
    #[cfg(target_arch = "x86")]
    let ret = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassLongW(hwnd, GCLP_HICON) };
    #[cfg(not(target_arch = "x86"))]
    let ret =
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetClassLongPtrW(hwnd, GCLP_HICON) };
    if ret != 0 {
        return unsafe { CopyIcon(HICON(ret as _)) }.ok();
    }
    let ret = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETICON,
            WPARAM(ICON_SMALL2 as _),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            250,
            Some(&mut result),
        )
    };
    if ret.0 != 0 && result != 0 {
        return unsafe { CopyIcon(HICON(result as _)) }.ok();
    }
    None
}

fn get_browser_profile_icon(module_path: &str) -> Option<AppIcon> {
    let parts: Vec<&str> = module_path.split("::").collect();
    if parts.len() != 2 {
        return None;
    }
    let exe_path = parts[0];
    let profile = parts[1];

    let local_app_data = std::env::var("LOCALAPPDATA").ok()?;
    let (user_data_dir, icon_file) = if exe_path.to_lowercase().contains("chrome.exe") {
        (
            PathBuf::from(&local_app_data).join(r"Google\Chrome\User Data"),
            "Google Profile.ico",
        )
    } else if exe_path.to_lowercase().contains("msedge.exe") {
        (
            PathBuf::from(&local_app_data).join(r"Microsoft\Edge\User Data"),
            "Edge Profile.ico",
        )
    } else {
        return None;
    };

    let profile_dir = super::window::pwa_map_profile_dir(profile);
    let icon_path = user_data_dir.join(&profile_dir).join(icon_file);
    load_image_as_hicon(&icon_path)
}

fn get_pwa_icon_from_lnk(module_path: &str) -> Option<AppIcon> {
    let parts: Vec<&str> = module_path.split("::").collect();
    if parts.len() != 3 {
        return None;
    }
    let exe_path = parts[0];
    let typ = parts[1];
    let app_id = parts[2];

    if typ == "appx" {
        let package_dir = super::window::find_appx_pkg_dir(app_id)?;
        let logo_paths = get_appx_logo_paths_from_dir(&PathBuf::from(package_dir))?;
        load_images_as_hicon(&logo_paths)
    } else {
        let user_data_dir = super::window::get_default_user_data_dir(exe_path)?;
        let lnk_path = super::window::pwa_find_lnk_path(&user_data_dir, typ, app_id)?;
        get_exe_icon(&lnk_path.to_string_lossy())
    }
}

const SHIL_JUMBO: i32 = 0x04;
const SHIL_EXTRALARGE: i32 = 0x02;
const SHIL_LARGE: i32 = 0x00;

fn get_exe_icon(module_path: &str) -> Option<AppIcon> {
    let info = get_shfileinfo(module_path)?;
    let mut result = AppIcon::new();
    for shil in [SHIL_JUMBO, SHIL_EXTRALARGE, SHIL_LARGE] {
        unsafe {
            let Some(list) = SHGetImageList::<IImageList>(shil).ok() else {
                continue;
            };
            let Some(hicon) = list.GetIcon(info.iIcon, 1u32).ok() else {
                continue;
            };
            match is_valid_icon(hicon) {
                Some(true) => result.push(hicon),
                _ => {
                    let _ = DestroyIcon(hicon);
                }
            }
        }
    }
    (!result.candidates.is_empty()).then_some(result)
}

fn get_shfileinfo(module_path: &str) -> Option<SHFILEINFOW> {
    unsafe {
        let mut p_path: Vec<u16> = module_path
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut file_info = SHFILEINFOW::default();
        // Retry up to 3 times because SHGetFileInfoW can transiently fail
        // (e.g. shell not fully initialized, file system contention). A simple
        // short sleep + retry handles these spurious failures robustly.
        for _ in 0..3 {
            let fff: usize = SHGetFileInfoW(
                PCWSTR::from_raw(p_path.as_mut_ptr()),
                FILE_ATTRIBUTE_NORMAL,
                Some(&mut file_info),
                mem::size_of_val(&file_info) as u32,
                SHGFI_SYSICONINDEX,
            );
            if fff != 0 {
                return Some(file_info);
            } else {
                let millis = time::Duration::from_millis(30);
                std::thread::sleep(millis);
            }
        }
        None
    }
}

/// Returns `false` for icons whose content is squeezed into the top-left corner
/// while the rest is stretched/padded garbage — e.g. the icon of `hh.exe`
/// (Windows's help viewer, reused by AutoHotKey, etc. to display their *.chm help).
/// These look ugly in the switcher and are better replaced with a fallback icon.
fn is_valid_icon(hicon: HICON) -> Option<bool> {
    let bounds = get_icon_bounds(hicon)?;
    Some(!is_topleft_icon(&bounds))
}

struct IconBounds {
    pub canvas_width: i32,
    pub canvas_height: i32,
    pub min_x: i32,
    pub min_y: i32,
    pub max_x: i32,
    pub max_y: i32,
}

fn is_topleft_icon(bounds: &IconBounds) -> bool {
    let bbox_width = bounds.max_x - bounds.min_x + 1;
    let bbox_height = bounds.max_y - bounds.min_y + 1;

    let bbox_area = bbox_width * bbox_height;
    let canvas_area = bounds.canvas_width * bounds.canvas_height;

    let bbox_ratio = bbox_area as f32 / canvas_area as f32;

    let center_x = (bounds.min_x + bounds.max_x) as f32 / 2.0 / bounds.canvas_width as f32;
    let center_y = (bounds.min_y + bounds.max_y) as f32 / 2.0 / bounds.canvas_height as f32;

    let small_content = bbox_ratio < 0.25;
    let top_left = center_x < 0.30 && center_y < 0.30;

    small_content && top_left
}

struct HdcGuard(HDC);
impl Drop for HdcGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.0);
        }
    }
}

struct ScreenDcGuard(HDC);
impl Drop for ScreenDcGuard {
    fn drop(&mut self) {
        unsafe {
            ReleaseDC(None, self.0);
        }
    }
}

struct BitmapGuard(HBITMAP);
impl Drop for BitmapGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.0 .0 as _));
        }
    }
}

fn get_icon_bounds(hicon: HICON) -> Option<IconBounds> {
    unsafe {
        let mut icon_info: ICONINFO = std::mem::zeroed();
        if GetIconInfo(hicon, &mut icon_info).is_err() {
            return None;
        }
        let _color_guard = BitmapGuard(icon_info.hbmColor);
        let _mask_guard = BitmapGuard(icon_info.hbmMask);

        let mut bmp = BITMAP::default();
        let color_bitmap = !icon_info.hbmColor.is_invalid();
        let source_bitmap = if color_bitmap {
            icon_info.hbmColor
        } else {
            icon_info.hbmMask
        };
        if GetObjectW(
            source_bitmap.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut _),
        ) == 0
        {
            return None;
        }

        let width = bmp.bmWidth;
        let height = if color_bitmap {
            bmp.bmHeight
        } else {
            bmp.bmHeight / 2
        };
        if width <= 0 || height <= 0 {
            return None;
        }

        let screen_dc = GetDC(None);
        let _screen_guard = ScreenDcGuard(screen_dc);

        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let _dc_guard = HdcGuard(mem_dc);

        if !color_bitmap {
            let mask_height = height * 2;
            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -mask_height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0,
                    ..Default::default()
                },
                bmiColors: [RGBQUAD::default()],
            };
            let mut pixels = vec![0; (width * mask_height * 4) as usize];
            if GetDIBits(
                mem_dc,
                source_bitmap,
                0,
                mask_height as u32,
                Some(pixels.as_mut_ptr() as *mut _),
                &mut bmi,
                DIB_RGB_COLORS,
            ) == 0
            {
                return None;
            }
            let visible = monochrome_mask_visibility(width, height, &pixels)?;
            let bounds = mask_bounds(width, height, &visible)?;
            return Some(IconBounds {
                canvas_width: width,
                canvas_height: height,
                min_x: bounds.min_x,
                min_y: bounds.min_y,
                max_x: bounds.max_x,
                max_y: bounds.max_y,
            });
        }

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD {
                rgbBlue: 0,
                rgbGreen: 0,
                rgbRed: 0,
                rgbReserved: 0,
            }; 1],
        };

        let buf_size = (width * height * 4) as usize;
        let mut pixels: Vec<u8> = vec![0; buf_size];

        if 0 == GetDIBits(
            mem_dc,
            source_bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr() as *mut _),
            &mut bmi,
            DIB_RGB_COLORS,
        ) {
            return None;
        }

        let alpha_bounds = alpha_bounds(width, height, &pixels);
        let has_semi_transparent = pixels.chunks_exact(4).any(|pixel| {
            let alpha = pixel[3];
            alpha > ALPHA_THRESHOLD && alpha < 255
        });
        let (mut min_x, mut min_y, mut max_x, mut max_y) = alpha_bounds
            .map(|bounds| (bounds.min_x, bounds.min_y, bounds.max_x, bounds.max_y))
            .unwrap_or((width, height, -1, -1));

        // Use the legacy mask when alpha is unavailable or covers the whole
        // canvas. Keep the alpha bounds if the mask is empty or contradictory.
        if !has_semi_transparent
            && (max_x < 0
                || (min_x == 0 && min_y == 0 && max_x == width - 1 && max_y == height - 1))
        {
            if 0 != GetDIBits(
                mem_dc,
                icon_info.hbmMask,
                0,
                height as u32,
                Some(pixels.as_mut_ptr() as *mut _),
                &mut bmi,
                DIB_RGB_COLORS,
            ) {
                let visible = pixels
                    .chunks_exact(4)
                    .map(|c| c[0] == 0 && c[1] == 0 && c[2] == 0)
                    .collect::<Vec<_>>();
                if let Some(bounds) = mask_bounds(width, height, &visible) {
                    min_x = bounds.min_x;
                    min_y = bounds.min_y;
                    max_x = bounds.max_x;
                    max_y = bounds.max_y;
                }
            }
        }

        if max_x < 0 {
            return None;
        }

        Some(IconBounds {
            canvas_width: width,
            canvas_height: height,
            min_x,
            min_y,
            max_x,
            max_y,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        alpha_bounds, get_icon_bounds, ico_frame_dimensions, mask_bounds, normalized_rect,
        premultiply_alpha, select_bounds, AppIcon, PixelBounds, HICON,
    };

    #[test]
    fn alpha_bounds_ignores_nearly_transparent_pixels() {
        let pixels = [0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255];

        assert_eq!(
            alpha_bounds(2, 2, &pixels),
            Some(PixelBounds {
                canvas_width: 2,
                canvas_height: 2,
                min_x: 1,
                min_y: 1,
                max_x: 1,
                max_y: 1,
            })
        );
    }

    #[test]
    fn normalized_rect_preserves_aspect_ratio_and_uses_seventy_percent() {
        let bounds = PixelBounds {
            canvas_width: 100,
            canvas_height: 50,
            min_x: 10,
            min_y: 5,
            max_x: 89,
            max_y: 44,
        };

        assert_eq!(normalized_rect(bounds, 100), (15, 32, 70, 35));
    }

    #[test]
    fn empty_mask_falls_back_to_the_complete_canvas() {
        let all_black = [true; 16];
        let all_white = [false; 16];

        assert_eq!(
            mask_bounds(4, 4, &all_black),
            Some(PixelBounds::full(4, 4))
        );
        assert_eq!(mask_bounds(4, 4, &all_white), None);
        assert_eq!(
            select_bounds(None, None, 4, 4, &[0; 64]),
            PixelBounds::full(4, 4)
        );
    }

    #[test]
    fn valid_mask_bounds_are_returned() {
        let mut visible = [false; 16];
        visible[5] = true;
        visible[6] = true;
        visible[9] = true;

        assert_eq!(
            mask_bounds(4, 4, &visible),
            Some(PixelBounds {
                canvas_width: 4,
                canvas_height: 4,
                min_x: 1,
                min_y: 1,
                max_x: 2,
                max_y: 2,
            })
        );
    }

    #[test]
    fn canonical_pixels_are_premultiplied() {
        let mut pixels = [0x10, 0x20, 0x30, 0, 0x40, 0x80, 0xff, 128];

        premultiply_alpha(&mut pixels);

        assert_eq!(pixels, [0, 0, 0, 0, 0x20, 0x40, 0x80, 128]);
    }

    #[test]
    fn candidate_selection_avoids_upscaling_at_high_dpi() {
        let mut icon = AppIcon::new();
        for size in [32, 48, 256] {
            icon.push_candidate(HICON::default(), None, size, size, None);
        }

        assert_eq!(icon.selected(64).map(|candidate| candidate.width), Some(48));
        assert_eq!(icon.selected(128).map(|candidate| candidate.width), Some(256));
    }

    #[test]
    fn ico_dimensions_contain_only_native_unique_frames() {
        let mut data = vec![0, 0, 1, 0, 3, 0];
        for (width, height) in [(0, 0), (48, 48), (48, 48)] {
            data.extend_from_slice(&[
                width, height, 0, 0, 1, 0, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ]);
        }

        assert_eq!(ico_frame_dimensions(&data), Some(vec![(256, 256), (48, 48)]));
        assert_eq!(ico_frame_dimensions(&data[..data.len() - 1]), None);
    }

    #[test]
    fn hicon_candidates_keep_the_native_renderer() {
        use windows::Win32::Graphics::GdiPlus::{
            GdiplusShutdown, GdiplusStartup, GdiplusStartupInput,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            CopyIcon, LoadIconW, IDI_APPLICATION,
        };

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
            {
                let shared = LoadIconW(None, IDI_APPLICATION).unwrap();
                let owned = CopyIcon(shared).unwrap();
                let icon = AppIcon::from_hicon(owned);

                assert!(icon.candidates[0].bitmap.is_none());
            }
            GdiplusShutdown(token);
        }
    }

    #[test]
    fn monochrome_hicon_bounds_include_inverted_pixels() {
        use windows::Win32::UI::WindowsAndMessaging::{CreateIcon, DestroyIcon};

        let and_mask = [0xc0, 0x00];
        let xor_mask = [0x40, 0x00];
        let icon = unsafe {
            CreateIcon(
                None,
                2,
                1,
                1,
                1,
                and_mask.as_ptr(),
                xor_mask.as_ptr(),
            )
        }
        .unwrap();

        let bounds = get_icon_bounds(icon).unwrap();

        assert_eq!((bounds.min_x, bounds.max_x), (1, 1));
        let _ = unsafe { DestroyIcon(icon) };
    }

    #[test]
    fn visible_bounds_prefer_alpha_then_mask() {
        let alpha = PixelBounds {
            canvas_width: 4,
            canvas_height: 4,
            min_x: 1,
            min_y: 1,
            max_x: 2,
            max_y: 2,
        };
        let mask = PixelBounds::full(4, 4);
        let mut pixels = [0; 64];
        pixels[3] = 255;

        assert_eq!(select_bounds(Some(alpha), Some(mask), 4, 4, &pixels), alpha);
        assert_eq!(select_bounds(Some(mask), Some(alpha), 4, 4, &pixels), alpha);
    }
}
