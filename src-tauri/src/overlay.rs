//! The full-screen selection overlay: capture flow, window lifecycle, and the
//! AppKit glue that Tauri's window API doesn't expose.
//!
//! Latency notes, since this is the hot path:
//! - Overlay windows are created once at startup and kept hidden, so no
//!   WebView is ever cold-started by a hotkey press.
//! - The frozen frame is a native `NSImageView` *behind* the transparent
//!   webview. Pixels never cross IPC; the web layer only draws UI.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSColor, NSFont, NSImage, NSImageScaling, NSImageView,
    NSAutoresizingMaskOptions, NSRunningApplication, NSScreenSaverWindowLevel, NSEvent, NSEventMask,
    NSEventModifierFlags, NSEventType, NSTextField, NSTextAlignment, NSView, NSWindow, NSWindowAnimationBehavior,
    NSWindowCollectionBehavior, NSWindowOrderingMode, NSWorkspace,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGDisplayBounds, CGMainDisplayID};
use objc2_foundation::{NSObjectNSKeyValueCoding, NSProcessInfo, NSNumber, NSString};
use block2::RcBlock;
use serde::Serialize;
use std::ptr::NonNull;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

use crate::capture::{self, Frame};
use crate::clog;
use crate::geo::{cg_rect, Rect, WinRect};
use crate::util::lock;
use crate::AppState;

pub const RESULT_LABEL: &str = "result";
pub const SETTINGS_LABEL: &str = "settings";
const RESULT_SIZE: (f64, f64) = (600.0, 540.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    Shot,
    Ocr,
    Translate,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Shot => "截图",
            Self::Ocr => "提取文字",
            Self::Translate => "截图翻译",
        }
    }
}

pub struct Session {
    pub id: u64,
    pub mode: Mode,
    /// One frozen frame per display, for that display's overlay and payload.
    pub frames: Vec<Frame>,
    /// Every display composited into one frame, for crops that span displays.
    pub desktop: Frame,
    pub previous_app_pid: Option<i32>,
}

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
/// True between "hotkey pressed" and "pixels are in", so a double press
/// cancels instead of starting a second capture.
static CAPTURING: AtomicBool = AtomicBool::new(false);

thread_local! {
    static FROZEN_VIEWS: RefCell<HashMap<String, Retained<NSImageView>>> = RefCell::new(HashMap::new());
    static MAG_VIEWS: RefCell<HashMap<String, Retained<NSImageView>>> = RefCell::new(HashMap::new());
    static MAG_LABELS: RefCell<HashMap<String, Retained<NSTextField>>> = RefCell::new(HashMap::new());
    static EVENT_MONITOR: RefCell<Option<Retained<AnyObject>>> = RefCell::new(None);
}

/// Native magnifier size in CSS pixels.
const MAG_CSS: f64 = 126.0;
/// Height of the colour readout under the magnifier.
const MAG_LABEL_CSS: f64 = 17.0;
/// Half of the patch side in physical pixels (side = 2 * MAG_HALF + 1).
const MAG_HALF: i32 = 10;

fn overlay_label(monitor_id: u32) -> String {
    format!("overlay-{monitor_id}")
}

pub fn monitor_id_from_label(label: &str) -> Option<u32> {
    label.strip_prefix("overlay-")?.parse().ok()
}

/// Run `f` on the main thread: right away if we're already there, otherwise on
/// the next main-loop turn.
fn on_main(app: &AppHandle, f: impl FnOnce() + Send + 'static) {
    if MainThreadMarker::new().is_some() {
        f();
    } else {
        let _ = app.run_on_main_thread(f);
    }
}

/// Flip a global CoreGraphics rect (y down, origin = main display top-left)
/// into AppKit screen coordinates (y up, origin = main display bottom-left).
fn to_ns_rect(rect: &Rect) -> objc2_core_foundation::CGRect {
    let main = CGDisplayBounds(CGMainDisplayID());
    cg_rect(rect.x, main.origin.y + main.size.height - rect.bottom(), rect.w, rect.h)
}

/// Mouse position in global CoreGraphics coordinates.
fn mouse_cg() -> (f64, f64) {
    let p = NSEvent::mouseLocation();
    let main = CGDisplayBounds(CGMainDisplayID());
    (p.x, main.origin.y + main.size.height - p.y)
}

fn ns_window(window: &WebviewWindow) -> Option<Retained<NSWindow>> {
    let ptr = window.ns_window().ok()? as *mut NSWindow;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: Tauri hands out a pointer to its live NSWindow; we take our own +1.
    unsafe { Retained::retain(ptr) }
}

/// The page's own view (the WKWebView) inside a Tauri webview window.
///
/// `WebviewWindow::ns_view()` is *not* it: that is wry's container view, which
/// is also the window's content view. The webview is its child.
pub(crate) fn webview_ns_view(ns: &NSWindow) -> Option<Retained<NSView>> {
    ns.contentView()?
        .subviews()
        .into_iter()
        .find(|view| view.class().name().to_string_lossy().contains("WebView"))
}

