//! macOS screen capture through ScreenCaptureKit, plus display and window geometry.
//!
//! Why ScreenCaptureKit instead of the older `CGWindowListCreateImage`: Apple
//! deprecated the old API and macOS Sequoia keeps nagging the user to re-approve
//! apps that still use it.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_core_foundation::{CFData, CFRetained};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorSpace, CGDataProvider, CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode,
    CGDisplayIsMain, CGDisplayMode, CGError, CGGetActiveDisplayList, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess,
    CGWindowListCopyWindowInfo, CGWindowListOption, CGColorRenderingIntent, kCGColorSpaceSRGB, kCGNullWindowID,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString};
use objc2_screen_capture_kit::{
    SCCaptureResolutionType, SCContentFilter, SCDisplay, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
    SCWindow,
};

use crate::clog;
use crate::geo::{cg_rect, MonitorInfo, Rect, WinRect};

/// `kCVPixelFormatType_32BGRA` ('BGRA'): the layout ScreenCaptureKit hands us.
const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// A `CGImage` that may cross threads. CGImage is immutable, so this is safe;
/// the type system just can't tell.
pub struct SendImage(*mut CGImage);

// SAFETY: CGImage is immutable and safe to read from any thread.
unsafe impl Send for SendImage {}
// SAFETY: as above; `get` only retains and never mutates.
unsafe impl Sync for SendImage {}

impl SendImage {
    pub fn new(image: CFRetained<CGImage>) -> Self {
        Self(CFRetained::into_raw(image).as_ptr())
    }

    /// A +1 reference to the captured image, independent of `self`.
    ///
    /// Must retain: `from_raw` would take over `self`'s own reference, so every
    /// call would release the image once too often.
    pub fn get(&self) -> CFRetained<CGImage> {
        let ptr = std::ptr::NonNull::new(self.0).expect("owned CGImage reference");
        // SAFETY: `self.0` is kept alive by `self` until drop, and CFRetain
        // returns a new +1 reference.
        unsafe { CFRetained::retain(ptr) }
    }
}

impl Clone for SendImage {
    fn clone(&self) -> Self {
        Self::new(self.get())
    }
}

impl Drop for SendImage {
    fn drop(&mut self) {
        let ptr = std::ptr::NonNull::new(self.0).expect("owned CGImage reference");
        // SAFETY: releases the +1 reference created in `new`.
        drop(unsafe { CFRetained::from_raw(ptr) });
    }
}

/// Rectangle in physical pixels, inside a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// One frozen display: the pixels, plus what was on top of them.
#[derive(Clone)]
pub struct Frame {
    pub monitor: MonitorInfo,
    /// Physical pixel size; `rgba` is top-down with a stride of `width * 4`.
    pub width: usize,
    pub height: usize,
    pub rgba: Arc<Vec<u8>>,
    /// On-screen windows in z-order (front first), local to this monitor.
    pub windows: Vec<WinRect>,
    image: SendImage,
}

impl Frame {
    /// A frame from pixels we already have (tests, previews).
    pub fn from_rgba(monitor: MonitorInfo, width: usize, height: usize, rgba: Vec<u8>) -> Self {
        let image = rgba_to_image(&rgba, width as u32, height as u32).expect("valid RGBA buffer");
        Self {
            monitor,
            width,
            height,
            rgba: Arc::new(rgba),
            windows: Vec::new(),
            image: SendImage::new(image),
        }
    }

    pub fn scale(&self) -> f64 {
        self.width as f64 / self.monitor.rect.w.max(1.0)
    }

    /// Convert a monitor-local logical rect into a clamped physical-pixel rect.
    pub fn to_pixels(&self, sel: &Rect) -> Option<IRect> {
        let s = self.scale();
        let (w, h) = (self.width as f64, self.height as f64);
        let x0 = (sel.x * s).round().clamp(0.0, w);
        let y0 = (sel.y * s).round().clamp(0.0, h);
        let x1 = (sel.right() * s).round().clamp(0.0, w);
        let y1 = (sel.bottom() * s).round().clamp(0.0, h);
        let r = IRect { x: x0 as i32, y: y0 as i32, w: (x1 - x0) as i32, h: (y1 - y0) as i32 };
        (r.w >= 1 && r.h >= 1).then_some(r)
    }

    pub fn crop_rgba(&self, r: &IRect) -> Vec<u8> {
        let mut out = vec![0u8; (r.w * r.h * 4) as usize];
        let stride = self.width * 4;
        for row in 0..r.h as usize {
            let src = (r.y as usize + row) * stride + r.x as usize * 4;
            let dst = row * r.w as usize * 4;
            out[dst..dst + r.w as usize * 4].copy_from_slice(&self.rgba[src..src + r.w as usize * 4]);
        }
        out
    }

    /// Crop straight from the captured image (no re-encoding, color space kept).
    pub fn crop_image(&self, r: &IRect) -> Option<CFRetained<CGImage>> {
        let rect = cg_rect(r.x as f64, r.y as f64, r.w as f64, r.h as f64);
        CGImage::with_image_in_rect(Some(&self.image.get()), rect)
    }

    /// The captured image itself, for callers that crop elsewhere.
    pub fn cg_image(&self) -> CFRetained<CGImage> {
        self.image.get()
    }
}

