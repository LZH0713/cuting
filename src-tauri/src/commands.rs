//! IPC commands called from the webviews.
//!
//! Sync commands run on the main thread, which is where AppKit and the
//! pasteboard want to be touched. Anything slow (OCR, PNG encoding, network)
//! lives in an `async` command or is moved to a blocking thread.

use base64::Engine as _;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGImage;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::DialogExt;

use crate::capture::{self, Frame, IRect};
use crate::clipboard;
use crate::clog;
use crate::geo::Rect;
use crate::ocr;
use crate::overlay::{self, Mode};
use crate::proxy;
use crate::scroll;
use crate::settings;
use crate::translate;
use crate::tray;
use crate::util::lock;
use crate::AppState;

/// The frame to crop from, plus the selection in its pixels.
///
/// A selection inside one display crops from that display's native frame
/// (full Retina resolution); one spanning displays crops from the composited
/// desktop. Also returns the display the drag started on (for window
/// placement) and its rect.
fn selected_frame(
    app: &AppHandle,
    label: &str,
    rect: &Rect,
) -> Result<(u32, Frame, IRect, Rect), String> {
    let monitor_id = overlay::monitor_id_from_label(label).ok_or("不是截图覆盖层窗口")?;
    let display = overlay::frame(app, monitor_id).ok_or("截图会话已结束，请重新截图")?;
    let global = overlay::desktop_rect(app, label, rect).ok_or("截图会话已结束，请重新截图")?;
    let desktop = overlay::desktop_frame(app).ok_or("截图会话已结束，请重新截图")?;
    let screen = display.monitor.rect;
    if display.monitor.rect.contains_rect(&global) {
        let px = display.to_pixels(rect).ok_or("选区太小，请重新框选")?;
        Ok((monitor_id, display, px, screen))
    } else {
        let screen = desktop.monitor.rect;
        let px = desktop.to_pixels(&global).ok_or("选区太小，请重新框选")?;
        Ok((monitor_id, desktop, px, screen))
    }
}

/// Relay a page's selection rectangle (desktop coordinates) to every overlay,
/// so each draws only the part that falls on its own display.
#[tauri::command]
pub fn broadcast_selection(app: AppHandle, rect: Option<Rect>, active: String) {
    overlay::broadcast_selection(&app, rect, &active);
}

/// The beginning of a drag: announce it (this clears the box on other screens).
#[tauri::command]
pub fn selection_start(app: AppHandle, label: String, x: f64, y: f64) {
    overlay::broadcast_selection(&app, None, &label);
    let _ = (app, x, y);
}

/// The drag moved: convert to desktop coordinates and broadcast.
#[tauri::command]
pub fn selection_move(app: AppHandle, label: String, x: f64, y: f64) {
    if let Some(monitor_id) = overlay::monitor_id_from_label(&label) {
        if let Some(origin) = overlay::frame(&app, monitor_id).map(|f| f.monitor.rect) {
            let rect = Rect::new(origin.x + x, origin.y + y, 0.0, 0.0);
            overlay::broadcast_selection(&app, Some(rect), &label);
        }
    }
}

/// The drag ended: broadcast the final rectangle.
#[tauri::command]
pub fn selection_end(app: AppHandle, label: String, x: f64, y: f64) {
    if let Some(monitor_id) = overlay::monitor_id_from_label(&label) {
        if let Some(origin) = overlay::frame(&app, monitor_id).map(|f| f.monitor.rect) {
            let rect = Rect::new(origin.x + x, origin.y + y, 0.0, 0.0);
            overlay::broadcast_selection(&app, Some(rect), &label);
        }
    }
}

// ------------------------------------------------------------------- overlay

/// State for an overlay page. Pulled on mount because a `capture-start` event
/// sent before the page finished loading would be lost.
#[tauri::command]
pub fn overlay_start_payload(window: WebviewWindow, app: AppHandle) -> Option<overlay::StartPayload> {
    overlay::monitor_id_from_label(window.label()).and_then(|id| overlay::start_payload(&app, id))
}