pub(crate) fn ns_window_of(app: &AppHandle, monitor_id: u32) -> Option<Retained<NSWindow>> {
    ns_window(&app.get_webview_window(&overlay_label(monitor_id))?)
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartPayload {
    pub session_id: u64,
    /// This overlay's window label; pages echo it when reporting selections.
    pub label: String,
    /// This display's global origin (CSS, y down from the main display's
    /// top-left): display-local + origin = desktop coordinates.
    pub x: f64,
    pub y: f64,
    pub monitor_id: u32,
    pub mode: Mode,
    /// Physical pixels per CSS pixel.
    pub scale: f64,
    pub width: f64,
    pub height: f64,
    /// Windows in z-order (front first), local to this monitor.
    pub windows: Vec<WinRect>,
}

fn payload_for(session_id: u64, mode: Mode, frame: &Frame) -> StartPayload {
    StartPayload {
        session_id,
        label: overlay_label(frame.monitor.id),
        x: frame.monitor.rect.x,
        y: frame.monitor.rect.y,
        monitor_id: frame.monitor.id,
        mode,
        scale: frame.scale(),
        width: frame.monitor.rect.w,
        height: frame.monitor.rect.h,
        windows: frame.windows.clone(),
    }
}

/// What an overlay window needs to know. Offered on demand because an event
/// fired before a freshly created page finished loading would be lost.
pub fn start_payload(app: &AppHandle, monitor_id: u32) -> Option<StartPayload> {
    let state = app.state::<AppState>();
    let session = lock(&state.session);
    let session = session.as_ref()?;
    let frame = session.frames.iter().find(|f| f.monitor.id == monitor_id)?;
    Some(payload_for(session.id, session.mode, frame))
}

pub fn is_active(app: &AppHandle) -> bool {
    lock(&app.state::<AppState>().session).is_some()
}

/// A cheap clone of the frozen frame for `monitor_id` in the active session.
pub fn frame(app: &AppHandle, monitor_id: u32) -> Option<Frame> {
    lock(&app.state::<AppState>().session).as_ref()?.frames.iter().find(|f| f.monitor.id == monitor_id).cloned()
}

/// A cheap clone of the composited desktop (every display in one frame).
pub fn desktop_frame(app: &AppHandle) -> Option<Frame> {
    lock(&app.state::<AppState>().session).as_ref().map(|s| s.desktop.clone())
}

/// Convert a display-local selection into desktop coordinates, so crops can
/// come from the composited desktop frame. Unclamped: a selection that
/// extends onto another display keeps its off-window coordinates here.
pub fn desktop_rect(app: &AppHandle, label: &str, local: &Rect) -> Option<Rect> {
    let monitor_id = monitor_id_from_label(label)?;
    let frame = frame(app, monitor_id)?;
    Some(Rect::new(local.x + frame.monitor.rect.x, local.y + frame.monitor.rect.y, local.w, local.h))
}

/// Relay a page's selection rectangle to every overlay page.
///
/// `rect` is DESKTOP coordinates (page display-local + display origin); each
/// page draws only the part that falls on its own display, which is what
/// makes a selection work across screens. `rect = None` clears everywhere
/// (a drag restarted on another screen).
pub fn broadcast_selection(app: &AppHandle, rect: Option<Rect>, active: &str) {
    let _ = app.emit("selection-update", serde_json::json!({ "rect": rect, "active": active }));
}

/// Id of the capture on screen. Work that finishes asynchronously compares
/// against it to notice that the user cancelled in the meantime.
pub fn session_id(app: &AppHandle) -> Option<u64> {
    lock(&app.state::<AppState>().session).as_ref().map(|s| s.id)
}

// ---------------------------------------------------------------- capture flow

pub fn start(app: &AppHandle, mode: Mode) {
    let handle = app.clone();
    on_main(app, move || start_on_main(&handle, mode));
}

fn start_on_main(app: &AppHandle, mode: Mode) {
    if is_active(app) {
        clog!("cancel capture (hotkey pressed while a capture is active)");
        cancel(app);
        return;
    }
    if CAPTURING.swap(true, Ordering::SeqCst) {
        clog!("capture already in flight, ignoring");
        return;
    }
    if !capture::screen_capture_allowed() {
        clog!("screen recording permission is missing; opening the authorization flow");
        CAPTURING.store(false, Ordering::SeqCst);
        capture::request_screen_capture_access();
        permission_dialog(app);
        return;
    }
    let monitors = capture::active_displays();
    if monitors.is_empty() {
        CAPTURING.store(false, Ordering::SeqCst);
        clog!("no active displays, aborting capture");
        return;
    }
    let previous_app_pid = frontmost_app_pid();
    let own_pid = std::process::id() as i32;
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let captured = tauri::async_runtime::spawn_blocking(move || capture::capture_all(&monitors, own_pid)).await;
        let result = captured.unwrap_or_else(|e| Err(format!("抓屏任务异常：{e}")));
        let handle2 = handle.clone();
        let _ = handle.run_on_main_thread(move || finish_start(&handle2, mode, result, previous_app_pid));
    });
}

fn finish_start(app: &AppHandle, mode: Mode, result: Result<Vec<Frame>, String>, previous_app_pid: Option<i32>) {
    CAPTURING.store(false, Ordering::SeqCst);
    match result {
        Ok(frames) if !frames.is_empty() => present(app, mode, frames, previous_app_pid),
        Err(e) => {
            clog!("capture failed: {e}");
            activate_previous(previous_app_pid);
            alert(app, "截图失败", e);
        }
        Ok(_) => clog!("capture returned no displays"),
    }
}