pub fn screen_capture_allowed() -> bool {
    CGPreflightScreenCaptureAccess()
}

pub fn request_screen_capture_access() {
    CGRequestScreenCaptureAccess();
}

/// Active displays in the order CoreGraphics lists them (the main one first).
pub fn active_displays() -> Vec<MonitorInfo> {
    let mut ids = [0u32; 32];
    let mut count = 0u32;
    // SAFETY: the array and count pointer are valid for this call.
    if unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) } != CGError(0) {
        clog!("CGGetActiveDisplayList failed");
        return Vec::new();
    }
    ids[..(count as usize).min(ids.len())].iter().map(|&id| monitor_info(id)).collect()
}

fn monitor_info(id: CGDirectDisplayID) -> MonitorInfo {
    let bounds = CGDisplayBounds(id);
    let mode = CGDisplayCopyDisplayMode(id);
    // Pixels per point from the display mode. On HiDPI modes the pixel size is
    // the real framebuffer, so the ratio is the backing scale factor.
    let (pw, ph, scale) = mode.as_deref().map_or((0, 0, 1.0), |m| {
        let (pw, ph) = (CGDisplayMode::pixel_width(Some(m)), CGDisplayMode::pixel_height(Some(m)));
        let w = CGDisplayMode::width(Some(m));
        (pw as u32, ph as u32, if w > 0 { pw as f64 / w as f64 } else { 1.0 })
    });
    MonitorInfo {
        id,
        rect: Rect::from_cg(bounds),
        scale: if scale > 0.0 { scale } else { 1.0 },
        pixel_width: pw,
        pixel_height: ph,
        is_main: CGDisplayIsMain(id),
    }
}

fn dict_number(dict: &NSDictionary, key: &str) -> Option<f64> {
    let value = dict.objectForKey(&NSString::from_str(key))?;
    value.downcast::<NSNumber>().ok().map(|n| n.doubleValue())
}

/// On-screen windows in z-order (front first), global logical coordinates.
///
/// Bounds need no Screen Recording permission; only window titles do, and we
/// never read those.
fn window_list(own_pid: i32) -> Vec<WinRect> {
    let options = CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
    let Some(list) = CGWindowListCopyWindowInfo(options, kCGNullWindowID) else {
        clog!("CGWindowListCopyWindowInfo returned nothing");
        return Vec::new();
    };
    // SAFETY: CFArray and NSArray are toll-free bridged. `list` is a +1
    // reference from the Copy function; retain it again so `windows` owns a
    // balanced reference instead of stealing `list`'s (double-release).
    let windows: Retained<NSArray> = unsafe {
        let ptr = CFRetained::as_ptr(&list).cast::<NSArray>().as_ptr();
        Retained::retain(ptr)
    }
    .expect("non-null list");

    let mut out = Vec::new();
    for i in 0..windows.count() {
        let Ok(dict) = windows.objectAtIndex(i).downcast::<NSDictionary>() else { continue };
        let layer = dict_number(&dict, "kCGWindowLayer").unwrap_or(-1.0) as i32;
        let pid = dict_number(&dict, "kCGWindowOwnerPID").unwrap_or(-1.0) as i32;
        // Negative layers are the desktop; our own windows are the overlay itself.
        if layer < 0 || pid == own_pid {
            continue;
        }
        let Some(bounds) = dict.objectForKey(&NSString::from_str("kCGWindowBounds")) else { continue };
        let Ok(bounds) = bounds.downcast::<NSDictionary>() else { continue };
        let (Some(x), Some(y), Some(w), Some(h)) = (
            dict_number(&bounds, "X"),
            dict_number(&bounds, "Y"),
            dict_number(&bounds, "Width"),
            dict_number(&bounds, "Height"),
        ) else {
            continue;
        };
        if w < 2.0 || h < 2.0 {
            continue;
        }
        out.push(WinRect {
            id: dict_number(&dict, "kCGWindowNumber").unwrap_or(0.0) as u32,
            layer,
            pid,
            rect: Rect::new(x, y, w, h),
        });
    }
    out
}

/// Human text for an NSError pointer that may be null.
///
/// # Safety
/// `error` must be null or a live NSError.
unsafe fn error_text(error: *mut NSError) -> String {
    if error.is_null() {
        return "未知错误".to_string();
    }
    // SAFETY: caller guarantees `error` is a live NSError.
    let e = unsafe { &*error };
    format!("{}（代码 {}）", e.localizedDescription().to_string(), e.code())
}

fn wait_ptr(name: &str, rx: mpsc::Receiver<Result<usize, String>>) -> Result<usize, String> {
    rx.recv_timeout(CAPTURE_TIMEOUT).map_err(|_| format!("{name}超时"))?
}

fn shareable_content() -> Result<Retained<SCShareableContent>, String> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        let result = if content.is_null() {
            // SAFETY: the only pointer read is `error`, valid during the callback.
            Err(unsafe { error_text(error) })
        } else {
            // SAFETY: retain before this thread's autorelease pool drains.
            let retained = unsafe { Retained::retain(content) }.expect("live object");
            Ok(Retained::into_raw(retained) as usize)
        };
        let _ = tx.send(result);
    });
    // SAFETY: the block is alive for the whole call; the handler only touches
    // its arguments.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false,
            false,
            &block,
        )
    }
    let ptr = wait_ptr("获取屏幕内容", rx)?;
    // SAFETY: the pointer is the +1 reference retained in the handler.
    Ok(unsafe { Retained::from_raw(ptr as *mut SCShareableContent) }.expect("non-null content"))
}

