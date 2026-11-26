//! Cuting: a lightweight macOS screenshot tool with text extraction and
//! translation. See `overlay.rs` for the capture flow.

mod capture;
mod clipboard;
mod commands;
mod geo;
mod ocr;
mod overlay;
mod proxy;
mod scroll;
mod selftest;
mod settings;
mod translate;
mod tray;
mod util;

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use tauri::Manager;

pub struct AppState {
    pub settings: Mutex<settings::Settings>,
    /// The capture currently on screen, frozen pixels included.
    pub session: Mutex<Option<overlay::Session>>,
    /// The running long-screenshot job, if any.
    pub scroll: Mutex<Option<std::sync::Arc<scroll::Job>>>,
    /// Result waiting for the result window to pick it up.
    pub pending_result: Mutex<Option<serde_json::Value>>,
    /// The overlay page is in its select phase; the native magnifier follows
    /// the cursor only then.
    pub magnifier_on: AtomicBool,
    /// Running the automated self test: windows stay out of the user's way.
    pub selftest: bool,
    /// Overlay labels -> last JS heartbeat (ms epoch). A session presented to
    /// a page whose heartbeats don't resume is dead (its webview wedged at
    /// startup) and gets recreated — a dead page used to strand a frozen
    /// screenshot over the whole desktop.
    pub page_heartbeats: Mutex<HashMap<String, u64>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // A panic ends the process (release sets panic=abort), and with no
    // terminal attached its message would vanish. Route it into the log file
    // — the one place "why did my app die" is answerable.
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        clog!("PANIC: {info}\nbacktrace:\n{bt}");
    }));
    let selftest = selftest::requested();
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::overlay_start_payload,
            commands::set_overlay_phase,
            commands::broadcast_selection,
            commands::selection_start,
            commands::selection_move,
            commands::selection_end,
            commands::cancel_capture,
            commands::copy_region,
            commands::save_region,
            commands::region_pixels,
            commands::ocr_region,
            commands::scroll_capture_start,
            commands::scroll_state,
            commands::scroll_capture_save,
            commands::scroll_capture_copy,
            commands::scroll_capture_cancel,
            commands::translate_text,
            commands::test_network,
            commands::detect_local_proxies,
            commands::check_update,
            commands::install_update,
            commands::get_app_info,
            commands::save_settings,
            commands::pick_save_dir,
            commands::take_result,
            commands::open_settings_window,
            commands::screen_capture_allowed,
            commands::open_screen_settings,
            commands::open_url,
            commands::copy_text,
            commands::start_capture,
            commands::log_message,
            commands::page_heartbeat,
            selftest::selftest_report,
        ]);
    // The single-instance lock is keyed by the app identifier. The self test
    // must not disturb (or be stopped by) the user's running instance.
    let builder = if selftest {
        builder
    } else {
        builder.plugin(tauri_plugin_single_instance::init(|_app, _args, _cwd| {}))
    };

    let app = builder
        .setup(move |app| {
            let settings = settings::load(app.handle());
            // Which services and hotkeys are in effect, without any secrets:
            // the first thing to look at when something "doesn't work".
            clog!(
                "settings loaded: translator {}, proxy {}, hotkeys {:?} / {:?} / {:?}",
                settings.translator.name(),
                serde_json::to_string(&settings.proxy).unwrap_or_default(),
                settings.hotkeys.shot,
                settings.hotkeys.ocr,
                settings.hotkeys.translate
            );
            // The first accurate request per process pays a model load that can
            // stall for tens of seconds on a machine with a wedged ANE pipeline;
            // pay it invisibly here instead of on the user's first screenshot.
            // (The self test skips it on purpose: it exercises the degraded path.)
            let ocr_langs = settings.ocr_languages.clone();
            app.manage(AppState {
                settings: Mutex::new(settings),
                session: Mutex::new(None),
                scroll: Mutex::new(None),
                pending_result: Mutex::new(None),
                magnifier_on: AtomicBool::new(false),
                selftest,
                page_heartbeats: Mutex::new(HashMap::new()),
            });
            // Menu bar app (LSUIElement already keeps it out of the Dock): the
            // tray is the entry point.
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            if selftest {
                overlay::prewarm(app.handle());
                selftest::run(app.handle().clone());
            } else {
                if let Err(e) = overlay::register_hotkeys(app.handle()) {
                    clog!("{e}");
                    // A hotkey that silently doesn't work is the most confusing
                    // failure there is, so say so — once the tray is up.
                    let handle = app.handle().clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(800));
                        overlay::alert(&handle, "部分快捷键不可用", e);
                    });
                }
                tray::create(app)?;
                overlay::prewarm(app.handle());
                ocr::prewarm(ocr_langs);
                // Quiet update check a few seconds in, so a slow network never
                // delays startup. The result surfaces in the settings window.
                let check_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    match commands::check_update(check_handle.clone()).await {
                        Ok(Some(info)) => clog!(
                            "update available: {} (current {}); see the settings window",
                            info.version, info.current_version
                        ),
                        Ok(None) => clog!("no update available"),
                        Err(e) => clog!("update check failed (no release feed yet?): {e}"),
                    }
                });
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|_, _| {});
}
