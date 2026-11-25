//! Menu bar icon: left click takes a screenshot, right click opens the menu.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use crate::overlay::{self, Mode};
use crate::util::lock;
use crate::AppState;

const TRAY_ICON: &[u8] = include_bytes!("../icons/tray.png");

fn tray_image() -> tauri::image::Image<'static> {
    tauri::image::Image::from_bytes(TRAY_ICON).expect("icons/tray.png is a valid PNG")
}

fn menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let hotkeys = lock(&app.state::<AppState>().settings).hotkeys.clone();
    let shot = MenuItem::with_id(app, "shot", "截图", true, Some(hotkeys.shot.as_str()))?;
    let ocr = MenuItem::with_id(app, "ocr", "截图提取文字", true, Some(hotkeys.ocr.as_str()))?;
    let translate = MenuItem::with_id(app, "translate", "截图翻译", true, Some(hotkeys.translate.as_str()))?;
    let settings = MenuItem::with_id(app, "settings", "设置…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出 Cuting", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    Menu::with_items(app, &[&shot, &ocr, &translate, &separator, &settings, &separator, &quit])
}

pub fn create(app: &mut tauri::App) -> tauri::Result<()> {
    TrayIconBuilder::with_id("main")
        .icon(tray_image())
        .icon_as_template(true)
        .tooltip("Cuting 截图")
        .show_menu_on_left_click(false)
        .menu(&menu(app.handle())?)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "shot" => overlay::start(app, Mode::Shot),
            "ocr" => overlay::start(app, Mode::Ocr),
            "translate" => overlay::start(app, Mode::Translate),
            "settings" => overlay::show_settings(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                overlay::start(tray.app_handle(), Mode::Shot);
            }
        })
        .build(app)?;
    Ok(())
}

/// Rebuild the menu so the accelerator labels follow the settings.
pub fn refresh(app: &AppHandle) -> tauri::Result<()> {
    if let Some(tray) = app.tray_by_id("main") {
        tray.set_menu(Some(menu(app)?))?;
    }
    Ok(())
}