fn capture_display(display: &SCDisplay, monitor: &MonitorInfo) -> Result<CFRetained<CGImage>, String> {
    // SAFETY: `alloc` is paired with `init`; both objects outlive the call.
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), display, &NSArray::new())
    };
    // SAFETY: `new` is a plain Objective-C class method on a non-main-thread-only class.
    let config = unsafe { SCStreamConfiguration::new() };
    // Width/height are the output size in pixels. Asking for the native
    // framebuffer keeps Retina sharpness (`scalesToFit = false` never upscales).
    // SAFETY: plain setters on a freshly created object; the static is a live constant.
    unsafe {
        config.setWidth(monitor.pixel_width.max(1) as usize);
        config.setHeight(monitor.pixel_height.max(1) as usize);
        configure_capture(&config);
    }
    shot(&filter, &config)
}

/// The configuration shared by every capture: cursor-free, sRGB, opaque, best
/// resolution. Width/height (and the region) are set by the caller.
fn configure_capture(config: &SCStreamConfiguration) {
    // SAFETY: plain setters on a live `SCStreamConfiguration`.
    unsafe {
        config.setShowsCursor(false);
        config.setPixelFormat(PIXEL_FORMAT_BGRA);
        // Convert from the display's color space (often Display P3) to sRGB so
        // the saved PNG looks the same everywhere.
        config.setColorSpaceName(kCGColorSpaceSRGB);
        config.setCaptureResolution(SCCaptureResolutionType::Best);
        config.setScalesToFit(false);
        config.setShouldBeOpaque(true);
    }
}

/// One screenshot through `SCScreenshotManager`, bridged to a synchronous call.
fn shot(filter: &SCContentFilter, config: &SCStreamConfiguration) -> Result<CFRetained<CGImage>, String> {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        let result = if image.is_null() {
            // SAFETY: the only pointer read is `error`, valid during the callback.
            Err(unsafe { error_text(error) })
        } else {
            let non_null = std::ptr::NonNull::new(image).expect("non-null image");
            // SAFETY: retain before this thread's autorelease pool drains; the
            // callback hands us an autoreleased reference, not a +1.
            let retained = unsafe { CFRetained::retain(non_null) };
            Ok(CFRetained::into_raw(retained).as_ptr() as usize)
        };
        let _ = tx.send(result);
    });
    // SAFETY: as in `shareable_content`.
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(filter, config, Some(&block))
    }
    let ptr = wait_ptr("截取屏幕", rx)?;
    let non_null = std::ptr::NonNull::new(ptr as *mut CGImage).expect("non-null image");
    // SAFETY: the pointer is the +1 reference retained in the handler.
    Ok(unsafe { CFRetained::from_raw(non_null) })
}

/// A per-session capture setup for the scroll capture: everything
/// ScreenCaptureKit needs (shareable content, filter, configuration) is built
/// once and each tick only runs the shot. On a 300 ms cadence the old
/// one-shot path re-enumerated all shareable content every tick — the most
/// expensive call in the chain — and rebuilt filter and config around it.
///
/// Holds Objective-C objects, so it is not `Send`: it must live on the thread
/// that captures (the scroll loop's own thread, which is exactly the point).
pub struct RegionCapturer {
    region: IRect,
    /// `true` = capture the whole display and crop, the geometry-correct
    /// fallback for when `sourceRect` disagrees with reality.
    full: bool,
    filter: Retained<SCContentFilter>,
    config: Retained<SCStreamConfiguration>,
}

