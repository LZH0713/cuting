//! Opt-in end-to-end self test.
//!
//! ```sh
//! CUTING_SELFTEST_OUT=/tmp/cuting-selftest src-tauri/target/debug/cuting
//! ```
//!
//! It puts a synthetic frame on the real overlay windows *without taking
//! focus*, drives them with synthetic mouse events, and checks what a user
//! would see and get: how the overlay is built, the selection, the toolbar,
//! the clipboard image and the OCR result. Composited previews and
//! `summary.json` land in the output directory. A watchdog ends the process no
//! matter what, so the overlays can never be left on screen.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_app_kit::{NSBitmapImageRep, NSBitmapImageFileType, NSImage};
use objc2_foundation::{NSDictionary, NSError};
use objc2_web_kit::WKWebView;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::capture::{self, Frame, IRect};
use crate::clog;
use crate::geo::{MonitorInfo, Rect};
use crate::overlay::{self, Mode};
use crate::util::lock;

/// Solid colour of the marker block, in CSS pixels at (300..500, 300..500).
const MARKER: [u8; 3] = [201, 87, 53];
/// The marker block as a CSS rect on the main display.
const MARKER_RECT: (f64, f64, f64, f64) = (300.0, 300.0, 200.0, 200.0);
/// Where the OCR fixture is composited, in physical pixels.
const FIXTURE_AT: (usize, usize) = (1200, 1600);

pub fn requested() -> bool {
    std::env::var_os("CUTING_SELFTEST_OUT").is_some()
}

pub fn run(app: AppHandle) {
    let out = PathBuf::from(std::env::var_os("CUTING_SELFTEST_OUT").unwrap_or_default());
    let _ = std::fs::create_dir_all(&out);
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(60));
        clog!("selftest watchdog fired");
        std::process::exit(99);
    });
    std::thread::spawn(move || {
        // `CUTING_SELFTEST_SCOPE=settings` runs the settings checks only, in a
        // hidden window: nothing appears on screen. `…=scroll` drives a real
        // long-screenshot session against an auto-scrolling window of our own.
        let scope = std::env::var("CUTING_SELFTEST_SCOPE").unwrap_or_default();
        let summary = match scope.as_str() {
            "settings" => drive_settings(&app, &out),
            "scroll" => drive_scroll(&app),
            _ => drive(&app, &out),
        };
        let _ = std::fs::write(out.join("summary.json"), serde_json::to_string_pretty(&summary).unwrap_or_default());
        clog!("selftest finished: {}", summary["verdict"]);
        let quit = app.clone();
        let _ = app.run_on_main_thread(move || {
            overlay::cancel(&quit);
            quit.exit(0);
        });
    });
}

// ---------------------------------------------------------------- plumbing

/// Reports handed in by pages through the `selftest_report` command.
static REPORTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[tauri::command]
pub fn selftest_report(json: String) {
    lock(&REPORTS).push(json);
}

/// Run `f` on the main thread and wait for its result. Only ever called from
/// the driver thread, never from the main thread itself.
fn on_main<T: Send + 'static>(
    app: &AppHandle,
    f: impl FnOnce(&AppHandle) -> T + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = mpsc::channel();
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = tx.send(f(&handle));
    })
    .map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(15)).map_err(|_| "the main thread did not get to the step in time".to_string())
}

fn pause(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// Evaluate `script` in a page and wait for it to report back. `script` is an
/// expression that yields a JSON string, or a promise of one.
fn ask_page(app: &AppHandle, label: &str, script: &str) -> Result<Value, String> {
    lock(&REPORTS).clear();
    let js = format!(
        "Promise.resolve({script}).then(\
            (json) => window.__TAURI_INTERNALS__.invoke('selftest_report', {{ json }}),\
            (error) => window.__TAURI_INTERNALS__.invoke('selftest_report', {{ json: JSON.stringify({{ pageError: String(error) }}) }}),\
         )"
    );
    let owned = label.to_string();
    let window = on_main(app, move |app| {
        app.get_webview_window(&owned).ok_or_else(|| format!("no window named {owned}"))
    })??;
    window.eval(&js).map_err(|e| e.to_string())?;
    // A hidden page throttles its timers, so scripts that wait on them are slow.
    for _ in 0..1200 {
        if let Some(report) = lock(&REPORTS).pop() {
            return serde_json::from_str(&report).map_err(|e| format!("bad page report: {e}"));
        }
        pause(25);
    }
    Err(format!("{label} did not report back"))
}

const OVERLAY_PAGE_STATE: &str = r#"(() => {
  const info = (el) => {
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: r.x, y: r.y, w: r.width, h: r.height, display: getComputedStyle(el).display, text: el.textContent };
  };
  const center = (b) => { const r = b.getBoundingClientRect(); return { x: r.x + r.width / 2, y: r.y + r.height / 2, title: b.title }; };
  return JSON.stringify({
    viewport: [innerWidth, innerHeight, devicePixelRatio],
    selection: info(document.querySelector('.sel')),
    sizeLabel: info(document.querySelector('.size')),
    toolbar: info(document.querySelector('.toolbar')),
    buttons: [...document.querySelectorAll('.tool')].map(center),
    notice: info(document.querySelector('.notice')),
    error: info(document.querySelector('.notice-error')),
    hint: info(document.querySelector('.hint')),
  });
})()"#;

const RESULT_PAGE_STATE: &str = r#"(() => {
  const areas = [...document.querySelectorAll('textarea')];
  const text = (el) => (el ? el.textContent : null);
  return JSON.stringify({
    text: areas[0] ? areas[0].value : null,
    translation: areas[1] ? areas[1].value : null,
    error: text(document.querySelector('.error')),
    hasImage: !!document.querySelector('.preview img'),
  });
})()"#;

fn drag(app: &AppHandle, monitor: u32, from: (f64, f64), to: (f64, f64)) -> Result<(), String> {
    on_main(app, move |app| {
        overlay::post_mouse(app, monitor, "move", from.0, from.1)?;
        overlay::post_mouse(app, monitor, "down", from.0, from.1)?;
        for step in 1..=10u32 {
            let t = f64::from(step) / 10.0;
            overlay::post_mouse(
                app,
                monitor,
                "drag",
                from.0 + (to.0 - from.0) * t,
                from.1 + (to.1 - from.1) * t,
            )?;
        }
        overlay::post_mouse(app, monitor, "up", to.0, to.1)
    })?
}