/// Which overlay phase the page is in; the native magnifier only follows the
/// cursor during selection. Cheap: a few calls per session, not per move.
#[tauri::command]
pub fn set_overlay_phase(app: AppHandle, phase: String) {
    overlay::set_magnifier_phase(&app, phase == "select");
}

/// Close the overlay. `reason` says what triggered it (Esc, the close button,
/// a right click, …); it only goes to the log, but that is exactly what makes
/// "why did my screenshot disappear" answerable later.
#[tauri::command]
pub fn cancel_capture(app: AppHandle, reason: Option<String>) {
    clog!("cancel capture ({})", reason.as_deref().unwrap_or("unspecified"));
    overlay::cancel(&app);
}

#[tauri::command]
pub fn copy_region(window: WebviewWindow, app: AppHandle, rect: Rect, annotation: Option<String>) -> Result<Value, String> {
    clog!("copy {rect:?}{}", if annotation.is_some() { " (annotated)" } else { "" });
    let (_, frame, px, _) = selected_frame(&app, window.label(), &rect)?;
    let scale = frame.scale();
    let image = match annotated_crop(&frame, &px, annotation.as_deref())? {
        Some(rgba) => capture::rgba_to_image(&rgba, px.w as u32, px.h as u32).ok_or("合成标注失败")?,
        None => frame.crop_image(&px).ok_or("裁剪截图失败")?,
    };
    // Clipboard first: if it fails the overlay stays up and the user can retry.
    // The image keeps its on-screen size, so a Retina shot doesn't paste huge.
    clipboard::write_image(&image, px.w as f64 / scale, px.h as f64 / scale)?;
    let previous = overlay::end_session(&app);
    overlay::restore(&app, previous);
    clog!("copied {}x{} px ({} pt) to the clipboard", px.w, px.h, px.w as f64 / scale);
    Ok(json!({ "ok": true, "width": px.w, "height": px.h }))
}

/// The selection's pixels with the web annotations composited in, or `None`
/// when the selection is clean. The layer comes from the overlay page as a
/// base64 PNG sized exactly like the crop; anything else is a bug to report.
fn annotated_crop(frame: &Frame, px: &IRect, annotation: Option<&str>) -> Result<Option<Vec<u8>>, String> {
    let Some(encoded) = annotation else { return Ok(None) };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| format!("标注数据无法解码：{e}"))?;
    let (w, h, layer) = capture::decode_png(&bytes)?;
    if (w as i32, h as i32) != (px.w, px.h) {
        return Err(format!("标注图层尺寸不匹配：{w}x{h}，应为 {}x{}", px.w, px.h));
    }
    Ok(Some(capture::composite_overlay(&frame.crop_rgba(px), &layer)?))
}

#[tauri::command]
pub async fn save_region(window: WebviewWindow, app: AppHandle, rect: Rect, annotation: Option<String>) -> Result<Value, String> {
    clog!("save {rect:?}{}", if annotation.is_some() { " (annotated)" } else { "" });
    let (_, frame, px, _) = selected_frame(&app, window.label(), &rect)?;
    let (width, height, scale) = (px.w as u32, px.h as u32, frame.scale());
    let rgba = match annotated_crop(&frame, &px, annotation.as_deref())? {
        Some(annotated) => annotated,
        None => frame.crop_rgba(&px),
    };
    let png = tauri::async_runtime::spawn_blocking(move || capture::encode_png(&rgba, width, height, scale))
        .await
        .map_err(|e| e.to_string())??;
    let previous = overlay::end_session(&app);
    spawn_save_dialog(&app, png, format!("截图 {}.png", crate::util::file_stamp()), previous);
    Ok(json!({ "ok": true }))
}