impl RegionCapturer {
    /// Find the display and build the (optionally window-excluding) filter.
    fn build(monitor: &MonitorInfo, exclude: &[u32]) -> Result<(Retained<SCDisplay>, Retained<SCContentFilter>), String> {
        let content = shareable_content()?;
        // SAFETY: plain getters on the content returned by ScreenCaptureKit.
        let displays = unsafe { content.displays() };
        let display = unsafe {
            (0..displays.count()).map(|i| displays.objectAtIndex(i)).find(|d| d.displayID() == monitor.id)
        }
        .ok_or_else(|| format!("显示器 {} 不可截取", monitor.id))?;
        // SAFETY: plain getter on `content`.
        let windows = unsafe { content.windows() };
        let excluded: Retained<NSArray<SCWindow>> = if exclude.is_empty() {
            NSArray::new()
        } else {
            let mut picked: Vec<Retained<SCWindow>> = Vec::new();
            // SAFETY: plain getters on `content`.
            for i in 0..windows.count() {
                // SAFETY: plain getters on a window from `content`.
                let window = unsafe {
                    let window = windows.objectAtIndex(i);
                    if exclude.contains(&window.windowID()) {
                        Some(window)
                    } else {
                        None
                    }
                };
                if let Some(window) = window {
                    picked.push(window);
                }
            }
            let refs: Vec<&SCWindow> = picked.iter().map(|w| &**w).collect();
            NSArray::from_slice(&refs)
        };
        // SAFETY: `alloc` is paired with `init`; both objects outlive the call.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &excluded)
        };
        // The filter keeps the display alive; the returned handle is for the
        // full-display variant's config (and keeps a +1 for the caller).
        Ok((display, filter))
    }

    /// Region capture through `sourceRect`, with our own on-screen windows
    /// (`exclude`, the scroll outline and control panel) left out.
    pub fn new_region(monitor: &MonitorInfo, region: &IRect, exclude: &[u32]) -> Result<Self, String> {
        let (_display, filter) = Self::build(monitor, exclude)?;
        // SAFETY: `new` is a plain Objective-C class method.
        let config = unsafe { SCStreamConfiguration::new() };
        let scale = monitor.scale.max(1.0);
        // SAFETY: plain setters on a freshly created object.
        unsafe {
            // `sourceRect` is in points, relative to the display's origin;
            // the output size is the region's pixel size, so no resampling.
            config.setSourceRect(cg_rect(
                region.x as f64 / scale,
                region.y as f64 / scale,
                region.w as f64 / scale,
                region.h as f64 / scale,
            ));
            config.setWidth(region.w.max(1) as usize);
            config.setHeight(region.h.max(1) as usize);
            configure_capture(&config);
        }
        Ok(Self { region: *region, full: false, filter, config })
    }

    /// Whole-display capture cropped to `region`. Slower per tick, but its
    /// geometry matches the frozen frames exactly.
    pub fn new_full(monitor: &MonitorInfo, region: &IRect) -> Result<Self, String> {
        let (_display, filter) = Self::build(monitor, &[])?;
        // SAFETY: `new` is a plain Objective-C class method.
        let config = unsafe { SCStreamConfiguration::new() };
        // SAFETY: plain setters on a freshly created object.
        unsafe {
            config.setWidth(monitor.pixel_width.max(1) as usize);
            config.setHeight(monitor.pixel_height.max(1) as usize);
            configure_capture(&config);
        }
        Ok(Self { region: *region, full: true, filter, config })
    }

    /// One shot, cropped to the region.
    ///
    /// Must run off the main thread: it blocks while ScreenCaptureKit calls.
    pub fn capture(&self) -> Result<(usize, usize, Vec<u8>), String> {
        objc2::rc::autoreleasepool(|_| {
            let image = shot(&self.filter, &self.config)?;
            let (width, height, rgba) = image_to_rgba(&image)?;
            let region = &self.region;
            if self.full {
                if (region.x + region.w) as usize > width || (region.y + region.h) as usize > height {
                    return Err(format!(
                        "区域 {}x{}+{}+{} 超出显示器 {}x{}",
                        region.w, region.h, region.x, region.y, width, height
                    ));
                }
                let mut out = vec![0u8; (region.w * region.h * 4) as usize];
                for row in 0..region.h as usize {
                    let src = (region.y as usize + row) * width * 4 + region.x as usize * 4;
                    let dst = row * region.w as usize * 4;
                    out[dst..dst + region.w as usize * 4].copy_from_slice(&rgba[src..src + region.w as usize * 4]);
                }
                Ok((region.w as usize, region.h as usize, out))
            } else {
                if (width, height) != (region.w as usize, region.h as usize) {
                    // ScreenCaptureKit may adjust the rect; resample is worse
                    // than a skipped tick, so surface it and let the caller
                    // drop the frame.
                    return Err(format!("区域截图返回 {width}x{height}，期望 {}x{}", region.w, region.h));
                }
                Ok((width, height, rgba))
            }
        })
    }
}

/// Capture the whole display and crop `region` (display-local physical
/// pixels) out of it — a one-shot helper for the scroll session's sourceRect
/// sanity check. The steady-state loop uses [`RegionCapturer`].
///
/// Must run off the main thread: it blocks while ScreenCaptureKit calls back.
pub fn capture_full_region_rgba(monitor: &MonitorInfo, region: &IRect) -> Result<(usize, usize, Vec<u8>), String> {
    objc2::rc::autoreleasepool(|_| {
        let (display, _filter) = RegionCapturer::build(monitor, &[])?;
        let image = capture_display(&display, monitor)?;
        let (width, height, rgba) = image_to_rgba(&image)?;
        if (region.x + region.w) as usize > width || (region.y + region.h) as usize > height {
            return Err(format!(
                "区域 {}x{}+{}+{} 超出显示器 {}x{}",
                region.w, region.h, region.x, region.y, width, height
            ));
        }
        let mut out = vec![0u8; (region.w * region.h * 4) as usize];
        for row in 0..region.h as usize {
            let src = (region.y as usize + row) * width * 4 + region.x as usize * 4;
            let dst = row * region.w as usize * 4;
            out[dst..dst + region.w as usize * 4].copy_from_slice(&rgba[src..src + region.w as usize * 4]);
        }
        Ok((region.w as usize, region.h as usize, out))
    })
}

