//! The `glob` and `grep` search tools.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use regex::RegexBuilder;

use crate::args;
use crate::paths;
use crate::{BoxFuture, Tool, ToolContext, ToolInvocation, ToolOutput};

/// Maximum number of glob matches returned.
const GLOB_CAP: usize = 100;
/// Maximum number of grep matching lines returned.
const GREP_CAP: usize = 250;
/// Maximum length of a single emitted grep line, in bytes.
const GREP_LINE_CAP: usize = 4 * 1024;
/// Maximum size of a file the directory walk of `grep` inspects.
const GREP_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Bytes inspected when deciding whether a file is binary.
const GREP_SNIFF_BYTES: usize = 8 * 1024;

/// Finds files by glob pattern below a base directory.
#[derive(Clone, Copy, Debug, Default)]
pub struct GlobTool;

impl GlobTool {
    /// Creates the tool.
    pub fn new() -> Self {
        Self
    }
}

impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files whose path matches a glob pattern, honouring .gitignore."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern matched against paths relative to the base directory."
                },
                "path": {
                    "type": "string",
                    "description": "Base directory (default: the working directory)."
                },
                "hidden": {
                    "type": "boolean",
                    "description": "Also consider hidden files and directories (default false)."
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let pattern = match args::require_str(inv, "pattern") {
                Ok(pattern) => pattern,
                Err(output) => return output,
            };
            let base = match args::opt_str(inv, "path") {
                Some(raw) => paths::resolve(cx, raw),
                None => cx.cwd.clone(),
            };
            let hidden = args::opt_bool(inv, "hidden").unwrap_or(false);
            let matcher: GlobMatcher = match Glob::new(pattern) {
                Ok(glob) => glob.compile_matcher(),
                Err(error) => return ToolOutput::err(format!("invalid glob {pattern:?}: {error}")),
            };
            if let Err(error) = tokio::fs::metadata(&base).await {
                return ToolOutput::err(format!("cannot access \"{}\": {error}", base.display()));
            }

            let mut found: Vec<String> = Vec::new();
            let walker = WalkBuilder::new(&base)
                .hidden(!hidden)
                .follow_links(false)
                .sort_by_file_path(|left, right| left.cmp(right))
                .build();
            for entry in walker {
                let Ok(entry) = entry else { continue };
                if !entry
                    .file_type()
                    .map(|kind| kind.is_file())
                    .unwrap_or(false)
                {
                    continue;
                }
                let relative = match entry.path().strip_prefix(&base) {
                    Ok(relative) => relative,
                    Err(_) => continue,
                };
                if !matcher.is_match(paths::slash_path(relative)) {
                    continue;
                }
                found.push(paths::relative_display(entry.path(), &cx.cwd));
            }
            found.sort();
            if found.is_empty() {
                return ToolOutput::ok(format!("no files matched {pattern}"));
            }
            let truncated = found.len() > GLOB_CAP;
            found.truncate(GLOB_CAP);
            let mut content = found.join("\n");
            content.push('\n');
            if truncated {
                content.push_str("[... more matches truncated at 100]\n");
            }
            ToolOutput::ok(content)
        })
    }
}

/// Searches file contents with a regular expression.
#[derive(Clone, Copy, Debug, Default)]
pub struct GrepTool;

impl GrepTool {
    /// Creates the tool.
    pub fn new() -> Self {
        Self
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regular expression and report matching lines."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Rust regular expression to search for."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search (default: the working directory)."
                },
                "include": {
                    "type": "string",
                    "description": "Glob filter applied to searched file paths, e.g. \"*.rs\"."
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Match case-insensitively (default false)."
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let pattern = match args::require_str(inv, "pattern") {
                Ok(pattern) => pattern,
                Err(output) => return output,
            };
            let case_insensitive = args::opt_bool(inv, "case_insensitive").unwrap_or(false);
            let regex = match RegexBuilder::new(pattern)
                .case_insensitive(case_insensitive)
                .build()
            {
                Ok(regex) => regex,
                Err(error) => return ToolOutput::err(format!("invalid regex: {error}")),
            };
            let include: Option<GlobMatcher> = match args::opt_str(inv, "include") {
                Some(glob) => match Glob::new(glob) {
                    Ok(glob) => Some(glob.compile_matcher()),
                    Err(error) => return ToolOutput::err(format!("invalid include glob: {error}")),
                },
                None => None,
            };
            let base = match args::opt_str(inv, "path") {
                Some(raw) => paths::resolve(cx, raw),
                None => cx.cwd.clone(),
            };
            let metadata = match tokio::fs::metadata(&base).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    return ToolOutput::err(format!(
                        "cannot access \"{}\": {error}",
                        base.display()
                    ));
                }
            };

            let mut candidates: Vec<PathBuf> = Vec::new();
            if metadata.is_file() {
                if matcher_allows(&include, &base) {
                    candidates.push(base.clone());
                }
            } else {
                let walker = WalkBuilder::new(&base)
                    .follow_links(false)
                    .sort_by_file_path(|left, right| left.cmp(right))
                    .build();
                for entry in walker {
                    let Ok(entry) = entry else { continue };
                    if !entry
                        .file_type()
                        .map(|kind| kind.is_file())
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    let Ok(entry_metadata) = entry.metadata() else {
                        continue;
                    };
                    if entry_metadata.len() > GREP_MAX_FILE_BYTES {
                        continue;
                    }
                    if matcher_allows(&include, entry.path()) {
                        candidates.push(entry.into_path());
                    }
                }
            }

            let mut matches: Vec<String> = Vec::new();
            let mut truncated = false;
            'files: for path in candidates {
                let Ok(bytes) = tokio::fs::read(&path).await else {
                    continue;
                };
                if bytes.iter().take(GREP_SNIFF_BYTES).any(|byte| *byte == 0) {
                    continue;
                }
                let text = String::from_utf8_lossy(&bytes);
                let label = paths::relative_display(&path, &cx.cwd);
                for (index, line) in text.lines().enumerate() {
                    let line = line.trim_end_matches('\r');
                    if !regex.is_match(line) {
                        continue;
                    }
                    let mut rendered = line;
                    if rendered.len() > GREP_LINE_CAP {
                        rendered = &rendered[..paths::floor_char_boundary(rendered, GREP_LINE_CAP)];
                    }
                    matches.push(format!("{label}:{}: {rendered}", index + 1));
                    if matches.len() >= GREP_CAP {
                        truncated = true;
                        break 'files;
                    }
                }
            }

            if matches.is_empty() {
                return ToolOutput::ok(format!("no matches for {pattern}"));
            }
            let mut content = matches.join("\n");
            content.push('\n');
            if truncated {
                content.push_str("[... truncated: 250 matches shown]\n");
            }
            ToolOutput::ok(content)
        })
    }
}

/// Returns whether `path` passes the optional include filter.
///
/// Matching uses the file name for a file base and the full path otherwise,
/// always with `/` separators.
fn matcher_allows(include: &Option<GlobMatcher>, path: &Path) -> bool {
    match include {
        Some(matcher) => {
            matcher.is_match(paths::slash_path(path))
                || path
                    .file_name()
                    .map(|name| matcher.is_match(name))
                    .unwrap_or(false)
        }
        None => true,
    }
}