/// Show the save panel for an already-encoded PNG on the main thread, writing
/// the file once the user picks a spot. `previous` is the app to hand focus
/// back to after the panel closes.
pub(crate) fn spawn_save_dialog(app: &AppHandle, png: Vec<u8>, default_name: String, previous: Option<i32>) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let dir = lock(&handle.state::<AppState>().settings)
            .save_dir
            .clone()
            .filter(|d| !d.trim().is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(settings::default_save_dir);
        let restore_handle = handle.clone();
        handle
            .dialog()
            .file()
            .set_directory(dir)
            .set_file_name(default_name)
            .add_filter("PNG 图片", &["png"])
            .save_file(move |path| {
                overlay::restore(&restore_handle, previous);
                let Some(path) = path.and_then(|p| p.into_path().ok()) else { return };
                std::thread::spawn(move || match std::fs::write(&path, &png) {
                    Ok(()) => clog!("saved {}", path.display()),
                    Err(e) => clog!("save to {} failed: {e}", path.display()),
                });
            });
    });
}

/// The selection's pixels as a PNG data URL, for the mosaic tool: it needs the
/// real pixels under the selection to pixelate them.
#[tauri::command]
pub async fn region_pixels(window: WebviewWindow, app: AppHandle, rect: Rect) -> Result<String, String> {
    let (_, frame, px, _) = selected_frame(&app, window.label(), &rect)?;
    let scale = frame.scale();
    // A full-res crop encode can reach tens of megabytes — keep it off the
    // async workers.
    let png = tauri::async_runtime::spawn_blocking(move || {
        let rgba = frame.crop_rgba(&px);
        capture::encode_png(&rgba, px.w as u32, px.h as u32, scale)
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png)))
}

// --------------------------------------------------------------- ocr / translate

/// Everything the result payload needs except the recognition itself: the
/// preview data URL and the crop's pixel size. Built once so the accurate
/// pass and the fast fallback can share it.
struct Prepared {
    width: u32,
    height: u32,
    image_url: String,
}

/// One recognition attempt on the blocking pool, timed. The caller layers the
/// staged timeouts and the fast fallback around this.
async fn recognize_pass(
    image: CFRetained<CGImage>,
    langs: Vec<String>,
    level: ocr::Level,
) -> Result<(Vec<ocr::OcrLine>, u128), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let started = crate::util::now_ms();
        let lines = match level {
            ocr::Level::Accurate => ocr::recognize(&image, &langs)?,
            ocr::Level::Fast => ocr::recognize_fast(&image, &langs)?,
        };
        let elapsed = crate::util::now_ms() - started;
        clog!("ocr: {} line(s) in {elapsed} ms ({level:?})", lines.len());
        Ok((lines, elapsed))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Recognize the selection and show it in the result window. `action` decides
