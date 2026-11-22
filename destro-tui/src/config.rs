//! Persisted workspace list (`workspaces.json`), so `/open` survives restarts.

use std::path::PathBuf;

fn file() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("destro").join("workspaces.json"))
}

pub fn load() -> Vec<String> {
    let Some(p) = file() else { return vec![] };
    let Ok(text) = std::fs::read_to_string(p) else {
        return vec![];
    };
    serde_json::from_str::<Vec<String>>(&text).unwrap_or_default()
}

pub fn save(paths: &[String]) {
    let Some(p) = file() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(paths) {
        let _ = std::fs::write(p, text);
    }
}

// ----- settings (theme, ...) -----------------------------------------------------------------

fn settings_file() -> Option<PathBuf> {
    file().map(|p| p.with_file_name("settings.json"))
}

fn read_settings() -> serde_json::Value {
    settings_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Saved theme name, if any.
pub fn load_theme() -> Option<String> {
    read_settings()["theme"].as_str().map(str::to_string)
}

pub fn save_theme(name: &str) {
    let Some(p) = settings_file() else { return };
    let mut v = read_settings();
    if !v.is_object() {
        v = serde_json::json!({});
    }
    v["theme"] = serde_json::Value::String(name.to_string());
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(&v) {
        let _ = std::fs::write(p, text);
    }
}

// ----- prompt history ------------------------------------------------------------------------

fn history_file() -> Option<PathBuf> {
    file().map(|p| p.with_file_name("history.json"))
}

pub fn load_history() -> Vec<String> {
    history_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_history(h: &[String]) {
    let Some(p) = history_file() else { return };
    if let Ok(t) = serde_json::to_string(h) {
        let _ = std::fs::write(p, t);
    }
}