/// Store the frozen frames and put the overlays on screen.
pub(crate) fn present(app: &AppHandle, mode: Mode, frames: Vec<Frame>, previous_app_pid: Option<i32>) {
    present_inner(app, mode, frames, previous_app_pid, true)
}

fn present_inner(app: &AppHandle, mode: Mode, frames: Vec<Frame>, previous_app_pid: Option<i32>, activate: bool) {
    let session_id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let mut shown = 0usize;
    for frame in &frames {
        match ensure_overlay_window(app, frame.monitor.id) {
            Ok(window) => {
                if let Err(e) = prepare_overlay(&window, frame) {
                    clog!("overlay for display {}: {e}", frame.monitor.id);
                    continue;
                }
                let _ = window.emit("capture-start", payload_for(session_id, mode, frame));
                shown += 1;
            }
            Err(e) => clog!("overlay for display {}: {e}", frame.monitor.id),
        }
    }
    if shown == 0 {
        // Nothing is on screen, so there must be no session either —
        // otherwise the next hotkey press would just "cancel" an invisible one.
        alert(app, "截图失败", "无法显示截图覆盖层，请查看日志后重试".to_string());
        return;
    }
    let Some(desktop) = capture::union_frame(&frames) else {
        clog!("union frame failed; nothing to present");
        return;
    };
    let state = app.state::<AppState>();
    *lock(&state.session) = Some(Session { id: session_id, mode, frames, desktop, previous_app_pid });
    if activate {
        show_overlays(app);
    } else {
        show_without_activation(app);
    }
    install_magnifier(app);
}

/// Show the overlays without taking focus or keyboard input. Only the
/// automated self test uses this, so a run never interrupts whatever the
/// user is doing.
fn show_without_activation(app: &AppHandle) {
    let monitors: Vec<u32> = lock(&app.state::<AppState>().session)
        .as_ref()
        .map(|s| s.frames.iter().map(|f| f.monitor.id).collect())
        .unwrap_or_default();
    for id in monitors {
        if let Some(window) = ns_window_of(app, id) {
            window.orderFrontRegardless();
        }
    }
}

/// Bring the overlays on screen and make ourselves the active app so the
/// overlay receives keyboard events (Esc, arrows, Enter).
fn show_overlays(app: &AppHandle) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let monitors: Vec<(u32, Rect)> = lock(&app.state::<AppState>().session)
        .as_ref()
        .map(|s| s.frames.iter().map(|f| (f.monitor.id, f.monitor.rect)).collect())
        .unwrap_or_default();
    if monitors.is_empty() {
        return;
    }
    let application = NSApplication::sharedApplication(mtm);
    // The deprecated call is still the one that reliably works from a
    // non-active accessory app; `activate` alone is sometimes ignored.
    #[allow(deprecated)]
    application.activateIgnoringOtherApps(true);
    application.activate();

    let (mx, my) = mouse_cg();
    let mut key = None;
    for (id, rect) in &monitors {
        if rect.contains(mx, my) {
            key = Some(*id);
        }
        if let Some(window) = ns_window_of(app, *id) {
            window.orderFrontRegardless();
        }
    }
    // The display under the mouse gets keyboard focus; Esc anywhere cancels.
    if let Some(window) = key.or_else(|| monitors.first().map(|m| m.0)).and_then(|id| ns_window_of(app, id)) {
        window.makeKeyWindow();
    }
}

/// Take every overlay off screen and release the frozen pixels and the
/// magnifier's event monitor.
fn hide_overlays(app: &AppHandle) {
    for (label, window) in app.webview_windows() {
        if monitor_id_from_label(&label).is_some() {
            let _ = window.emit("capture-end", ());
            if let Some(ns) = ns_window(&window) {
                ns.orderOut(None);
            }
            // The view itself stays for next time; only its image is dropped.
            FROZEN_VIEWS.with(|views| {
                if let Some(view) = views.borrow().get(&label) {
                    view.setImage(None);
                }
            });
        }
    }
    remove_magnifier_mt(app);
}

/// Close the overlay. Returns the app that was frontmost before the capture so
/// the caller can hand focus back once it's done (a save dialog, a result
/// window). Callers that show nothing call `restore` right away.
pub fn end_session(app: &AppHandle) -> Option<i32> {
    let handle = app.clone();
    on_main(app, move || hide_overlays(&handle));
    lock(&app.state::<AppState>().session).take().and_then(|s| s.previous_app_pid)
}

/// Give focus back to the app that was frontmost before the last capture.
pub fn restore(app: &AppHandle, pid: Option<i32>) {
    on_main(app, move || activate_previous(pid));
}

pub fn cancel(app: &AppHandle) {
    let previous = end_session(app);
    restore(app, previous);
}

fn activate_previous(pid: Option<i32>) {
    if let Some(app) = pid.and_then(NSRunningApplication::runningApplicationWithProcessIdentifier) {
        // Default options: bring back the window that was active before, not
        // every window of that app. We are the active app at this point, which
        // is what macOS 14+ requires to hand focus to another app.
        app.activateWithOptions(NSApplicationActivationOptions(0));
    }
}