/// Copy a 32-bit CGImage into a plain RGBA8 buffer.
fn image_to_rgba(image: &CGImage) -> Result<(usize, usize, Vec<u8>), String> {
    let (w, h) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    let bpp = CGImage::bits_per_pixel(Some(image));
    if w == 0 || h == 0 || bpp != 32 {
        return Err(format!("不支持的截图格式：{bpp} 位，{w}x{h}"));
    }
    let bpr = CGImage::bytes_per_row(Some(image));
    let provider = CGImage::data_provider(Some(image)).ok_or("截图没有像素数据")?;
    let data = CGDataProvider::data(Some(&provider)).ok_or("读取截图像素失败")?;
    let len = data.len();
    if len < bpr.saturating_mul(h - 1).saturating_add(w * 4) {
        return Err("截图像素数据不完整".to_string());
    }
    // SAFETY: `byte_ptr` is valid for `len` bytes; checked above to cover every row.
    let src = unsafe { std::slice::from_raw_parts(data.byte_ptr(), len) };

    let first = |info: CGImageAlphaInfo| {
        matches!(info.0, v if v == CGImageAlphaInfo::PremultipliedFirst.0 || v == CGImageAlphaInfo::First.0 || v == CGImageAlphaInfo::NoneSkipFirst.0)
    };
    let little = matches!(
        CGImage::byte_order_info(Some(image)).0,
        v if v == CGImageByteOrderInfo::Order32Little.0 || v == CGImageByteOrderInfo::OrderDefault.0
    );
    // Index of R, G and B inside each 4-byte pixel.
    let (ri, gi, bi) = match (little, first(CGImage::alpha_info(Some(image)))) {
        (true, true) => (2, 1, 0),
        (true, false) => (0, 1, 2),
        (false, true) => (1, 2, 3),
        (false, false) => (3, 2, 1),
    };

    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let row = &src[y * bpr..y * bpr + w * 4];
        let dst = &mut out[y * w * 4..(y + 1) * w * 4];
        for (s, d) in row.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            d[0] = s[ri];
            d[1] = s[gi];
            d[2] = s[bi];
            d[3] = 255;
        }
    }
    Ok((w, h, out))
}

/// Capture every given display. Must run off the main thread: it blocks while
/// ScreenCaptureKit calls back.
pub fn capture_all(monitors: &[MonitorInfo], own_pid: i32) -> Result<Vec<Frame>, String> {
    let t0 = crate::util::now_ms();
    // Blocking-pool threads have no autorelease pool of their own; without one,
    // every autoreleased object ScreenCaptureKit hands us would leak until the
    // thread dies.
    objc2::rc::autoreleasepool(|_| capture_all_inner(monitors, own_pid, t0))
}

fn capture_all_inner(monitors: &[MonitorInfo], own_pid: i32, t0: u128) -> Result<Vec<Frame>, String> {
    let content = shareable_content()?;
    // SAFETY: plain getters on the content returned by ScreenCaptureKit.
    let displays = unsafe { content.displays() };
    if displays.count() == 0 {
        // Typical cause: permission granted to an older build of the app, or
        // granted seconds ago and the change needs a relaunch.
        return Err("没有可截取的屏幕：如果刚授权过，请退出并重新打开 Cuting 后重试".to_string());
    }
    // Keep each shot next to its monitor: a display can be skipped, and
    // zipping two lists would then pair the wrong ones.
    let mut shots = Vec::with_capacity(monitors.len());
    for monitor in monitors {
        // SAFETY: objectAtIndex/displayID are plain getters on `displays`.
        let display = unsafe {
            (0..displays.count()).map(|i| displays.objectAtIndex(i)).find(|d| d.displayID() == monitor.id)
        };
        let Some(display) = display else {
            clog!("display {} is not shareable, skipping", monitor.id);
            continue;
        };
        let t = crate::util::now_ms();
        let image = capture_display(&display, monitor)?;
        let (width, height, rgba) = image_to_rgba(&image)?;
        if (width, height) != (monitor.pixel_width as usize, monitor.pixel_height as usize) {
            clog!(
                "display {} captured at {width}x{height}, display mode says {}x{}",
                monitor.id, monitor.pixel_width, monitor.pixel_height
            );
        }
        clog!("display {} captured in {} ms", monitor.id, crate::util::now_ms() - t);
        shots.push((monitor.clone(), image, width, height, rgba));
    }
    // Ask for windows after the pixels so nothing captured goes missing.
    let windows = window_list(own_pid);
    let frames: Vec<Frame> = shots
        .into_iter()
        .map(|(monitor, image, width, height, rgba)| {
            let local = windows
                .iter()
                .filter_map(|w| {
                    let r = w.rect.intersection(&monitor.rect)?;
                    Some(WinRect {
                        id: w.id,
                        layer: w.layer,
                        pid: w.pid,
                        rect: Rect::new(r.x - monitor.rect.x, r.y - monitor.rect.y, r.w, r.h),
                    })
                })
                .collect();
            Frame {
                monitor: monitor.clone(),
                width,
                height,
                rgba: Arc::new(rgba),
                windows: local,
                image: SendImage::new(image),
            }
        })
        .collect();
    clog!("captured {} display(s) in {} ms", frames.len(), crate::util::now_ms() - t0);
    Ok(frames)
}

