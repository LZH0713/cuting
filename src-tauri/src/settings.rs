//! User settings, persisted as JSON in the app config directory.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::proxy::ProxyConfig;
use crate::translate::{TranslatorConfig, DEFAULT_TARGET};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Hotkeys {
    pub shot: String,
    pub ocr: String,
    pub translate: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self { shot: "Alt+A".into(), ocr: "Alt+Shift+A".into(), translate: "Alt+Shift+T".into() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub hotkeys: Hotkeys,
    /// Vision recognition languages, priority order. Empty = auto-detect.
    pub ocr_languages: Vec<String>,
    /// Default target language for translations.
    pub target_lang: String,
    /// Note: API keys are stored in plain text on this machine.
    pub translator: TranslatorConfig,
    /// Where the save dialog starts. `None` = `~/Pictures/cuting`.
    pub save_dir: Option<String>,
    /// How translation requests reach the network.
    pub proxy: ProxyConfig,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkeys: Hotkeys::default(),
            ocr_languages: vec!["zh-Hans".into(), "en-US".into()],
            target_lang: DEFAULT_TARGET.into(),
            translator: TranslatorConfig::Google,
            save_dir: None,
            proxy: ProxyConfig::System,
        }
    }
}

fn config_path(app: &AppHandle) -> PathBuf {
    // The self test points this at a scratch directory so it never touches
    // the user's settings.
    std::env::var_os("CUTING_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| app.path().app_config_dir().ok())
        .unwrap_or_else(std::env::temp_dir)
        .join("settings.json")
}

pub fn load(app: &AppHandle) -> Settings {
    let path = config_path(app);
    let Ok(text) = std::fs::read_to_string(&path) else { return Settings::default() };
    match serde_json::from_str(&text) {
        Ok(settings) => settings,
        Err(e) => {
            crate::clog!("settings at {} are invalid ({e}); using defaults", path.display());
            Settings::default()
        }
    }
}

pub fn save_to_disk(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let path = config_path(app);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("无法创建配置目录：{e}"))?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("无法保存设置：{e}"))
}

pub fn default_save_dir() -> PathBuf {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Pictures/cuting")).unwrap_or_else(std::env::temp_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let s: Settings = serde_json::from_str(r#"{"hotkeys":{"shot":"F1"}}"#).unwrap();
        assert_eq!(s.hotkeys.shot, "F1");
        assert_eq!(s.hotkeys.ocr, "Alt+Shift+A");
        assert_eq!(s.target_lang, "zh-CN");
        assert_eq!(s.ocr_languages, vec!["zh-Hans", "en-US"]);
        // Unknown keys from a newer version are ignored rather than rejected.
        let s: Settings = serde_json::from_str(r#"{"somethingNew":true}"#).unwrap();
        assert_eq!(s.hotkeys.shot, "Alt+A");
        assert_eq!(s.proxy, ProxyConfig::System);
        // Settings written before the proxy option existed still load.
        let s: Settings = serde_json::from_str(r#"{"hotkeys":{"shot":"F1"},"translator":{"provider":"google"}}"#).unwrap();
        assert_eq!(s.proxy, ProxyConfig::System);
        let s: Settings = serde_json::from_str(r#"{"proxy":{"mode":"manual","url":"socks5://127.0.0.1:1080"}}"#).unwrap();
        assert_eq!(s.proxy, ProxyConfig::Manual { url: "socks5://127.0.0.1:1080".into() });
    }
}