fn frontmost_app_pid() -> Option<i32> {
    if MainThreadMarker::new().is_none() {
        return None;
    }
    let pid = NSWorkspace::sharedWorkspace().frontmostApplication()?.processIdentifier();
    (pid != std::process::id() as i32).then_some(pid)
}

/// Set while the "grant permission" flow is active so repeated hotkey presses
/// don't stack dialogs and pollers.
static PERMISSION_FLOW: AtomicBool = AtomicBool::new(false);

fn permission_dialog(app: &AppHandle) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    if PERMISSION_FLOW.swap(true, Ordering::SeqCst) {
        clog!("the permission flow is already open; not opening another one");
        return; // already guiding the user through the settings
    }
    clog!("showing the permission dialog and opening System Settings on request");
    let handle = app.clone();
    app.dialog()
        .message("Cuting 需要「屏幕录制」权限才能截图。\n\n点击「打开系统设置」后，在列表中打开 Cuting；\n授权成功后 Cuting 会自动重启并进入截图。")
        .title("需要屏幕录制权限")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom("打开系统设置".into(), "稍后".into()))
        .show(move |ok| {
            if ok {
                open_screen_settings();
            }
            watch_permission(handle.clone(), ok);
        });
}

/// Poll until the system reports the permission, then offer an immediate
/// restart — macOS applies screen-recording changes on relaunch.
fn watch_permission(app: AppHandle, watch: bool) {
    if !watch {
        PERMISSION_FLOW.store(false, Ordering::SeqCst);
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..600 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if capture::screen_capture_allowed() {
                clog!("screen recording permission granted; offering a restart");
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || offer_restart(&handle));
                return;
            }
        }
        PERMISSION_FLOW.store(false, Ordering::SeqCst);
    });
}

fn offer_restart(app: &AppHandle) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    let handle = app.clone();
    app.dialog()
        .message("屏幕录制权限已获取。\n\n重启 Cuting 后生效，现在重启吗？")
        .title("授权成功")
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom("立即重启".into(), "稍后自行重启".into()))
        .show(move |yes| {
            PERMISSION_FLOW.store(false, Ordering::SeqCst);
            if yes {
                relaunch(&handle);
            }
        });
}

/// Relaunch via the .app bundle — that keeps the TCC identity, launching the
/// bare binary would look like a different app to the system.
fn relaunch(app: &AppHandle) {
    let exe = std::env::current_exe().ok();
    let bundle = exe
        .as_ref()
        .and_then(|p| p.ancestors().nth(2).map(std::path::Path::to_path_buf))
        .filter(|p| p.extension().is_some_and(|e| e == "app"));
    if let Some(bundle) = bundle {
        let _ = std::process::Command::new("open").arg(bundle).spawn();
    } else if let Some(exe) = exe {
        let _ = std::process::Command::new(exe).spawn();
    }
    app.exit(0);
}

pub fn open_screen_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        .spawn();
}

pub(crate) fn alert(app: &AppHandle, title: &str, message: String) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
    app.dialog().message(message).title(title).kind(MessageDialogKind::Error).show(|_| {});
}

/// Bind the three hotkeys from the current settings, replacing old bindings.
/// One shortcut that is taken must not take the others down with it: every
/// binding is attempted, and the failures are reported together.
pub fn register_hotkeys(app: &AppHandle) -> Result<(), String> {
    let settings = lock(&app.state::<AppState>().settings).clone();
    app.global_shortcut().unregister_all().map_err(|e| format!("重置快捷键失败：{e}"))?;
    let bindings = [
        (Mode::Shot, settings.hotkeys.shot),
        (Mode::Ocr, settings.hotkeys.ocr),
        (Mode::Translate, settings.hotkeys.translate),
    ];
    let mut failures = Vec::new();
    for (mode, combo) in bindings {
        if combo.trim().is_empty() {
            continue;
        }
        let registered = app
            .global_shortcut()
            .on_shortcut(combo.as_str(), move |app, _shortcut, event| {
                if event.state() == ShortcutState::Pressed {
                    start(app, mode);
                }
            })
            .map_err(|e| e.to_string());
        if let Err(e) = registered {
            failures.push(format!("{}「{combo}」：{e}", mode.label()));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("快捷键已被其他应用占用，对应功能无法使用：\n{}", failures.join("\n")))
    }
}

// ------------------------------------------------------------ overlay windows

/// Create hidden overlay windows for the current displays so the first
/// hotkey press doesn't pay for a cold WebView.
pub fn prewarm(app: &AppHandle) {
    for monitor in capture::active_displays() {
        if let Err(e) = ensure_overlay_window(app, monitor.id) {
            clog!("{e}");
        }
    }
}

/// Create the hidden overlay window for a display, or return the existing one.
fn ensure_overlay_window(app: &AppHandle, monitor_id: u32) -> Result<WebviewWindow, String> {
    let label = overlay_label(monitor_id);
    if let Some(window) = app.get_webview_window(&label) {
        return Ok(window);
    }
    clog!("creating overlay window {label}");
    let window = WebviewWindowBuilder::new(app, &label, WebviewUrl::App("index.html".into()))
        .title("Cuting")
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(false)
        .visible(false)
        .accept_first_mouse(true)
        .build()
        .map_err(|e| format!("创建覆盖层窗口失败：{e}"))?;
    if let Some(ns) = ns_window(&window) {
        configure_overlay(&ns);
    }
    Ok(window)
}

