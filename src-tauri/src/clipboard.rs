//! Pasteboard access for images.
//!
//! Tauri's clipboard plugin writes images at one point per pixel, which makes
//! a Retina screenshot paste twice as large into apps that honour point sizes
//! (Notes, Pages, Word). We write an `NSImage` at the size it had on screen.

use objc2::{AnyThread, MainThreadMarker};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSImage, NSPasteboard, NSPasteboardWriting};
use objc2_core_foundation::{CFRetained, CGSize};
use objc2_core_graphics::CGImage;
use objc2_foundation::NSArray;

/// Put `image` on the general pasteboard, `width_pt × height_pt` points big.
/// Must run on the main thread: AppKit owns the pasteboard.
pub fn write_image(image: &CFRetained<CGImage>, width_pt: f64, height_pt: f64) -> Result<(), String> {
    if MainThreadMarker::new().is_none() {
        return Err("剪贴板只能在主线程写入".to_string());
    }
    let image = NSImage::initWithCGImage_size(NSImage::alloc(), image, CGSize::new(width_pt, height_pt));
    let writer: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*image);
    let objects = NSArray::from_slice(&[writer]);
    let pasteboard = NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    // `writeObjects` declares the formats the image provides (PNG, TIFF) itself.
    pasteboard.writeObjects(&objects).then_some(()).ok_or_else(|| "写入剪贴板失败".to_string())
}