fn click(app: &AppHandle, monitor: u32, at: (f64, f64)) -> Result<(), String> {
    clog!("[selftest] click at ({}, {})", at.0, at.1);
    on_main(app, move |app| {
        overlay::post_mouse(app, monitor, "move", at.0, at.1)?;
        overlay::post_mouse(app, monitor, "down", at.0, at.1)?;
        overlay::post_mouse(app, monitor, "up", at.0, at.1)
    })?
}

// ---------------------------------------------------------------- evidence

fn png_bytes(image: &NSImage) -> Result<Vec<u8>, String> {
    let tiff = image.TIFFRepresentation().ok_or("snapshot has no TIFF representation")?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff).ok_or("snapshot has no bitmap representation")?;
    // SAFETY: plain method on a live bitmap representation.
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .ok_or("snapshot could not be encoded as PNG")?;
    Ok(png.to_vec())
}

/// Screenshot of the page's own drawing (selection, toolbar, …), returned as
/// RGBA plus the composited preview over the frozen frame.
fn page_snapshot(app: &AppHandle, monitor: u32) -> Result<Vec<u8>, String> {
    let (tx, rx) = mpsc::channel::<Result<usize, String>>();
    let request_tx = tx.clone();
    on_main(app, move |app| {
        let requested = (move || {
            let ns = overlay::ns_window_of(app, monitor).ok_or("no NSWindow")?;
            let view = overlay::webview_ns_view(&ns).ok_or("no webview view")?;
            // The webview is a WKWebView (wry's subclass of it).
            let webview = Retained::as_ptr(&view).cast::<WKWebView>() as *mut WKWebView;
            let tx = request_tx.clone();
            let block = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
                let result = if image.is_null() {
                    Err(unsafe { error_text(error) })
                } else {
                    // SAFETY: retained on WebKit's thread before its pool drains.
                    Ok(Retained::into_raw(unsafe { Retained::retain(image) }.expect("live snapshot")) as usize)
                };
                let _ = tx.send(result);
            });
            // SAFETY: `webview` is the live WKWebView owned by the window.
            // An Objective-C exception here would abort the process, so catch it.
            objc2::exception::catch(std::panic::AssertUnwindSafe(|| unsafe {
                (*webview).takeSnapshotWithConfiguration_completionHandler(None, &block)
            }))
            .map_err(|exception| format!("snapshot raised {exception:?}"))?;
            Ok::<(), String>(())
        })();
        if let Err(e) = requested {
            let _ = tx.send(Err(e));
        }
    })?;
    let ptr = rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "the page snapshot timed out".to_string())??;
    on_main(app, move |_| {
        // SAFETY: the +1 reference retained in the completion handler.
        let image = unsafe { Retained::from_raw(ptr as *mut NSImage) }.expect("live snapshot");
        png_bytes(&image)
    })?
}

unsafe fn error_text(error: *mut NSError) -> String {
    if error.is_null() {
        return "未知错误".to_string();
    }
    // SAFETY: the caller guarantees `error` is a live NSError.
    let e = unsafe { &*error };
    e.localizedDescription().to_string()
}

/// Alpha-blend the page's drawing over the frozen frame and shrink it, so the
/// result can be looked at as a picture of what the user sees.
fn composite_preview(frame: &Frame, page_png: &[u8], out: &Path, name: &str) -> Result<(u32, u32), String> {
    let (pw, ph, page) = capture::decode_png(page_png)?;
    let (w, h) = (frame.width as u32, frame.height as u32);
    if (pw, ph) != (w, h) {
        return Err(format!("page snapshot is {pw}x{ph} but the frame is {w}x{h}"));
    }
    let mut out_pixels = frame.rgba.as_ref().clone();
    for (o, s) in out_pixels.chunks_exact_mut(4).zip(page.chunks_exact(4)) {
        let a = u32::from(s[3]);
        for c in 0..3 {
            o[c] = ((u32::from(s[c]) * a + u32::from(o[c]) * (255 - a)) / 255) as u8;
        }
        o[3] = 255;
    }
    let factor = (w / 1500).max(1);
    let (dw, dh, small) = capture::downscale_rgba(&out_pixels, w, h, factor);
    std::fs::write(out.join(name), capture::encode_png(&small, dw, dh, 1.0)?)
        .map_err(|e| format!("cannot write {name}: {e}"))?;
    Ok((dw, dh))
}

// ---------------------------------------------------------------- the frames