fn configure_overlay(ns: &NSWindow) {
    ns.setLevel(NSScreenSaverWindowLevel);
    ns.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    ns.setAnimationBehavior(NSWindowAnimationBehavior::None);
    ns.setHasShadow(false);
    ns.setIgnoresMouseEvents(false);
}

/// Fit the window to its display and put the frozen frame behind the webview.
fn prepare_overlay(window: &WebviewWindow, frame: &Frame) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("prepare_overlay must run on the main thread")?;
    let ns = ns_window(window).ok_or("覆盖层窗口尚未就绪")?;
    configure_overlay(&ns);
    ns.setFrame_display(to_ns_rect(&frame.monitor.rect), false);

    let webview = webview_ns_view(&ns).ok_or("覆盖层的网页视图尚未就绪")?;
    let view = frozen_view(mtm, &ns, &webview, window.label());
    let image = NSImage::initWithCGImage_size(
        NSImage::alloc(),
        &frame.cg_image(),
        CGSize::new(frame.monitor.rect.w, frame.monitor.rect.h),
    );
    view.setImage(Some(&image));
    ensure_magnifier(mtm, &ns, &webview, window.label());
    Ok(())
}

/// The image view sitting between the window background and the webview.
fn frozen_view(mtm: MainThreadMarker, ns: &NSWindow, webview: &NSView, key: &str) -> Retained<NSImageView> {
    if let Some(view) = FROZEN_VIEWS.with(|views| views.borrow().get(key).cloned()) {
        return view;
    }
    // SAFETY: plain getter on the live webview owned by the window.
    let parent = unsafe { webview.superview() }
        .or_else(|| ns.contentView())
        .expect("window has a content view");
    let view = NSImageView::initWithFrame(NSImageView::alloc(mtm), parent.bounds());
    view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
    view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    parent.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Below, Some(webview));
    FROZEN_VIEWS.with(|views| views.borrow_mut().insert(key.to_string(), view.clone()));
    view
}

// ----------------------------------------------------------------- magnifier

/// The native magnifier floats above the webview: the cursor-following zoom
/// and its colour readout are drawn from the frozen pixels, with no IPC on
/// pointer moves. It never sits under the cursor (it keeps an 18 px gap), so
/// it never swallows clicks meant for the overlay.
fn ensure_magnifier(mtm: MainThreadMarker, ns: &NSWindow, webview: &NSView, key: &str) {
    // SAFETY: plain getter on the live webview owned by the window.
    let parent = unsafe { webview.superview() }
        .or_else(|| ns.contentView())
        .expect("window has a content view");
    let view = match MAG_VIEWS.with(|views| views.borrow().get(key).cloned()) {
        Some(view) => view,
        None => {
            let view = NSImageView::initWithFrame(NSImageView::alloc(mtm), cg_rect(0.0, 0.0, MAG_CSS, MAG_CSS));
            // No autoresizing: the magnifier keeps its size when the window is refit.
            view.setAutoresizingMask(NSAutoresizingMaskOptions::empty());
            view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
            view.setHidden(true);
            parent.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Above, Some(webview));
            MAG_VIEWS.with(|views| views.borrow_mut().insert(key.to_string(), view.clone()));
            view
        }
    };
    if MAG_LABELS.with(|labels| labels.borrow().get(key).is_none()) {
        let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        label.setFrame(cg_rect(0.0, 0.0, MAG_CSS, MAG_LABEL_CSS));
        label.setAutoresizingMask(NSAutoresizingMaskOptions::empty());
        label.setDrawsBackground(true);
        label.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.07, 0.88)));
        label.setTextColor(Some(&NSColor::whiteColor()));
        label.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        label.setAlignment(NSTextAlignment::Center);
        label.setHidden(true);
        parent.addSubview_positioned_relativeTo(&label, NSWindowOrderingMode::Above, Some(&view));
        MAG_LABELS.with(|labels| labels.borrow_mut().insert(key.to_string(), label));
    }
}

/// The overlay page tells us which phase it is in; the magnifier only follows
/// the cursor while the user is selecting.
pub fn set_magnifier_phase(app: &AppHandle, select: bool) {
    app.state::<AppState>().magnifier_on.store(select, Ordering::Relaxed);
    if !select {
        on_main(app, hide_magnifiers);
    }
}

fn hide_magnifiers() {
    MAG_VIEWS.with(|views| {
        for view in views.borrow().values() {
            view.setHidden(true);
        }
    });
    MAG_LABELS.with(|labels| {
        for label in labels.borrow().values() {
            label.setHidden(true);
        }
    });
}