/// Encode RGBA8 pixels as PNG. `pixels_per_point` is written as the PNG pixel
/// density so Retina screenshots keep their on-screen size when placed into
/// documents (chat apps ignore it and paste at pixel size, which is also fine).
pub fn encode_png(rgba: &[u8], width: u32, height: u32, pixels_per_point: f64) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_compression(png::Compression::Fast);
    let per_meter = (pixels_per_point * 72.0 / 0.0254).round() as u32;
    if per_meter > 0 {
        encoder.set_pixel_dims(Some(png::PixelDimensions { xppu: per_meter, yppu: per_meter, unit: png::Unit::Meter }));
    }
    let mut writer = encoder.write_header().map_err(|e| format!("PNG 编码失败：{e}"))?;
    writer.write_image_data(rgba).map_err(|e| format!("PNG 编码失败：{e}"))?;
    drop(writer);
    Ok(out)
}

/// Build a CGImage from plain RGBA8 pixels (used by tests and previews).
pub fn rgba_to_image(rgba: &[u8], width: u32, height: u32) -> Option<CFRetained<CGImage>> {
    let color_space = CGColorSpace::new_device_rgb()?;
    let data = CFData::from_bytes(rgba);
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    let info = CGBitmapInfo(CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::OrderDefault.0);
    // SAFETY: the provider holds the bytes, sizes match the buffer, the rest are values.
    unsafe {
        CGImage::new(
            width as usize,
            height as usize,
            8,
            32,
            width as usize * 4,
            Some(&color_space),
            info,
            Some(&provider),
            std::ptr::null(),
            true,
            CGColorRenderingIntent(0),
        )
    }
}

/// Decode a PNG file into RGBA8.
///
/// Every caller feeds it PNGs this app encoded itself (annotation layers,
/// debug dumps), so the strict 8-bit-per-channel paths cover reality; anything
/// exotic is refused rather than half-converted.
pub fn decode_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(|e| format!("PNG 解码失败：{e}"))?;
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("PNG 解码失败：{e}"))?;
    let (w, h) = (info.width, info.height);
    let len = w as usize * h as usize * 4;
    let rgba = match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => buf[..len].to_vec(),
        (png::ColorType::Rgb, png::BitDepth::Eight) => {
            let mut out = Vec::with_capacity(len);
            for px in buf[..w as usize * h as usize * 3].chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
        (png::ColorType::Grayscale, png::BitDepth::Eight) => buf[..w as usize * h as usize]
            .iter()
            .flat_map(|&g| [g, g, g, 255])
            .collect(),
        (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => {
            let mut out = Vec::with_capacity(len);
            for px in buf[..w as usize * h as usize * 2].chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
            out
        }
        _ => return Err(format!("不支持的 PNG 格式：{:?} / {:?}", info.color_type, info.bit_depth)),
    };
    Ok((w, h, rgba))
}

/// Alpha-composite a transparent annotation layer over an opaque screenshot
/// crop (both RGBA8, identical dimensions), with straight src-over maths:
/// out = over·a + base·(1−a). The web layer renders the annotations, so the
/// pixels the user drew are exactly the pixels that get copied or saved.
///
/// An opaque layer (the mosaic tool pastes real pixels in) simply replaces the
/// crop, which is what "drawn on top" should mean there too.
pub fn composite_overlay(base: &[u8], over: &[u8]) -> Result<Vec<u8>, String> {
    if base.len() != over.len() {
        return Err(format!("标注图层与截图尺寸不匹配（{} vs {} 字节）", over.len(), base.len()));
    }
    let mut out = base.to_vec();
    for (dst, src) in out.chunks_exact_mut(4).zip(over.chunks_exact(4)) {
        let a = src[3] as u32;
        if a == 0 {
            continue;
        }
        if a == 255 {
            dst.copy_from_slice(src);
            continue;
        }
        for c in 0..3 {
            let blended = src[c] as u32 * a + dst[c] as u32 * (255 - a) + 127;
            dst[c] = (blended / 255) as u8;
        }
        // The base is opaque, so the result stays opaque.
    }
    Ok(out)
}

/// Shrink RGBA8 by an integer factor with box averaging (for previews).
pub fn downscale_rgba(rgba: &[u8], width: u32, height: u32, factor: u32) -> (u32, u32, Vec<u8>) {
    if factor <= 1 {
        return (width, height, rgba.to_vec());
    }
    let (fw, fh) = (width / factor, height / factor);
    let mut out = Vec::with_capacity((fw * fh * 4) as usize);
    for y in 0..fh {
        for x in 0..fw {
            let mut sum = [0u32; 4];
            for dy in 0..factor {
                let row = ((y * factor + dy) * width) as usize;
                for dx in 0..factor {
                    let i = (row + (x * factor + dx) as usize) * 4;
                    for c in 0..4 {
                        sum[c] += rgba[i + c] as u32;
                    }
                }
            }
            let n = factor * factor;
            out.extend(sum.map(|v| (v / n) as u8));
        }
    }
    (fw, fh, out)
}