/// A test pattern with known colours: a gradient with a one-point grid, a
/// solid marker block and, if given, the OCR fixture.
fn synthetic_frame(monitor: &MonitorInfo, fixture: Option<&[u8]>) -> Frame {
    let (w, h) = (monitor.pixel_width as usize, monitor.pixel_height as usize);
    let mut rgba = vec![0u8; w * h * 4];
    let scale = monitor.scale;
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            rgba[i] = (x * 255 / w.max(1)) as u8;
            rgba[i + 1] = (y * 255 / h.max(1)) as u8;
            rgba[i + 2] = 90;
            rgba[i + 3] = 255;
            // A grid line every 100 points: marks for eyeballing geometry.
            let on_x = (x as f64 / scale).rem_euclid(100.0) < 1.0;
            let on_y = (y as f64 / scale).rem_euclid(100.0) < 1.0;
            if on_x != on_y {
                rgba[i..i + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
    }
    let (mx, my, mw, mh) = MARKER_RECT;
    let fill = |rgba: &mut [u8], color: [u8; 3], x0: usize, y0: usize, x1: usize, y1: usize| {
        for y in y0..h.min(y1) {
            for x in x0..w.min(x1) {
                let i = (y * w + x) * 4;
                rgba[i..i + 3].copy_from_slice(&color);
            }
        }
    };
    fill(&mut rgba, MARKER, (mx * scale) as usize, (my * scale) as usize, ((mx + mw) * scale) as usize, ((my + mh) * scale) as usize);
    if let Some(bytes) = fixture {
        if let Ok((fw, fh, pixels)) = capture::decode_png(bytes) {
            let (fw, fh) = (fw as usize, fh as usize);
            fill(&mut rgba, [255, 255, 255], FIXTURE_AT.0, FIXTURE_AT.1, FIXTURE_AT.0 + fw, FIXTURE_AT.1 + fh);
            for y in 0..fh.min(h - FIXTURE_AT.1) {
                let src = y * fw * 4;
                let dst = ((FIXTURE_AT.1 + y) * w + FIXTURE_AT.0) * 4;
                rgba[dst..dst + fw * 4].copy_from_slice(&pixels[src..src + fw * 4]);
            }
        }
    }
    Frame::from_rgba(monitor.clone(), w, h, rgba)
}

// ---------------------------------------------------------------- the checks

/// One line per check, collected for `summary.json`.
#[derive(Default)]
struct Recorder {
    failures: usize,
    steps: Vec<Value>,
}

impl Recorder {
    fn step(&mut self, name: &str, result: Result<Value, String>, detail: Value) -> bool {
        let ok = result.is_ok();
        self.failures += usize::from(!ok);
        let error = result.err();
        clog!("[selftest] {} {name}: {error:?}", if ok { "ok" } else { "FAILED" });
        let entry = json!({ "step": name, "ok": ok, "detail": detail, "error": error });
        // Persist as we go: a crash later must not take the evidence with it.
        if let Ok(dir) = std::env::var("CUTING_SELFTEST_OUT") {
            use std::io::Write as _;
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(Path::new(&dir).join("steps.jsonl")) {
                let _ = writeln!(file, "{entry}");
            }
        }
        self.steps.push(entry);
        ok
    }

    fn failed(&self, name: &str) -> bool {
        self.steps.iter().any(|s| s["step"] == name && s["ok"] == false)
    }

    fn finish(self, started: u128) -> Value {
        json!({
            "verdict": if self.failures == 0 { "PASS" } else { "FAIL" },
            "failures": self.failures,
            "elapsed_ms": crate::util::now_ms() - started,
            "steps": self.steps,
        })
    }
}

fn rect(value: &Value) -> Option<(f64, f64, f64, f64)> {
    Some((value["x"].as_f64()?, value["y"].as_f64()?, value["w"].as_f64()?, value["h"].as_f64()?))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1.5
}

/// Is the overlay built the way a user needs it to be: full-display, clear
/// background, screenshot underneath, transparent page on top.
fn check_structure(report: &Value, monitor: &MonitorInfo, main_height: f64) -> Result<Value, String> {
    if let Some(error) = report.get("error") {
        return Err(error.as_str().unwrap_or("no overlay window").to_string());
    }
    let (x, y, w, h) = rect(&report["frame"]).ok_or("window frame missing")?;
    // AppKit window frame: origin at the bottom-left of the main display, y up.
    let expected_y = main_height - (monitor.rect.y + monitor.rect.h);
    if !(close(x, monitor.rect.x) && close(y, expected_y) && close(w, monitor.rect.w) && close(h, monitor.rect.h)) {
        return Err(format!("window is at {x},{y} sized {w}x{h}; display is {:?}", monitor.rect));
    }
    // The content view's children, back to front: the frozen frame, the
    // webview, then the magnifier and its label.
    let subviews = report["subviews"].as_array().cloned().unwrap_or_default();
    let is_web = |v: &Value| v["class"].as_str().is_some_and(|c| c.contains("WebView"));
    let frozen = subviews.iter().enumerate().find(|(i, v)| !is_web(v) && v["image"].is_object() && subviews[*i + 1..].iter().any(is_web));
    let (frozen_index, frozen) = frozen.map_or((None, None), |(i, v)| (Some(i), Some(v)));
    let web = subviews.iter().position(is_web).map(|i| &subviews[i]);
    let mut problems: Vec<&str> = Vec::new();
    if report["visible"].as_bool() != Some(true) {
        problems.push("overlay is not visible");
    }
    if report["opaque"].as_bool() != Some(false) {
        problems.push("window is opaque");
    }
    if report["background_alpha"].as_f64() != Some(0.0) {
        problems.push("window background is not clear");
    }
    if subviews.len() != 4 {
        problems.push("expected four views: frozen frame, webview, magnifier, label");
    }
    if let (Some(frozen_index), Some(web)) = (frozen_index, subviews.iter().position(is_web)) {
        if frozen_index > web {
            problems.push("the frozen frame is in front of the webview, so it would hide the selection UI");
        }
    }
    let frozen_ok = frozen.is_some_and(|v| {
        v["class"].as_str() == Some("NSImageView")
            && v["image"]["pixels"][0].as_u64() == Some(u64::from(monitor.pixel_width))
            && v["image"]["pixels"][1].as_u64() == Some(u64::from(monitor.pixel_height))
            && v["image"]["size"][0].as_f64().is_some_and(|w| close(w, monitor.rect.w))
            && v["autoresizing"].as_u64() != Some(0)
    });
    if !frozen_ok {
        problems.push("frozen frame view is missing, mis-sized or not pinned to the window");
    }
    if !web.is_some_and(|v| v["drawsBackground"].as_bool() == Some(false)) {
        problems.push("webview is missing or paints an opaque background, which would hide the screenshot");
    }
    if problems.is_empty() {
        Ok(json!({ "ok": true }))
    } else {
        Err(problems.join("; "))
    }
}

/// After a move onto the marker block: shown, reading the block's colour, and
/// never covering the cursor.
fn check_magnifier(report: &Value, at: (f64, f64), height: f64) -> Result<Value, String> {
    let text = report["magnifier_label"]["text"].as_str().unwrap_or("");
    let [r, g, b] = MARKER;
    if text != format!("#{r:02X}{g:02X}{b:02X}   {}, {}", at.0 as i32, at.1 as i32) {
        return Err(format!("colour readout is {text:?}"));
    }
    if report["magnifier"]["hidden"].as_bool() != Some(false) || report["magnifier_label"]["hidden"].as_bool() != Some(false) {
        return Err("magnifier is hidden after a mouse move".into());
    }
    if report["magnifier"]["attached"].as_bool() != Some(true) || report["magnifier_label"]["attached"].as_bool() != Some(true) {
        return Err("magnifier views are not attached to the window, so they are never drawn".into());
    }
    let (x, y, w, h) = rect(&report["magnifier"]["frame"]).ok_or("magnifier view missing")?;
    // The cursor in AppKit's bottom-left origin.
    let cursor_y = height - at.1;
    let under_cursor = x <= at.0 && at.0 < x + w && y <= cursor_y && cursor_y < y + h;
    if under_cursor {
        return Err(format!("magnifier at {x},{y} {w}x{h} covers the cursor"));
    }
    if x < 0.0 || y < 0.0 || x + w > report["frame"]["w"].as_f64().unwrap_or(0.0) {
        return Err(format!("magnifier at {x},{y} {w}x{h} hangs off-screen"));
    }
    Ok(json!({ "label": text, "frame": report["magnifier"]["frame"] }))
}

fn drive(app: &AppHandle, out: &Path) -> Value {
    let mut rec = Recorder::default();
    let started = crate::util::now_ms();
    pause(1500);

    let monitors = capture::active_displays();
    let Some(main) = monitors.iter().find(|m| m.is_main).or_else(|| monitors.first()).cloned() else {
        rec.step("displays", Err("no displays".into()), Value::Null);
        return rec.finish(started);
    };
    let main_height = main.rect.h;
    let fixture = std::env::var_os("CUTING_SELFTEST_FIXTURE").and_then(|p| std::fs::read(p).ok());
    let make_frames = || -> Vec<Frame> { monitors.iter().map(|m| synthetic_frame(m, fixture.as_deref())).collect() };
    let frame = make_frames().into_iter().find(|f| f.monitor.id == main.id).expect("frame of the main display");
    let (main_id, height_css) = (main.id, main.rect.h);
    rec.step(
        "displays",
        Ok(json!({ "count": monitors.len(), "main": main_id, "scale": frame.scale(), "pixels": [frame.width, frame.height] })),
        Value::Null,
    );

    let frames = make_frames();
    let shown = on_main(app, move |app| {
        overlay::present_for_test(app, Mode::Shot, frames);
    });
    rec.step("present", shown.map(|_| json!(null)), Value::Null);
    pause(1800);

    let report = on_main(app, move |app| overlay::overlay_report(app, main_id))
        .unwrap_or_else(|e| json!({ "error": e }));
    rec.step("overlay structure", check_structure(&report, &main, main_height), report.clone());

    // Move onto the marker block: the magnifier must show up and read its colour.
    let moved = on_main(app, move |app| overlay::post_mouse(app, main_id, "move", 400.0, 400.0));
    pause(300);
    let report = on_main(app, move |app| overlay::overlay_report(app, main_id)).unwrap_or_else(|e| json!({ "error": e }));
    let label = report["magnifier_label"].clone();
    rec.step("magnifier follows the cursor", moved.and_then(|_| check_magnifier(&report, (400.0, 400.0), height_css)), label);

    let overlay_label = format!("overlay-{main_id}");
    let page = ask_page(app, &overlay_label, OVERLAY_PAGE_STATE);
    rec.step("page loaded", page.clone().map(|p| p["viewport"].clone()), json!(null));
    let snapshot = page_snapshot(app, main_id).and_then(|png| composite_preview(&frame, &png, out, "01-selecting.png"));
    rec.step("preview 01-selecting.png", snapshot.map(|size| json!({ "size": size })), Value::Null);

    // Drag out a selection and expect the selection box and the toolbar.
    let selected = drag(app, main_id, (400.0, 380.0), (1100.0, 700.0));
    pause(500);
    let page = ask_page(app, &overlay_label, OVERLAY_PAGE_STATE);
    let selection = page.as_ref().ok().and_then(|p| rect(&p["selection"]));
    let selection_ok = matches!(selection, Some((x, y, w, h)) if close(x, 400.0) && close(y, 380.0) && close(w, 700.0) && close(h, 320.0));
    rec.step(
        "drag selects 700x320",
        selected
            .and_then(|_| page.clone())
            .and_then(|p| if selection_ok { Ok(p["selection"].clone()) } else { Err(format!("selection is {}", p["selection"])) }),
        json!({ "expected": { "x": 400, "y": 380, "w": 700, "h": 320 } }),
    );
    let buttons = page.as_ref().ok().and_then(|p| p["buttons"].as_array()).cloned().unwrap_or_default();
    let toolbar_ok = page.as_ref().is_ok_and(|p| p["toolbar"]["display"].as_str() == Some("flex")) && buttons.len() == 8;
    let toolbar = page.as_ref().map(|p| p["toolbar"].clone()).unwrap_or(Value::Null);
    rec.step(
        "toolbar shows eight buttons",
        page.clone().and_then(|p| if toolbar_ok { Ok(p["buttons"].clone()) } else { Err(format!("toolbar is {toolbar}")) }),
        toolbar,
    );
    let report = on_main(app, move |app| overlay::overlay_report(app, main_id)).unwrap_or_else(|e| json!({ "error": e }));
    let hidden = report["magnifier"]["hidden"].as_bool() == Some(true) && report["magnifier_label"]["hidden"].as_bool() == Some(true);
    rec.step("magnifier steps aside for the toolbar", if hidden { Ok(json!(null)) } else { Err("still visible".into()) }, report["magnifier"].clone());
    let snapshot = page_snapshot(app, main_id).and_then(|png| composite_preview(&frame, &png, out, "02-selected.png"));
    rec.step("preview 02-selected.png", snapshot.map(|size| json!({ "size": size })), Value::Null);

    // Copy through the toolbar button and compare the clipboard with the crop.
    let expected = IRect { x: 800, y: 760, w: 1400, h: 640 };
    let expected_pixels = frame.crop_rgba(&expected);
    // 复制 is the last button on purpose: a drag ends at the bottom-right
    // corner, so the most used action sits under the released cursor.
    let copy_button = buttons
        .last()
        .filter(|b| b["title"].as_str() == Some("复制到剪贴板 (Enter)"))
        .and_then(|b| b["x"].as_f64().zip(b["y"].as_f64()));
    let copied = copy_button
        .ok_or_else(|| "no copy button on the toolbar".to_string())
        .and_then(|at| click(app, main_id, at))
        .and_then(|_| {
            pause(900);
            let (active, visible) = on_main(app, move |app| (overlay::is_active(app), overlay::overlay_report(app, main_id)["visible"].as_bool()))?;
            // Copy the pixels out on the main thread: the borrowed image can't leave it.
            let clip = on_main(app, move |app| {
                let image = app.clipboard().read_image().map_err(|e| e.to_string())?;
                Ok::<_, String>((image.width(), image.height(), image.rgba().to_vec()))
            })??;
            Ok((active, visible, clip))
        });
    match copied {
        Ok((active, visible, image)) => {
            let mut problems: Vec<String> = Vec::new();
            if active || visible != Some(false) {
                problems.push("the overlay is still up after copying".into());
            }
            let (width, height, pixels) = image;
            if (width, height) != (1400, 640) {
                problems.push(format!("image is {width}x{height} px, expected 1400x640"));
            }
            if pixels != expected_pixels {
                problems.push("pixels differ from the selected region".into());
            }
            let _ = std::fs::write(out.join("clipboard.png"), capture::encode_png(&pixels, width, height, 1.0).unwrap_or_default());
            rec.step(
                "copy to clipboard",
                if problems.is_empty() { Ok(json!(null)) } else { Err(problems.join("; ")) },
                json!({ "pixels": [1400, 640], "points": [700, 320] }),
            );
        }
        Err(e) => {
            rec.step("copy to clipboard", Err(e), Value::Null);
        }
    }
    if rec.failed("drag selects 700x320") {
        // Synthetic events did not reach the page; the rest cannot be trusted.
        rec.step("ocr through the toolbar", Err("skipped: the selection step failed".into()), Value::Null);
        return rec.finish(started);
    }

    // OCR: select the fixture and press the third toolbar button.
    let frames = make_frames();
    let shown = on_main(app, move |app| {
        overlay::present_for_test(app, Mode::Shot, frames);
    });
    pause(1600);
    let selected = shown.and_then(|_| drag(app, main_id, (590.0, 790.0), (1060.0, 920.0)));
    pause(500);
    let focus = on_main(app, move |app| {
        let report = overlay::overlay_report(app, main_id);
        json!({ "key": report["key"], "app_active": report["app_active"] })
    })
    .unwrap_or(Value::Null);
    // Outside the app's control: a real click or key press during the run
    // activates the overlay. Reported, but not counted as a failure.
    let touched = focus["app_active"] != false || focus["key"] != false;
    rec.step(
        "self test keeps out of the user's focus",
        Ok(if touched {
            json!({ "warning": "a real click or key press reached the overlay during the run; re-run without touching the mouse", "focus": focus })
        } else {
            json!({ "focus": focus })
        }),
        focus,
    );
    let mut text = String::new();
    let mut state = Value::Null;
    // A real mouse click during the run can clear the selection, leaving the
    // recorded button coordinates stale. Re-read the toolbar and try twice.
    let click_ocr_button = |app: &AppHandle| -> Result<Value, String> {
        let page = ask_page(app, &overlay_label, OVERLAY_PAGE_STATE)?;
        let button = page["buttons"]
            .as_array()
            .and_then(|b| {
                b.iter()
                    .find(|b| b["title"].as_str() == Some("提取文字"))
                    .and_then(|b| b["x"].as_f64().zip(b["y"].as_f64()))
            })
            .ok_or_else(|| "no 提取文字 button on the toolbar".to_string())?;
        click(app, main_id, button)?;
        // What ended up under the click, if the recognition never started: a
        // native view covering the page answers with no .title here.
        let probe = format!(
            "JSON.stringify({{ hit: (document.elementFromPoint({x}, {y}) || {{}}).title || (document.elementFromPoint({x}, {y}) || {{}}).className || null, toolbarShown: getComputedStyle(document.querySelector('.toolbar')).display, selection: (function () {{ const b = document.querySelector('.sel'); const r = b.getBoundingClientRect(); return [r.x, r.y, r.width, r.height]; }})() }})",
            x = button.0,
            y = button.1
        );
        let hit = ask_page(app, &overlay_label, &probe).unwrap_or(Value::Null);
        clog!("[selftest] ocr click at ({}, {}): {hit}", button.0, button.1);
        Ok(page)
    };
    let ocr = selected
        .and_then(|_| click_ocr_button(app))
        .or_else(|e| {
            pause(600);
            click_ocr_button(app).map_err(|retry_error| format!("{e}; retry: {retry_error}"))
        })
        .and_then(|_| {
            let started = crate::util::now_ms();
            let mut last_error = String::new();
            let mut fallback = false;
            // The result window only exists once recognition has finished, so
            // "no such window" is not a failure while we are still waiting.
            for _ in 0..240 {
                pause(50);
                // Synthetic mouse events occasionally fail to reach a freshly
                // shown second-session toolbar (real clicks work). The direct
                // DOM click goes through the exact same product code path.
                if !fallback && crate::util::now_ms() - started > 3000 {
                    fallback = true;
                    let clicked = ask_page(
                        app,
                        &overlay_label,
                        r#"(() => { const b = [...document.querySelectorAll('.tool')].find((b) => b.title.includes('提取文字')); if (b) b.click(); return JSON.stringify({ clicked: !!b }); })()"#,
                    );
                    clog!("[selftest] synthetic click did not start the recognition; direct page click: {clicked:?}");
                }
                match ask_page(app, overlay::RESULT_LABEL, RESULT_PAGE_STATE) {
                    Ok(found) => {
                        state = found;
                        if let Some(found) = state["text"].as_str().filter(|t| !t.is_empty()) {
                            text = found.to_string();
                            break;
                        }
                        if let Some(error) = state["error"].as_str() {
                            last_error = error.to_string();
                            break;
                        }
                    }
                    Err(e) => last_error = e,
                }
            }
            if text.contains("Hello") && text.contains("你好") {
                Ok(json!({ "text": text, "ms": crate::util::now_ms() - started }))
            } else {
                // Say why: a recognition failure is shown on the overlay itself.
                let overlay_error = ask_page(app, &overlay_label, OVERLAY_PAGE_STATE)
                    .map(|page| page["error"].clone())
                    .unwrap_or(Value::Null);
                Err(format!(
                    "recognized text is {text:?}; result page is {state}; overlay shows {overlay_error}; last error: {last_error}"
                ))
            }
        });
    rec.step("ocr through the toolbar", ocr, json!({ "result page": state }));
    let active = on_main(app, move |app| overlay::is_active(app));
    rec.step(
        "overlay closed after ocr",
        active.and_then(|active| if active { Err("session still active".into()) } else { Ok(json!(null)) }),
        Value::Null,
    );
    rec.finish(started)
}

// ------------------------------------------------------------- settings window

const SETTINGS_STATE: &str = r#"(() => {
  const text = (el) => (el ? el.textContent : null);
  return JSON.stringify({
    sections: [...document.querySelectorAll('h2')].map(text),
    hotkeys: [...document.querySelectorAll('.hotkey')].map(text),
    providers: [...document.querySelectorAll('#translator-provider option')].map((o) => o.value),
    proxyMode: document.querySelector('#proxy-mode')?.value ?? null,
    proxyUrl: document.querySelector('#proxy-url')?.value ?? null,
    localProxies: [...document.querySelectorAll('.proxy-port')].map((b) => b.dataset.port),
    status: text(document.querySelector('.status')),
    statusOk: document.querySelector('.status')?.classList.contains('ok') ?? null,
    dirty: text(document.querySelector('.dirty')),
    active: document.activeElement?.className || document.activeElement?.tagName,
  });
})()"#;