/// whether the result window also offers a translation right away.
#[tauri::command]
pub async fn ocr_region(
    window: WebviewWindow,
    app: AppHandle,
    state: State<'_, AppState>,
    rect: Rect,
    action: String,
    annotation: Option<String>,
) -> Result<Value, String> {
    clog!("ocr_region invoked from {}", window.label());
    let (monitor_id, frame, px, screen_rect) = selected_frame(&app, window.label(), &rect)?;
    let session_id = overlay::session_id(&app).ok_or("截图会话已结束，请重新截图")?;
    let langs = lock(&state.settings).ocr_languages.clone();
    let mode = if action == "translate" { "translate" } else { "ocr" };
    clog!("ocr region {rect:?} (action {action}, session {session_id})");
    let image = frame.crop_image(&px).ok_or("裁剪截图失败")?;

    // The overlay stays up (with the busy spinner) until recognition succeeds;
    // on failure the user can press Esc or redraw the selection.
    //
    // The system Vision pipeline can hang for tens of seconds — or forever:
    // its accurate level waits on the ANE compiler service, which wedges after
    // an OS update (seen on this very machine). `performRequests` cannot be
    // cancelled, so a hung worker thread is abandoned, and `ocr::mark_hang`
    // keeps later requests off that path. The wait is staged: at 20 s the user
    // learns recognition is slow, at 40 s the accurate pass is declared hung
    // and the fast level — which never touches the ANE — takes over.
    let prep = tauri::async_runtime::spawn_blocking(move || -> Result<Prepared, String> {
        // Recognition reads the clean pixels (annotations would confuse it),
        // but the preview shows what the user drew. Both live here in the
        // blocking pool: decoding and compositing a large layer means tens of
        // megabytes of pixel work, too heavy for a tokio worker.
        let annotated = annotated_crop(&frame, &px, annotation.as_deref())?;
        // Preview is capped at ~1200 px on its long side to keep the data URL small.
        let pixels = annotated.unwrap_or_else(|| frame.crop_rgba(&px));
        let factor = (px.w.max(px.h) as u32 + 1199) / 1200;
        let (pw, ph, preview) = capture::downscale_rgba(&pixels, px.w as u32, px.h as u32, factor);
        let png = capture::encode_png(&preview, pw, ph, 1.0)?;
        Ok(Prepared {
            width: px.w as u32,
            height: px.h as u32,
            image_url: format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(png)
            ),
        })
    })
    .await
    .map_err(|e| e.to_string())??;

    const HUNG_MSG: &str = "文字识别超时：系统的识别服务暂时没有响应，快速识别也失败了（macOS 升级后的偶发问题）。请按 Esc 关闭后稍等一分钟再试；反复出现时重启电脑可恢复。";
    const SLOW_AFTER: std::time::Duration = std::time::Duration::from_secs(20);
    const HANG_AFTER: std::time::Duration = std::time::Duration::from_secs(40);
    const FAST_AFTER: std::time::Duration = std::time::Duration::from_secs(15);

    // After a recent hang the accurate pass would just hang again; recognize
    // at the fast level right away (slightly lower accuracy, instantly back).
    let outcome: Result<(Vec<ocr::OcrLine>, u128, bool), String> = if ocr::cooling_down() {
        clog!("ocr_region: system pipeline recently hung; recognizing at the fast level");
        let pass = recognize_pass(image, langs, ocr::Level::Fast);
        match tokio::time::timeout(FAST_AFTER, pass).await {
            Ok(joined) => joined.map(|(lines, elapsed)| (lines, elapsed, true)),
            Err(_) => {
                ocr::mark_hang();
                Err(HUNG_MSG.to_string())
            }
        }
    } else {
        // The accurate pass consumes these; the fallback needs its own copies
        // (a `CFRetained` clone is just a retain, and the language list is tiny).
        let mut pass = Box::pin(recognize_pass(image.clone(), langs.clone(), ocr::Level::Accurate));
        match tokio::time::timeout(SLOW_AFTER, pass.as_mut()).await {
            Ok(joined) => joined.map(|(lines, elapsed)| (lines, elapsed, false)),
            Err(_) => {
                clog!("ocr_region: accurate pass slower than 20 s; system pipeline degraded");
                let _ = app.emit_to(window.label(), "ocr-slow", ());
                match tokio::time::timeout(HANG_AFTER - SLOW_AFTER, pass.as_mut()).await {
                    Ok(joined) => joined.map(|(lines, elapsed)| (lines, elapsed, false)),
                    Err(_) => {
                        ocr::mark_hang();
                        clog!("ocr_region: accurate pass hung past 40 s; retrying at the fast level");
                        let fast = recognize_pass(image, langs, ocr::Level::Fast);
                        match tokio::time::timeout(FAST_AFTER, fast).await {
                            Ok(joined) => joined.map(|(lines, elapsed)| (lines, elapsed, true)),
                            Err(_) => {
                                ocr::mark_hang();
                                Err(HUNG_MSG.to_string())
                            }
                        }
                    }
                }
            }
        }
    };

    let (lines, elapsed, degraded) = outcome?;
    let text = ocr::lines_to_text(&lines);
    let mut payload = json!({
        "mode": mode,
        "image": prep.image_url,
        "text": text,
        "lines": lines,
        "elapsedMs": elapsed,
        "width": prep.width,
        "height": prep.height,
    });
    if degraded {
        payload["degraded"] = json!(true);
        clog!("ocr_region: result served by the fast fallback");
    }
    if overlay::session_id(&app) != Some(session_id) {
        // The user pressed Esc while recognition was running: don't pop a
        // result window on top of what they went back to.
        clog!("recognition finished after the session ended; result dropped");
        return Ok(json!({ "ok": false, "cancelled": true }));
    }
    overlay::end_session(&app);
    overlay::show_result(&app, payload, Some((monitor_id, screen_rect)));
    Ok(json!({ "ok": true }))
}