/// Watch local mouse events while a session is up and move the magnifier.
/// The handler runs on the main thread (local monitors are dispatched there).
fn install_magnifier(app: &AppHandle) {
    if EVENT_MONITOR.with(|m| m.borrow().is_some()) {
        return;
    }
    app.state::<AppState>().magnifier_on.store(true, Ordering::Relaxed);
    let handle = app.clone();
    let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: the event is a live borrowed reference for this call.
        let event = unsafe { event.as_ref() };
        magnifier_track(&handle, event);
        event as *const NSEvent as *mut NSEvent
    });
    let mask = NSEventMask::MouseMoved | NSEventMask::LeftMouseDragged;
    // SAFETY: the block stays valid for the monitor's lifetime (AppKit copies
    // it) and we remove the monitor before the handle is dropped.
    let token = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) };
    EVENT_MONITOR.with(|m| *m.borrow_mut() = token);
}

/// Main-thread teardown of the magnifier: monitor off, views hidden, pixels dropped.
fn remove_magnifier_mt(app: &AppHandle) {
    EVENT_MONITOR.with(|m| {
        if let Some(token) = m.borrow_mut().take() {
            // SAFETY: the token came from addLocalMonitor... and is still installed.
            unsafe { NSEvent::removeMonitor(&token) };
        }
    });
    hide_magnifiers();
    MAG_VIEWS.with(|views| {
        for view in views.borrow().values() {
            view.setImage(None);
        }
    });
    app.state::<AppState>().magnifier_on.store(false, Ordering::Relaxed);
}

struct Zoomed {
    monitor_id: u32,
    monitor_rect: Rect,
    /// Output pixels; a square of `side * side * 4` bytes.
    pixels: Vec<u8>,
    side: u32,
    /// RGB of the pixel under the cursor.
    picked: [u8; 3],
}

/// Read what the magnifier should show. Pure pixel work, kept separate from
/// the AppKit updates so the session lock is not held across them.
fn zoom_at_cursor(gx: f64, gy: f64, session: &Session) -> Option<Zoomed> {
    let frame = session.frames.iter().find(|f| f.monitor.rect.contains(gx, gy))?;
    let s = frame.scale();
    let (w, h) = (frame.width as i32, frame.height as i32);
    let clamp_px = |v: f64, max: i32| (v.round() as i32).clamp(0, max - 1);
    // The pixel under the cursor, and a centre that keeps the patch on-frame.
    let (px, py) = (
        clamp_px((gx - frame.monitor.rect.x) * s, w),
        clamp_px((gy - frame.monitor.rect.y) * s, h),
    );
    let half = MAG_HALF as usize;
    let patch = half * 2 + 1;
    let (cx, cy) = (px.clamp(MAG_HALF, w - MAG_HALF - 1) as usize, py.clamp(MAG_HALF, h - MAG_HALF - 1) as usize);
    // Output is MAG_CSS points at the display's scale, rounded to a whole
    // number of blocks so every source pixel is the same size.
    let side = (((MAG_CSS * s).round() as usize) / patch).max(1) * patch;
    let pixels = capture::render_magnifier(&frame.rgba, frame.width, cx, cy, half, side)?;
    let at = (py as usize * frame.width + px as usize) * 4;
    Some(Zoomed {
        monitor_id: frame.monitor.id,
        monitor_rect: frame.monitor.rect,
        pixels,
        side: side as u32,
        picked: [frame.rgba[at], frame.rgba[at + 1], frame.rgba[at + 2]],
    })
}

/// The pointer position of `event` in global CoreGraphics coordinates.
fn event_cg(event: &NSEvent) -> Option<(f64, f64)> {
    let Some(mtm) = MainThreadMarker::new() else { return None };
    let in_window = event.locationInWindow();
    let on_screen = event.window(mtm)?.convertPointToScreen(in_window);
    let main = CGDisplayBounds(CGMainDisplayID());
    Some((on_screen.x, main.origin.y + main.size.height - on_screen.y))
}

fn magnifier_track(app: &AppHandle, event: &NSEvent) {
    if !app.state::<AppState>().magnifier_on.load(Ordering::Relaxed) {
        hide_magnifiers();
        return;
    }
    let Some((gx, gy)) = event_cg(event) else {
        hide_magnifiers();
        return;
    };
    let state = app.state::<AppState>();
    let zoomed = {
        let session = lock(&state.session);
        session.as_ref().and_then(|s| zoom_at_cursor(gx, gy, s))
    };
    let Some(zoomed) = zoomed else {
        hide_magnifiers();
        return;
    };
    let label_key = overlay_label(zoomed.monitor_id);
    let view = MAG_VIEWS.with(|views| views.borrow().get(&label_key).cloned());
    let label = MAG_LABELS.with(|labels| labels.borrow().get(&label_key).cloned());
    let (Some(view), Some(label)) = (view, label) else { return };
    MAG_VIEWS.with(|views| {
        for (key, other) in views.borrow().iter() {
            other.setHidden(key != &label_key);
        }
    });
    MAG_LABELS.with(|labels| {
        for (key, other) in labels.borrow().iter() {
            other.setHidden(key != &label_key);
        }
    });
    let Some(image) = capture::rgba_to_image(&zoomed.pixels, zoomed.side, zoomed.side) else { return };
    let ns_image = NSImage::initWithCGImage_size(NSImage::alloc(), &image, CGSize::new(MAG_CSS, MAG_CSS));
    view.setImage(Some(&ns_image));

    // Place it clear of the cursor, flipping sides near the screen edges.
    // Monitor-local CSS coordinates, y growing downwards.
    let (w, h) = (zoomed.monitor_rect.w, zoomed.monitor_rect.h);
    let (css_x, css_y) = (gx - zoomed.monitor_rect.x, gy - zoomed.monitor_rect.y);
    let stack = MAG_CSS + MAG_LABEL_CSS;
    let mut left = css_x + 18.0;
    let mut top = css_y + 18.0;
    if left + MAG_CSS > w - 4.0 {
        left = css_x - MAG_CSS - 18.0;
    }
    if top + stack > h - 4.0 {
        top = css_y - stack - 18.0;
    }
    left = left.clamp(4.0, (w - MAG_CSS - 4.0).max(4.0));
    top = top.clamp(4.0, (h - stack - 4.0).max(4.0));
    // CSS (y down) → AppKit (y up, origin bottom-left): the label hangs below.
    let magnifier_y = h - top - MAG_CSS;
    view.setFrameOrigin(CGPoint { x: left, y: magnifier_y });
    label.setFrameOrigin(CGPoint { x: left, y: magnifier_y - MAG_LABEL_CSS });
    let [r, g, b] = zoomed.picked;
    label.setStringValue(&NSString::from_str(&format!(
        "#{r:02X}{g:02X}{b:02X}   {}, {}",
        css_x.round() as i32,
        css_y.round() as i32
    )));
    view.setHidden(false);
    label.setHidden(false);
}