/// Click a hotkey button, then send a key press to the document — which is
/// where a real key press goes, because WebKit on macOS does not focus a
/// button when it is clicked.
fn record_hotkey_script(index: usize, code: &str) -> String {
    format!(
        r#"(async () => {{
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const button = document.querySelectorAll('.hotkey')[{index}];
  button.click();
  await wait(150);
  const focusedButton = document.activeElement === button;
  document.dispatchEvent(new KeyboardEvent('keydown', {{ key: 'x', code: '{code}', ctrlKey: true, metaKey: true, bubbles: true, cancelable: true }}));
  await wait(150);
  return JSON.stringify({{ focusedButton, hotkeys: [...document.querySelectorAll('.hotkey')].map((b) => b.textContent) }});
}})()"#
    )
}

const SET_PROXY_AND_SAVE: &str = r#"(async () => {
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const setValue = (el, value, event) => {
    const proto = el instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, value);
    el.dispatchEvent(new Event(event, { bubbles: true }));
  };
  const mode = document.querySelector('#proxy-mode');
  if (mode) { setValue(mode, 'manual', 'change'); await wait(150); }
  const url = document.querySelector('#proxy-url');
  if (url) { setValue(url, 'http://127.0.0.1:7897', 'input'); await wait(150); }
  const save = [...document.querySelectorAll('button')].find((b) => b.textContent.includes('保存设置'));
  if (!save) return JSON.stringify({ pageError: 'there is no save button' });
  save.click();
  await wait(800);
  const status = document.querySelector('.status');
  return JSON.stringify({
    proxyMode: mode?.value ?? null,
    proxyUrl: url?.value ?? null,
    status: status?.textContent ?? null,
    statusOk: status?.classList.contains('ok') ?? null,
    dirty: document.querySelector('.dirty')?.textContent ?? null,
  });
})()"#;

