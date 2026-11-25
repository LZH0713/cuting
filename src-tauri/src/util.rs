//! Small shared helpers: logging and file-name timestamps.

use std::io::Write as _;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use std::sync::MutexGuard;

static LOG_LOCK: Mutex<()> = Mutex::new(());

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn log_file_path() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join("Library/Logs/Cuting/app.log"))
        .unwrap_or_else(std::env::temp_dir)
}

pub fn log(args: std::fmt::Arguments) {
    let line = format!("{} {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"), args);
    eprintln!("{line}");
    let Ok(guard) = LOG_LOCK.lock() else { return };
    let path = log_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
    drop(guard);
}

/// Timestamp for generated file names, e.g. `2026-11-25 15.30.45`.
pub fn file_stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H.%M.%S").to_string()
}

/// Lock a mutex even if a previous holder panicked, so one bad request can't
/// wedge the whole app.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[macro_export]
macro_rules! clog {
    ($($arg:tt)*) => { $crate::util::log(format_args!($($arg)*)) };
}