#[tauri::command]
pub async fn translate_text(
    state: State<'_, AppState>,
    text: String,
    target: Option<String>,
) -> Result<translate::Translation, String> {
    let settings = lock(&state.settings).clone();
    let target = target.filter(|t| !t.trim().is_empty()).unwrap_or(settings.target_lang);
    let client = proxy::client(&settings.proxy)?;
    translate::translate(&client, &settings.translator, &text, &target, None).await
}

/// What the release feed offers, if anything.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub notes: Option<String>,
    pub current_version: String,
}

/// Ask the GitHub release feed whether a newer version exists.
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let update = app
        .updater()
        .map_err(|e| format!("更新组件不可用：{e}"))?
        .check()
        .await
        .map_err(|e| format!("检查更新失败：{e}"))?;
    Ok(update.map(|u| UpdateInfo {
        version: u.version.clone(),
        notes: u.body.clone(),
        current_version: u.current_version.clone(),
    }))
}

/// Download, install and restart. Only called from the settings window.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| format!("更新组件不可用：{e}"))?;
    let update = updater
        .check()
        .await
        .map_err(|e| format!("检查更新失败：{e}"))?
        .ok_or_else(|| "当前已是最新版本".to_string())?;
    clog!("downloading update {}", update.version);
    update
        .download_and_install(|received, total| {
            if let Some(total) = total {
                clog!("downloaded {received} / {total} bytes");
            }
        }, || {})
        .await
        .map_err(|e| format!("下载安装失败：{e}"))?;
    clog!("update {} installed, restarting", update.version);
    app.restart()
}

/// Try a proxy configuration the way a translation would use it. Takes the
/// settings as they are in the form, so it works before saving.
#[tauri::command]
pub async fn test_network(proxy: proxy::ProxyConfig) -> Result<proxy::TestReport, String> {
    proxy::test(&proxy).await
}

/// Local ports that look like a proxy, to spare the user from typing one.
#[tauri::command]
pub async fn detect_local_proxies() -> Vec<u16> {
    tauri::async_runtime::spawn_blocking(proxy::detect_local_proxies).await.unwrap_or_default()
}

// -------------------------------------------------------------------- settings

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LangInfo {
    pub code: &'static str,
    pub name: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub settings: settings::Settings,
    pub langs: Vec<LangInfo>,
    pub ocr_languages: Vec<LangInfo>,
    pub screen_capture_allowed: bool,
    pub default_save_dir: String,
    pub log_file: String,
    pub version: &'static str,
}

#[tauri::command]
pub fn get_app_info(state: State<'_, AppState>) -> AppInfo {
    AppInfo {
        settings: lock(&state.settings).clone(),
        langs: translate::LANGS.iter().map(|l| LangInfo { code: l.code, name: l.zh }).collect(),
        ocr_languages: ocr::OCR_LANGUAGES.iter().map(|(code, name)| LangInfo { code, name }).collect(),
        screen_capture_allowed: capture::screen_capture_allowed(),
        default_save_dir: settings::default_save_dir().to_string_lossy().into_owned(),
        log_file: crate::util::log_file_path().to_string_lossy().into_owned(),
        version: env!("CARGO_PKG_VERSION"),
    }
}