/// Nearest-neighbour zoom of the `(2 * half + 1)`-pixel patch around
/// `(cx, cy)`, for the magnifier. Inverted-colour crosshair lines stop at the
/// picked pixel and a one-pixel box marks it, so it stays fully visible.
///
/// `side_px` is the output size in physical pixels and must be a multiple of
/// the patch side. `(cx, cy)` must be at least `half` pixels away from every
/// edge of the frame.
pub fn render_magnifier(
    rgba: &[u8],
    width: usize,
    cx: usize,
    cy: usize,
    half: usize,
    side_px: usize,
) -> Option<Vec<u8>> {
    let patch = half * 2 + 1;
    if side_px % patch != 0 || cx < half || cy < half {
        return None;
    }
    let block = side_px / patch;
    let mut out = vec![0u8; side_px * side_px * 4];
    for by in 0..patch {
        for bx in 0..patch {
            let src = ((cy - half + by) * width + (cx - half + bx)) * 4;
            for ry in 0..block {
                let row = ((by * block + ry) * side_px + bx * block) * 4;
                for rx in 0..block {
                    let dst = row + rx * 4;
                    out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
                }
            }
        }
    }
    // Inverting keeps the marks visible on any background.
    let mut invert = |x: usize, y: usize| {
        let i = (y * side_px + x) * 4;
        out[i] = 255 - out[i];
        out[i + 1] = 255 - out[i + 1];
        out[i + 2] = 255 - out[i + 2];
    };
    let mid = half * block + block / 2;
    let (x0, x1) = (half * block, (half + 1) * block - 1);
    for i in 0..side_px {
        if !(x0..=x1).contains(&i) {
            invert(mid, i);
            invert(i, mid);
        }
        invert(i, x0);
        invert(i, x1);
        invert(x0, i);
        invert(x1, i);
    }
    Some(out)
}