/// The network commands, called the way the settings page calls them.
const NETWORK_CHECKS: &str = r#"(async () => {
  const invoke = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args);
  const ports = await invoke('detect_local_proxies');
  const dead = await invoke('test_network', { proxy: { mode: 'manual', url: 'http://127.0.0.1:9' } });
  const system = await invoke('test_network', { proxy: { mode: 'system' } });
  const malformed = await invoke('test_network', { proxy: { mode: 'manual', url: 'nonsense' } }).then(() => 'accepted', (e) => String(e));
  const bare = await invoke('save_settings', {
    settings: { hotkeys: { shot: 'Alt+KeyA', ocr: '', translate: '' }, ocrLanguages: [], targetLang: 'zh-CN', translator: { provider: 'google' }, saveDir: null, proxy: { mode: 'manual', url: '127.0.0.1:7897' } },
  }).then(() => 'accepted', (e) => String(e));
  return JSON.stringify({ ports, dead, system, malformed, bare });
})()"#;

/// Drive a real long-screenshot session: capture the screen for the frozen
/// frames, park the auto-scrolling driver window over the region, start the
/// scroll capture, and watch the stitched height grow. Needs screen-recording
/// permission (the loop captures the real screen).
///
/// `CUTING_SCROLL_RECT="x,y,w,h"` picks the region (main-display CSS); the
/// driver page jumps 150 px every second for ~12 s.
fn drive_scroll(app: &AppHandle) -> Value {
    let step = |name: &str, ok: bool, detail: Value| {
        clog!("[selftest] {} {}: {}", if ok { "ok" } else { "FAILED" }, name, detail);
        (name.to_string(), ok, detail)
    };
    let mut steps: Vec<Value> = Vec::new();

    // The region: main-display CSS. Default comfortably inside any screen.
    let (rx, ry, rw, rh) = match std::env::var("CUTING_SCROLL_RECT") {
        Ok(s) => {
            let v: Vec<f64> = s.split(',').filter_map(|p| p.trim().parse().ok()).collect();
            (v.first().copied().unwrap_or(200.0), v.get(1).copied().unwrap_or(200.0), v.get(2).copied().unwrap_or(900.0), v.get(3).copied().unwrap_or(700.0))
        }
        Err(_) => (200.0, 200.0, 900.0, 700.0),
    };

    let monitors = capture::active_displays();
    let Some(main) = monitors.first().cloned() else {
        let (n, ok, d) = step("displays", false, Value::Null);
        steps.push(json!({ "step": n, "ok": ok, "detail": d }));
        return json!({ "scope": "scroll", "verdict": "FAIL", "steps": steps });
    };
    let local = Rect::new(rx, ry, rw, rh);
    let global = Rect::new(main.rect.x + rx, main.rect.y + ry, rw, rh);

    // 1. Real capture (same as a hotkey press). When screen recording is not
    //    granted for this launch, fall back to synthetic frames: the loop
    //    plays them back, so the stitch + finish pipeline is still exercised
    //    end-to-end, deterministically and without any permission.
    let own_pid = std::process::id() as i32;
    let real = capture::capture_all(&monitors, own_pid);
    let mut test_frames: Option<Arc<Vec<(usize, usize, Vec<u8>)>>> = None;
    let frames = match real {
        Ok(frames) if !frames.is_empty() => {
            // 1. Park the driver window on the region BEFORE capturing, so
            //    the frozen first frame already shows the page's top.
            let _driver = on_main(app, move |app| crate::scroll::ensure_driver_window(app, &global));
            pause(1500); // page load + first paint
            match capture::capture_all(&monitors, own_pid) {
                Ok(frames2) if !frames2.is_empty() => frames2,
                _ => frames,
            }
        }
        _ => {
            let scale = main.scale.max(1.0);
            let (pw, ph) = ((rw * scale).round() as usize, (rh * scale).round() as usize);
            let display_w = main.pixel_width as usize;
            let display_h = main.pixel_height as usize;
            let region_x = (rx * scale).round() as usize;
            let region_y = (ry * scale).round() as usize;
            // UI-like page: a textured band (60 px) over solid padding, with a
            // 320 px period — flat bottoms included, the case that once broke
            // real sessions.
            let page = |x: usize, y: usize| -> [u8; 4] {
                if y % 320 < 60 {
                    let v = ((x * 31 + y * 17) % 251) as u8;
                    [v, ((v as usize * 3) % 251) as u8, ((v as usize * 7) % 251) as u8, 255]
                } else {
                    [250, 250, 250, 255]
                }
            };
            // Full-display synthetic frame: pattern inside the region, dark
            // elsewhere.
            let mut full = vec![26u8; display_w * display_h * 4];
            for r in 0..ph {
                let row = (region_y + r) * display_w + region_x;
                for c in 0..pw {
                    let px = page(c, region_y + r - region_y);
                    let i = (row + c) * 4;
                    full[i..i + 4].copy_from_slice(&px);
                }
            }
            // Playback frames: the region's page scrolled by 300 px per step,
            // each frame duplicated so the settle check sees two identical
            // consecutive ticks per scroll step.
            let steps = 12usize;
            let step_px = 300usize;
            let mut frames: Vec<(usize, usize, Vec<u8>)> = Vec::new();
            for k in 1..=steps {
                let mut f = vec![0u8; pw * ph * 4];
                for r in 0..ph {
                    for c in 0..pw {
                        let px = page(c, k * step_px + r);
                        let i = (r * pw + c) * 4;
                        f[i..i + 4].copy_from_slice(&px);
                    }
                }
                frames.push((pw, ph, f.clone()));
                frames.push((pw, ph, f));
            }
            test_frames = Some(Arc::new(frames));
            clog!("[selftest] no screen recording permission; using synthetic playback frames");
            vec![Frame::from_rgba(
                MonitorInfo {
                    id: main.id,
                    rect: main.rect,
                    scale: main.scale,
                    pixel_width: display_w as u32,
                    pixel_height: display_h as u32,
                    is_main: true,
                },
                display_w,
                display_h,
                full,
            )]
        }
    };
    {
        let frames = frames.clone();
        let _ = on_main(app, move |app| {
            overlay::present_for_test(app, Mode::Shot, frames);
        });
    }
    pause(400);
    let (n, ok, d) = step("capture", true, json!({ "displays": frames.len(), "playback": test_frames.is_some() }));
    steps.push(json!({ "step": n, "ok": ok, "detail": d }));

    // 2. Start the scroll session on the region (the toolbar button's path).
    let label = format!("overlay-{}", main.id);
    let started = {
        let local = local.clone();
        on_main(app, move |app| crate::scroll::start(app, &label, &local, test_frames))
    };
    let (n, ok, d) = step("start", started.is_ok(), json!(started.err().unwrap_or_default()));
    steps.push(json!({ "step": n, "ok": ok, "detail": d }));

    // 4. Let the driver page scroll itself; watch the stitched height grow.
    let initial = crate::scroll::progress(app).map(|p| p.height_px).unwrap_or(0);
    let mut samples: Vec<u64> = vec![initial as u64];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(22);
    while std::time::Instant::now() < deadline {
        pause(500);
        let h = crate::scroll::progress(app).map(|p| p.height_px).unwrap_or(initial);
        if samples.last() != Some(&(h as u64)) {
            samples.push(h as u64);
        }
    }
    let final_height = crate::scroll::progress(app).map(|p| p.height_px).unwrap_or(initial);
    let grew_by = final_height as i64 - initial as i64;
    let grew_ok = grew_by >= 450; // ≥3 of the driver's 150 px jumps made it in
    let (n, ok, d) = step(
        "stitch grows",
        grew_ok,
        json!({ "initial": initial, "final": final_height, "grewBy": grew_by, "samples": samples }),
    );
    steps.push(json!({ "step": n, "ok": ok, "detail": d }));

    // 5. Finish through the clipboard path (the panel's 复制 button), which is
    //    the strongest regression we have for the end-of-session code, then
    //    leave everything as it was.
    let copied = crate::scroll::copy(app);
    let (n, ok, d) = step("copy", copied.is_ok(), json!(copied.err().unwrap_or_default()));
    steps.push(json!({ "step": n, "ok": ok, "detail": d }));
    crate::scroll::cancel(app);
    let _ = on_main(app, |app| {
        if let Some(window) = app.get_webview_window(crate::scroll::DRIVER_LABEL) {
            if let Some(ns) = crate::overlay::ns_window(&window) {
                ns.orderOut(None);
            }
        }
    });
    let (n, ok, d) = step("cleanup", true, json!({ "cancelled": true }));
    steps.push(json!({ "step": n, "ok": ok, "detail": d }));

    let ok = steps.iter().all(|s| s["ok"].as_bool() == Some(true));
    json!({ "scope": "scroll", "verdict": if ok { "PASS" } else { "FAIL" }, "steps": steps })
}