/// Keys that type a character when pressed on their own.
fn types_on_its_own(code: &tauri_plugin_global_shortcut::Code) -> bool {
    // `Code` has explicit discriminants, so its names are matched as text.
    let name = format!("{code:?}");
    let one = |prefix: &str, is: fn(u8) -> bool| name.strip_prefix(prefix).is_some_and(|rest| rest.len() == 1 && rest.bytes().all(is));
    one("Key", |b| b.is_ascii_uppercase())
        || one("Digit", |b| b.is_ascii_digit())
        || one("Numpad", |b| b.is_ascii_digit())
        || matches!(
            name.as_str(),
            "Space" | "Tab" | "Enter" | "NumpadEnter" | "Comma" | "Period" | "Slash" | "Minus" | "Equal" | "Semicolon" | "Quote"
                | "Backslash" | "Backquote" | "BracketLeft" | "BracketRight" | "IntlRo" | "IntlYen"
        )
}

/// Reject a hotkey we can't register before it reaches the settings file.
fn validate_hotkey(combo: &str) -> Result<(), String> {
    let shortcut = combo
        .trim()
        .parse::<tauri_plugin_global_shortcut::Shortcut>()
        .map_err(|_| format!("无法识别快捷键「{combo}」"))?;
    // A bare letter or digit would be taken from every other app while typing.
    if shortcut.mods.is_empty() && types_on_its_own(&shortcut.key) {
        return Err(format!("快捷键「{combo}」需要至少一个修饰键（⌘ ⌥ ⌃ ⇧），否则会影响日常打字"));
    }
    Ok(())
}

#[tauri::command]
pub fn save_settings(app: AppHandle, state: State<'_, AppState>, settings: settings::Settings) -> Result<(), String> {
    settings.proxy.validate()?;
    let combos = [&settings.hotkeys.shot, &settings.hotkeys.ocr, &settings.hotkeys.translate];
    for combo in combos.iter().filter(|c| !c.trim().is_empty()) {
        validate_hotkey(combo)?;
    }
    clog!(
        "save settings: hotkeys {:?}, proxy {}, translator {}",
        settings.hotkeys,
        serde_json::to_string(&settings.proxy).unwrap_or_default(),
        settings.translator.name()
    );
    let (a, b, c) = (combos[0].trim(), combos[1].trim(), combos[2].trim());
    let duplicated = (!a.is_empty() && (a == b || a == c)) || (!b.is_empty() && b == c);
    if duplicated {
        return Err("三个功能不能使用相同的快捷键".to_string());
    }
    settings::save_to_disk(&app, &settings)?;
    *lock(&state.settings) = settings;
    // A bad binding must not survive on disk only to fail silently next launch.
    if let Err(e) = overlay::register_hotkeys(&app) {
        clog!("settings saved, but hotkeys were not applied: {e}");
        return Err(e);
    }
    clog!("settings saved and applied");
    if let Err(e) = tray::refresh(&app) {
        clog!("{e}");
    }
    Ok(())
}

#[tauri::command]
pub async fn pick_save_dir(app: AppHandle, current: Option<String>) -> Option<String> {
    let mut builder = app.dialog().file();
    if let Some(dir) = current.filter(|d| !d.trim().is_empty()) {
        builder = builder.set_directory(dir);
    }
    builder
        .blocking_pick_folder()
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned())
}

// ------------------------------------------------------------------------ misc

/// Hand the next result to the result window. Push-based (`show-result` event)
/// and pull-based (this command on mount) so a cold window never misses one.
#[tauri::command]
pub fn take_result(app: AppHandle) -> Option<Value> {
    lock(&app.state::<AppState>().pending_result).take()
}

