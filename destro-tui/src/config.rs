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
