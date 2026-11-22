//! Path resolution and text helpers shared by the tools.

use std::path::{Path, PathBuf};

use crate::ToolContext;

/// Resolves a caller-supplied path against the context working directory.
///
/// Absolute paths (including `C:\...` and UNC `\\server\share` paths) are
/// returned unchanged.
pub(crate) fn resolve(cx: &ToolContext, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cx.cwd.join(path)
    }
}

/// Renders `path` relative to `cwd` when it lives underneath it.
pub(crate) fn relative_display(path: &Path, cwd: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(relative) => relative.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

/// Returns the largest char boundary at or below `index`.
pub(crate) fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Truncates `text` so it fits in `max_bytes`, respecting char boundaries.
///
/// The returned flag is `true` when bytes were dropped.
pub(crate) fn truncate_bytes(text: &mut String, max_bytes: usize) -> bool {
    if text.len() <= max_bytes {
        return false;
    }
    let cut = floor_char_boundary(text, max_bytes);
    text.truncate(cut);
    true
}

/// Truncates `text` to its last `max_bytes` bytes, respecting char boundaries.
///
/// The returned flag is `true` when bytes were dropped.
pub(crate) fn truncate_tail(text: &mut String, max_bytes: usize) -> bool {
    if text.len() <= max_bytes {
        return false;
    }
    let cut = floor_char_boundary(text, text.len() - max_bytes);
    text.drain(..cut);
    true
}

/// Returns the path with `/` separators.
pub(crate) fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(cwd: &str) -> ToolContext {
        ToolContext {
            cwd: PathBuf::from(cwd),
            agent_id: "agent-000000".into(),
        }
    }

    #[test]
    fn resolve_joins_relative_and_keeps_absolute() {
        let cx = ctx(r"C:\work\repo");
        assert_eq!(
            resolve(&cx, "src/lib.rs"),
            PathBuf::from(r"C:\work\repo\src\lib.rs")
        );
        assert_eq!(
            resolve(&cx, r"C:\other\a.txt"),
            PathBuf::from(r"C:\other\a.txt")
        );
        assert_eq!(
            resolve(&cx, r"\\server\share\a.txt"),
            PathBuf::from(r"\\server\share\a.txt")
        );
    }

    #[test]
    fn relative_display_falls_back_to_absolute() {
        assert_eq!(
            relative_display(Path::new(r"C:\work\repo\a.txt"), Path::new(r"C:\work\repo")),
            "a.txt"
        );
        assert_eq!(
            relative_display(Path::new(r"C:\elsewhere\a.txt"), Path::new(r"C:\work\repo")),
            r"C:\elsewhere\a.txt"
        );
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let mut text = String::from("aéé");
        assert!(truncate_bytes(&mut text, 2));
        assert_eq!(text, "a");
        let mut tail = String::from("aéé");
        assert!(truncate_tail(&mut tail, 3));
        assert_eq!(tail, "éé");
        let mut short = String::from("abc");
        assert!(!truncate_bytes(&mut short, 3));
    }

    #[test]
    fn slash_path_normalizes_separators() {
        assert_eq!(slash_path(Path::new(r"a\b\c.rs")), "a/b/c.rs");
    }
}