#[tauri::command]
pub fn open_settings_window(app: AppHandle) {
    overlay::show_settings(&app);
}

#[tauri::command]
pub fn screen_capture_allowed() -> bool {
    capture::screen_capture_allowed()
}

#[tauri::command]
pub fn open_screen_settings() {
    overlay::open_screen_settings();
}

#[tauri::command]
pub fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn copy_text(app: AppHandle, text: String) -> Result<(), String> {
    app.clipboard().write_text(&text).map_err(|e| format!("写入剪贴板失败：{e}"))
}

#[tauri::command]
pub fn start_capture(app: AppHandle, mode: String) {
    let mode = match mode.as_str() {
        "ocr" => Mode::Ocr,
        "translate" => Mode::Translate,
        _ => Mode::Shot,
    };
    overlay::start(&app, mode);
}

// ---------------------------------------------------------------- long screenshot

/// Enter scroll-capture mode from the overlay: the region becomes the first
/// frame, the overlays come down, and the floating outline + control panel go up.
#[tauri::command]
pub fn scroll_capture_start(window: WebviewWindow, app: AppHandle, rect: Rect) -> Result<Value, String> {
    scroll::start(&app, window.label(), &rect, None)?;
    Ok(json!({ "ok": true }))
}

/// The panel's live state, for a page that loaded after the last event.
#[tauri::command]
pub fn scroll_state(app: AppHandle) -> Option<scroll::Progress> {
    scroll::progress(&app)
}

/// Finish: encode the stitched image and show the save dialog.
#[tauri::command]
pub async fn scroll_capture_save(app: AppHandle) -> Result<Value, String> {
    scroll::save(&app).await?;
    Ok(json!({ "ok": true }))
}

/// Finish and copy the stitched image to the clipboard.
#[tauri::command]
pub fn scroll_capture_copy(app: AppHandle) -> Result<Value, String> {
    let (width, height) = scroll::copy(&app)?;
    Ok(json!({ "ok": true, "width": width, "height": height }))
}

/// Stop and discard.
#[tauri::command]
pub fn scroll_capture_cancel(app: AppHandle, reason: Option<String>) {
    clog!("scroll capture cancel ({})", reason.as_deref().unwrap_or("unspecified"));
    scroll::cancel(&app);
}

/// Frontend errors land in the same log file as backend ones.
#[tauri::command]
pub fn log_message(message: String) {
    clog!("[ui] {message}");
}

/// Overlay pages beat every ~1.5 s (in-memory, no IO). The overlay session
/// watches for fresh beats after presenting; a page that never beats is dead
/// (its webview wedged at startup) and gets recreated.
#[tauri::command]
pub fn page_heartbeat(app: AppHandle, label: String) {
    let now = crate::util::now_ms() as u64;
    lock(&app.state::<AppState>().page_heartbeats).insert(label, now);
}

#[cfg(test)]
mod tests {
    use super::validate_hotkey;

    #[test]
    fn hotkeys_recorded_by_the_settings_page_are_accepted() {
        // The formats the recorder produces, with every modifier and none of
        // the ones that would break typing.
        for combo in ["Alt+KeyA", "Alt+Shift+KeyA", "Ctrl+Cmd+KeyK", "Shift+Cmd+Digit1", "Cmd+Space", "F1", "F12", "Ctrl+F5"] {
            assert!(validate_hotkey(combo).is_ok(), "{combo}");
        }
    }

    #[test]
    fn hotkeys_that_swallow_typing_are_refused() {
        for combo in ["KeyA", "a", "Digit1", "1", "Space", "Comma"] {
            let err = validate_hotkey(combo).unwrap_err();
            assert!(err.contains("修饰键"), "{combo} → {err}");
        }
        assert!(validate_hotkey("nonsense").unwrap_err().contains("无法识别"));
    }
}