// -------------------------------------------------------------- other windows

fn ensure_result_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    if let Some(window) = app.get_webview_window(RESULT_LABEL) {
        return Ok(window);
    }
    WebviewWindowBuilder::new(app, RESULT_LABEL, WebviewUrl::App("index.html".into()))
        .title("Cuting")
        .inner_size(RESULT_SIZE.0, RESULT_SIZE.1)
        .min_inner_size(420.0, 300.0)
        .always_on_top(true)
        .resizable(true)
        .visible(false)
        .build()
        .map_err(|e| format!("创建结果窗口失败：{e}"))
}

/// Place the result window just below the selection, above it when there is
/// no room, always fully inside the monitor.
fn result_frame(anchor: Option<(u32, Rect)>) -> Rect {
    let (w, h) = RESULT_SIZE;
    let monitors = capture::active_displays();
    let Some((anchor_id, sel)) = anchor else {
        return monitors
            .first()
            .map(|m| Rect::new(m.rect.center().0 - w / 2.0, m.rect.center().1 - h / 2.0, w, h))
            .unwrap_or(Rect::new(100.0, 100.0, w, h));
    };
    let Some(monitor) = monitors.iter().find(|m| m.id == anchor_id).or_else(|| monitors.first()) else {
        return Rect::new(sel.x, sel.y, w, h);
    };
    let r = monitor.rect;
    let x = sel.x.clamp(r.x + 8.0, (r.right() - w - 8.0).max(r.x + 8.0));
    let y = if sel.bottom() + 8.0 + h <= r.bottom() {
        sel.bottom() + 8.0
    } else if sel.y - 8.0 - h >= r.y {
        sel.y - 8.0 - h
    } else {
        sel.y.clamp(r.y + 8.0, (r.bottom() - h - 8.0).max(r.y + 8.0))
    };
    Rect::new(x, y, w, h)
}

/// Show `payload` in the result window, near `anchor` (monitor id + selection).
pub fn show_result(app: &AppHandle, payload: serde_json::Value, anchor: Option<(u32, Rect)>) {
    *lock(&app.state::<AppState>().pending_result) = Some(payload);
    let handle = app.clone();
    on_main(app, move || {
        let window = match ensure_result_window(&handle) {
            Ok(window) => window,
            Err(e) => {
                clog!("{e}");
                return;
            }
        };
        if let Some(ns) = ns_window(&window) {
            ns.setFrame_display(to_ns_rect(&result_frame(anchor)), false);
        }
        // The self test keeps it hidden: the page still loads and takes the
        // payload, and nothing pops up on the user's screen.
        if !handle.state::<AppState>().selftest {
            if let Err(e) = window.show() {
                clog!("show result window: {e}");
            }
            let _ = window.set_focus();
        }
        let _ = window.emit("show-result", ());
    });
}

pub fn show_settings(app: &AppHandle) {
    let handle = app.clone();
    on_main(app, move || {
        let window = match handle.get_webview_window(SETTINGS_LABEL) {
            Some(window) => window,
            None => match WebviewWindowBuilder::new(&handle, SETTINGS_LABEL, WebviewUrl::App("index.html".into()))
                .title("Cuting 设置")
                .inner_size(600.0, 660.0)
                .min_inner_size(480.0, 420.0)
                .center()
                .visible(false)
                .build()
            {
                Ok(window) => window,
                Err(e) => {
                    clog!("创建设置窗口失败：{e}");
                    return;
                }
            },
        };
        let _ = window.unminimize();
        // The self test drives the page in a hidden window instead.
        if !handle.state::<AppState>().selftest {
            if let Err(e) = window.show() {
                clog!("show settings window: {e}");
            }
            let _ = window.set_focus();
        }
    });
}

// ------------------------------------------------------------ self-test hooks
//
// The automated self test (`selftest.rs`) drives the real overlay windows
// without needing screen-recording permission or the user's attention.