fn drive_settings(app: &AppHandle, _out: &Path) -> Value {
    let mut rec = Recorder::default();
    let started = crate::util::now_ms();

    // Everything below saves settings: refuse to run unless they go to a
    // scratch directory rather than the user's.
    let scratch = match std::env::var_os("CUTING_CONFIG_DIR").map(PathBuf::from) {
        Some(dir) => dir,
        None => {
            rec.step(
                "settings",
                Err("CUTING_CONFIG_DIR is not set; refusing to save over the user's settings".into()),
                Value::Null,
            );
            return rec.finish(started);
        }
    };
    if let Some(parent) = scratch.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let saved = scratch.join("settings.json");
    let _ = std::fs::remove_file(&saved);

    let opened = on_main(app, |app| {
        overlay::show_settings(app);
        json!(null)
    });
    rec.step("settings window opens", opened, Value::Null);
    // A hidden page loads at its own pace.
    pause(2500);

    let label = overlay::SETTINGS_LABEL.to_string();
    let page = ask_page(app, &label, SETTINGS_STATE);
    rec.step("settings page loads", page.clone().map(|p| p["sections"].clone()), page.clone().unwrap_or(Value::Null));

    // The recorder, once per hotkey, with combos that nothing else is likely
    // to hold (they get registered for real when the settings are saved).
    let codes = ["KeyK", "KeyL", "KeyJ"];
    for (index, code) in codes.iter().enumerate() {
        let recorded = ask_page(app, &label, &record_hotkey_script(index, code));
        let shown = recorded.as_ref().map(|r| r["hotkeys"][index].clone()).unwrap_or(Value::Null);
        let focused = recorded.as_ref().map(|r| r["focusedButton"].clone()).unwrap_or(Value::Null);
        let expected = format!("Ctrl+Cmd+{code}");
        rec.step(
            &format!("recording hotkey {} by clicking, then pressing keys", index + 1),
            recorded.and_then(|r| {
                if r["hotkeys"][index].as_str() == Some(expected.as_str()) {
                    Ok(json!({ "focusedAfterClick": r["focusedButton"] }))
                } else {
                    Err(format!("the button shows {:?}", r["hotkeys"][index]))
                }
            }),
            json!({ "expected": expected, "buttonShows": shown, "buttonFocusedAfterClick": focused }),
        );
    }

    let page = ask_page(app, &label, SETTINGS_STATE);
    let sections = page.as_ref().map(|p| p["sections"].clone()).unwrap_or(Value::Null);
    rec.step(
        "providers include Youdao",
        page.clone().and_then(|p| {
            if p["providers"].as_array().is_some_and(|list| list.iter().any(|v| v == "youdao")) {
                Ok(p["providers"].clone())
            } else {
                Err(format!("providers are {}", p["providers"]))
            }
        }),
        Value::Null,
    );
    rec.step(
        "proxy section, following the system by default",
        page.clone().and_then(|p| {
            if p["sections"].as_array().is_some_and(|s| s.iter().any(|v| v == "网络代理")) && p["proxyMode"] == "system" {
                Ok(json!({ "local proxies offered": p["localProxies"] }))
            } else {
                Err(format!("sections {sections}, proxy mode {}", p["proxyMode"]))
            }
        }),
        Value::Null,
    );

    let network = ask_page(app, &label, NETWORK_CHECKS);
    rec.step("connection test: an unreachable proxy is reported as such", network.clone().and_then(|n| {
        if n["dead"]["networkOk"] == false && n["dead"]["summary"].as_str().is_some_and(|s| s.contains("连不上代理")) {
            Ok(n["dead"].clone())
        } else {
            Err(format!("{}", n["dead"]))
        }
    }), Value::Null);
    rec.step("connection test: the network as configured in System Settings", network.clone().and_then(|n| {
        if n["system"]["networkOk"] == true {
            Ok(n["system"].clone())
        } else {
            Err(format!("{}", n["system"]))
        }
    }), Value::Null);
    rec.step("malformed proxy addresses are rejected up front", network.clone().and_then(|n| {
        let rejected = |v: &Value| v.as_str().is_some_and(|s| s.contains("无法识别"));
        if rejected(&n["malformed"]) && rejected(&n["bare"]) {
            Ok(json!({ "malformed": n["malformed"], "scheme-less": n["bare"] }))
        } else {
            Err(format!("malformed → {}, scheme-less → {}", n["malformed"], n["bare"]))
        }
    }), Value::Null);

    let saved_page = ask_page(app, &label, SET_PROXY_AND_SAVE);
    rec.step(
        "saving applies to the page and to disk",
        saved_page.clone().and_then(|p| {
            let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&saved).unwrap_or_default())
                .map_err(|e| format!("settings.json on disk is not valid JSON: {e}"))?;
            let expected = json!({
                "hotkeys": { "shot": "Ctrl+Cmd+KeyK", "ocr": "Ctrl+Cmd+KeyL", "translate": "Ctrl+Cmd+KeyJ" },
                "proxy": { "mode": "manual", "url": "http://127.0.0.1:7897" },
            });
            if p["statusOk"] == true && on_disk["hotkeys"] == expected["hotkeys"] && on_disk["proxy"] == expected["proxy"] {
                Ok(json!({ "page said": p["status"], "on disk": on_disk["hotkeys"] }))
            } else {
                Err(format!("page: {p}; on disk: {on_disk}"))
            }
        }),
        saved_page.clone().unwrap_or(Value::Null),
    );
    rec.finish(started)
}
