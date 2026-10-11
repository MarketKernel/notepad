//! The few settings that outlive a window, in a plain `key=value` file:
//! `~/Library/Application Support/notepad/settings` on macOS,
//! `%APPDATA%\notepad\settings` on Windows, `~/.config/notepad/settings` elsewhere.

use std::path::PathBuf;

use eframe::egui::ThemePreference;

fn path() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".config")))?
    };
    Some(base.join("notepad").join("settings"))
}

fn theme_name(t: ThemePreference) -> &'static str {
    match t {
        ThemePreference::System => "system",
        ThemePreference::Light => "light",
        ThemePreference::Dark => "dark",
    }
}

pub fn load_theme() -> ThemePreference {
    let text = path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    text.lines()
        .filter_map(|l| l.strip_prefix("theme="))
        .find_map(|v| match v.trim() {
            "light" => Some(ThemePreference::Light),
            "dark" => Some(ThemePreference::Dark),
            "system" => Some(ThemePreference::System),
            _ => None,
        })
        .unwrap_or(ThemePreference::System)
}

/// Best effort: a failure only means the choice is not remembered.
pub fn save_theme(t: ThemePreference) {
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, format!("theme={}\n", theme_name(t)));
}