/// Put `frames` on screen without taking focus from whatever the user is doing.
pub(crate) fn present_for_test(app: &AppHandle, mode: Mode, frames: Vec<Frame>) {
    present_inner(app, mode, frames, None, false);
}

/// What is actually on screen for one overlay: window geometry, the view stack
/// and how the webview and the frozen image are set up. This is what lets the
/// self test check the things a user would see.
pub(crate) fn overlay_report(app: &AppHandle, monitor_id: u32) -> serde_json::Value {
    use serde_json::json;
    let rect_json = |r: CGRect| json!({ "x": r.origin.x, "y": r.origin.y, "w": r.size.width, "h": r.size.height });
    let Some(window) = ns_window_of(app, monitor_id) else {
        return json!({ "error": "no overlay window" });
    };
    let key = overlay_label(monitor_id);
    let subviews: Vec<serde_json::Value> = window
        .contentView()
        .map(|content| {
            content
                .subviews()
                .iter()
                .map(|view| {
                    let class = view.class().name().to_string_lossy().into_owned();
                    let mut entry = json!({
                        "class": class,
                        "frame": rect_json(view.frame()),
                        "hidden": view.isHidden(),
                        "autoresizing": view.autoresizingMask().0,
                    });
                    if let Some(image_view) = view.downcast_ref::<NSImageView>() {
                        entry["image"] = image_view
                            .image()
                            .map(|image| {
                                let size = image.size();
                                let (pw, ph) = image
                                    .representations()
                                    .firstObject()
                                    .map(|rep| (rep.pixelsWide(), rep.pixelsHigh()))
                                    .unwrap_or((0, 0));
                                json!({ "size": [size.width, size.height], "pixels": [pw, ph] })
                            })
                            .unwrap_or(serde_json::Value::Null);
                    }
                    // wry installs a WKWebView subclass (WryWebView).
                    if class.contains("WebView") {
                        // Private, but KVC finds the `_drawsBackground` getter.
                        // KVC raises an Objective-C exception for unknown keys;
                        // catching it keeps the process alive.
                        let draws_background = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
                            view.valueForKey(&NSString::from_str("drawsBackground"))
                                .and_then(|value| value.downcast::<NSNumber>().ok())
                                .map(|number| number.boolValue())
                        }))
                        .ok();
                        entry["drawsBackground"] = json!(draws_background);
                    }
                    entry
                })
                .collect()
        })
        .unwrap_or_default();
    let magnifier = MAG_VIEWS.with(|views| {
        views
            .borrow()
            .get(&key)
            // `attached` matters: a view that was never added to the window is
            // "not hidden" and still invisible.
            .map(|view| {
                // SAFETY: plain getter on a view we own a reference to.
                let attached = unsafe { view.superview() }.is_some();
                json!({ "hidden": view.isHidden(), "attached": attached, "frame": rect_json(view.frame()) })
            })
    });
    let magnifier_label = MAG_LABELS.with(|labels| {
        labels
            .borrow()
            .get(&key)
            .map(|label| {
                json!({
                    "hidden": label.isHidden(),
                    // SAFETY: plain getter on a view we own a reference to.
                    "attached": unsafe { label.superview() }.is_some(),
                    "frame": rect_json(label.frame()),
                    "text": label.stringValue().to_string(),
                })
            })
    });
    json!({
        "frame": rect_json(window.frame()),
        "visible": window.isVisible(),
        "key": window.isKeyWindow(),
        "app_active": MainThreadMarker::new().is_some_and(|mtm| NSApplication::sharedApplication(mtm).isActive()),
        "opaque": window.isOpaque(),
        "background_alpha": window.backgroundColor().alphaComponent(),
        "subviews": subviews,
        "magnifier": magnifier,
        "magnifier_label": magnifier_label,
    })
}

/// Deliver a synthetic mouse event to the overlay, as if the pointer were at
/// CSS `(x, y)` (monitor-local, y down). `kind` is one of `move`, `down`,
/// `drag`, `up`.
pub(crate) fn post_mouse(app: &AppHandle, monitor_id: u32, kind: &str, x: f64, y: f64) -> Result<(), String> {
    let Some(mtm) = MainThreadMarker::new() else {
        return Err("post_mouse must run on the main thread".to_string());
    };
    let window = ns_window_of(app, monitor_id).ok_or("no overlay window")?;
    let event_type = match kind {
        "move" => NSEventType::MouseMoved,
        "down" => NSEventType::LeftMouseDown,
        "drag" => NSEventType::LeftMouseDragged,
        "up" => NSEventType::LeftMouseUp,
        _ => return Err(format!("unknown mouse event kind {kind}")),
    };
    // AppKit wants window coordinates with the origin at the bottom-left.
    let content_height = window.contentView().map_or(0.0, |content| content.frame().size.height);
    let location = CGPoint::new(x, content_height - y);
    let click_count: isize = if matches!(kind, "down" | "up") { 1 } else { 0 };
    let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        event_type,
        location,
        NSEventModifierFlags::empty(),
        NSProcessInfo::processInfo().systemUptime(),
        window.windowNumber(),
        None,
        0,
        click_count,
        0.0,
    )
    .ok_or("could not create the mouse event")?;
    NSApplication::sharedApplication(mtm).sendEvent(&event);
    Ok(())
}