/// One frame covering the bounding box of every display, so a selection can
/// span displays (the macOS-native behaviour). Regions that belong to no
/// display — the gap between two screens — are filled dark. Frames are
/// resampled to the highest scale; single-scale setups pay nothing.
pub fn union_frame(frames: &[Frame]) -> Option<Frame> {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut max_scale = f64::MIN;
    for frame in frames {
        x0 = x0.min(frame.monitor.rect.x);
        y0 = y0.min(frame.monitor.rect.y);
        x1 = x1.max(frame.monitor.rect.right());
        y1 = y1.max(frame.monitor.rect.bottom());
        max_scale = max_scale.max(frame.scale());
    }
    if frames.is_empty() || !(x1 > x0 && y1 > y0) || max_scale <= 0.0 {
        return None;
    }
    let union = Rect::new(x0, y0, x1 - x0, y1 - y0);
    let width = (union.w * max_scale).round() as usize;
    let height = (union.h * max_scale).round() as usize;
    // Gaps between displays: a dark shade reads as "desktop behind the mask".
    let mut rgba = vec![26u8; width * height * 4];
    for chunk in rgba.chunks_exact_mut(4) {
        chunk[3] = 255;
    }
    for frame in frames {
        let factor = max_scale / frame.scale();
        let (fw, fh) = (frame.width as usize, frame.height as usize);
        let (w, h) = ((fw as f64 * factor).round() as usize, (fh as f64 * factor).round() as usize);
        let ox = ((frame.monitor.rect.x - union.x) * max_scale).round() as usize;
        let oy = ((frame.monitor.rect.y - union.y) * max_scale).round() as usize;
        let rows = h.min(height.saturating_sub(oy));
        let cols = w.min(width.saturating_sub(ox));
        for row in 0..rows {
            let dst_start = ((oy + row) * width + ox) * 4;
            let len = cols * 4;
            if factor == 1.0 {
                let src_start = row * fw * 4;
                rgba[dst_start..dst_start + len].copy_from_slice(&frame.rgba[src_start..src_start + len]);
            } else {
                // Nearest-neighbour resample row: crisp for the backdrop.
                for (dx, slot) in rgba[dst_start..dst_start + len].chunks_exact_mut(4).enumerate() {
                    let sx = ((dx as f64 / factor) as usize).min(fw - 1);
                    let sy = ((row as f64 / factor) as usize).min(fh - 1);
                    let src = (sy * fw + sx) * 4;
                    slot.copy_from_slice(&frame.rgba[src..src + 4]);
                }
            }
        }
    }
    let windows: Vec<WinRect> = frames
        .iter()
        .flat_map(|f| {
            f.windows.iter().map(|w| WinRect {
                id: w.id,
                layer: w.layer,
                pid: w.pid,
                rect: Rect::new(w.rect.x + f.monitor.rect.x - union.x, w.rect.y + f.monitor.rect.y - union.y, w.rect.w, w.rect.h),
            })
        })
        .collect();
    let monitor = MonitorInfo {
        id: 0,
        rect: union,
        scale: max_scale,
        pixel_width: width as u32,
        pixel_height: height as u32,
        is_main: true,
    };
    let mut frame = Frame::from_rgba(monitor, width, height, rgba);
    frame.windows = windows;
    Some(frame)
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CFGetRetainCount, CFType};

    use super::*;

    /// Retain count of the frame's image, measured the same way every time.
    fn retain_count(frame: &Frame) -> isize {
        let image = frame.cg_image();
        // SAFETY: any CF type can be viewed as the type-erased CFType.
        let cf = unsafe { CFRetained::cast_unchecked::<CFType>(image) };
        CFGetRetainCount(Some(&cf)) as isize
    }

    /// Every accessor must hand out its own reference: a `get` that reuses the
    /// owner's would release the image once per call and free it under us.
    #[test]
    fn image_accessors_keep_the_reference_count_flat() {
        let frame = test_frame(8, 8, 8.0, 8.0);
        let before = retain_count(&frame);
        for _ in 0..50 {
            drop(frame.cg_image());
            drop(frame.crop_image(&IRect { x: 1, y: 1, w: 2, h: 2 }));
            let clone = frame.clone();
            drop(clone.crop_image(&IRect { x: 0, y: 0, w: 4, h: 4 }));
            drop(clone);
        }
        assert_eq!(retain_count(&frame), before);
        // The image is still alive and intact.
        assert_eq!(CGImage::width(Some(&frame.cg_image())), 8);
    }

    #[test]
    fn magnifier_zooms_and_marks_the_center_pixel() {
        // 3x3 frame with a distinct colour per pixel; zoomed 3x → 9x9.
        let rgba: Vec<u8> = (0..9 * 4).map(|i| i as u8).collect();
        let out = render_magnifier(&rgba, 3, 1, 1, 1, 9).unwrap();
        assert_eq!(out.len(), 9 * 9 * 4);
        let px = |x: usize, y: usize| &out[(y * 9 + x) * 4..(y * 9 + x) * 4 + 3];
        // Each source pixel is a 3x3 block; corners keep their colours.
        assert_eq!(px(0, 0), &[0, 1, 2]);
        assert_eq!(px(8, 8), &[32, 33, 34]);
        // The picked pixel (center block, 3..6) is untouched inside its outline.
        assert_eq!(px(4, 4), &[16, 17, 18]);
        // Its outline inverts the colour: (16,17,18) → (239,238,237).
        assert_eq!(px(4, 3), &[239, 238, 237]);
        // Crosshair lines invert what they cross: above the block lies pixel
        // (1,0) = (4,5,6) → (251,250,249).
        assert_eq!(px(4, 0), &[251, 250, 249]);
        // Off-frame requests are refused rather than read out of bounds.
        assert!(render_magnifier(&rgba, 3, 0, 1, 1, 9).is_none());
        assert!(render_magnifier(&rgba, 3, 1, 1, 1, 8).is_none());
    }

    fn test_frame(w: usize, h: usize, logical_w: f64, logical_h: f64) -> Frame {
        let monitor = MonitorInfo {
            id: 1,
            rect: Rect::new(10.0, 20.0, logical_w, logical_h),
            scale: w as f64 / logical_w,
            pixel_width: w as u32,
            pixel_height: h as u32,
            is_main: true,
        };
        Frame::from_rgba(monitor, w, h, (0..w * h * 4).map(|i| i as u8).collect())
    }

    #[test]
    fn logical_rect_maps_to_pixels() {
        let frame = test_frame(200, 100, 100.0, 50.0);
        let px = frame.to_pixels(&Rect::new(5.0, 5.0, 10.0, 10.0)).unwrap();
        assert_eq!(px, IRect { x: 10, y: 10, w: 20, h: 20 });
        // Selections running past the edge are clamped to the frame.
        let px = frame.to_pixels(&Rect::new(90.0, 40.0, 500.0, 500.0)).unwrap();
        assert_eq!(px, IRect { x: 180, y: 80, w: 20, h: 20 });
        assert_eq!(frame.to_pixels(&Rect::new(200.0, 0.0, 10.0, 10.0)), None);
        // Crop reads the right rows: pixel (x=199, y=1) is index 1*200+199.
        let crop = frame.crop_rgba(&IRect { x: 199, y: 1, w: 1, h: 1 });
        let i = ((1 * 200 + 199) * 4) as u8;
        assert_eq!(crop, vec![i, i.wrapping_add(1), i.wrapping_add(2), i.wrapping_add(3)]);
    }

    #[test]
    fn png_round_trip_keeps_pixels() {
        let rgba: Vec<u8> = (0..16 * 16 * 4).map(|i| (i * 7 % 251) as u8).collect();
        let png = encode_png(&rgba, 16, 16, 2.0).unwrap();
        let (w, h, back) = decode_png(&png).unwrap();
        assert_eq!((w, h), (16, 16));
        assert_eq!(back, rgba);
    }

    #[test]
    fn downscale_averages_blocks() {
        let rgba: Vec<u8> = (0..4 * 2 * 4).map(|i| i as u8).collect();
        let (w, h, out) = downscale_rgba(&rgba, 4, 2, 2);
        assert_eq!((w, h), (2, 1));
        // Top-left 2x2 block, channel 0: 0, 4, 16, 20 → mean 10.
        assert_eq!(out[0], 10);
    }

    #[test]
    fn annotation_composite_blends_by_alpha() {
        let base = [10, 20, 30, 255, 200, 150, 100, 255, 1, 2, 3, 255];
        // One fully transparent pixel (base shows through), one opaque
        // (annotation replaces), then a half-transparent one below.
        let over = [0, 0, 0, 0, 255, 0, 0, 255, 90, 90, 90, 128];
        // Length mismatch is an error, not a panic.
        assert!(composite_overlay(&base, &over[..4]).is_err());
        let out = composite_overlay(&base, &over).unwrap();
        assert_eq!(&out[0..4], &[10, 20, 30, 255]); // transparent keeps the base
        assert_eq!(&out[4..8], &[255, 0, 0, 255]); // opaque replaces it
        // 90·128/255 + 1·127/255 + rounding ≈ 45.
        assert_eq!(out[8], ((90 * 128 + 1 * 127 + 127) / 255) as u8);
        assert_eq!(out[11], 255);
    }
}
