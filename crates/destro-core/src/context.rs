//! Project conventions: the instruction files every agent should see.
//!
//! Destro follows the conventions other agent tools established, so a repo that
//! already has an `AGENTS.md` or `CLAUDE.md` works with no setup:
//!
//! * **User scope**, always loaded first: `~/.destro/AGENTS.md`,
//!   `~/.destro/CLAUDE.md`, `~/.claude/CLAUDE.md`, `~/.agents/AGENTS.md` and
//!   `~/.dsh/AGENTS.md`. `$DESTRO_HOME` replaces `~/.destro`.
//! * **Project scope**: in every directory from the filesystem root down to the
//!   workspace: `AGENTS.md`, `CLAUDE.md`, `CLAUDE.local.md`,
//!   `.destro/AGENTS.md`, `.claude/CLAUDE.md`. Directories nearer the workspace
//!   come later, so they read as the more specific instructions.
//! * `@path/to/file.md` inside an instruction file inlines that file (up to four
//!   levels deep), the same import syntax `CLAUDE.md` uses.
//!
//! Identical files (a `CLAUDE.md` symlinked to `AGENTS.md`, say) load once.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::skills::Skill;

/// Per-chain size cap: user instructions and project instructions are budgeted
/// separately, like dsh does.
pub const BUDGET: usize = 64 * 1024;
const MAX_IMPORT_DEPTH: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

/// One instruction file that made it into the prompt.
#[derive(Clone, Debug)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub scope: Scope,
    /// Text after `@import` expansion and budget truncation.
    pub text: String,
    pub bytes: usize,
    pub truncated: bool,
    pub imports: usize,
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// `$DESTRO_HOME`, else `~/.destro`.
pub fn destro_home() -> Option<PathBuf> {
    match std::env::var_os("DESTRO_HOME") {
        Some(p) if !p.is_empty() => Some(PathBuf::from(p)),
        _ => home_dir().map(|h| h.join(".destro")),
    }
}

/// Per-user data (sessions, settings): `%APPDATA%\destro` or the XDG equivalent.
pub fn data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("destro"))
}

/// Directories from the filesystem root down to `cwd`, in that order.
pub fn ancestors_root_first(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = cwd.ancestors().map(Path::to_path_buf).collect();
    dirs.reverse();
    dirs
}

fn user_candidates() -> Vec<PathBuf> {
    let mut v = vec![];
    if let Some(d) = destro_home() {
        v.push(d.join("AGENTS.md"));
        v.push(d.join("CLAUDE.md"));
    }
    if let Some(h) = home_dir() {
        v.push(h.join(".claude").join("CLAUDE.md"));
        v.push(h.join(".agents").join("AGENTS.md"));
        let dsh = std::env::var_os("DSH_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| h.join(".dsh"));
        v.push(dsh.join("AGENTS.md"));
    }
    v
}

fn project_candidates(cwd: &Path) -> Vec<PathBuf> {
    let mut v = vec![];
    for dir in ancestors_root_first(cwd) {
        v.push(dir.join("AGENTS.md"));
        v.push(dir.join("CLAUDE.md"));
        v.push(dir.join("CLAUDE.local.md"));
        v.push(dir.join(".destro").join("AGENTS.md"));
        v.push(dir.join(".claude").join("CLAUDE.md"));
    }
    v
}

fn hash_of(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Every instruction file that applies to `cwd`, user scope first.
pub fn load_instructions(cwd: &Path) -> Vec<InstructionFile> {
    let mut out = vec![];
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    let mut seen_text: HashSet<u64> = HashSet::new();
    for (scope, cands) in [
        (Scope::User, user_candidates()),
        (Scope::Project, project_candidates(cwd)),
    ] {
        let mut used = 0usize;
        for path in cands {
            let Ok(canon) = std::fs::canonicalize(&path) else {
                continue;
            };
            if !canon.is_file() || !seen_paths.insert(canon.clone()) {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&canon) else {
                continue;
            };
            if raw.trim().is_empty() || !seen_text.insert(hash_of(raw.trim())) {
                continue;
            }
            let mut imports = 0;
            let mut visited = HashSet::from([canon.clone()]);
            let mut text = expand_imports(
                &raw,
                canon.parent().unwrap_or(Path::new(".")),
                1,
                &mut visited,
                &mut imports,
            );
            let bytes = text.len();
            let room = BUDGET.saturating_sub(used);
            let truncated = bytes > room;
            if truncated {
                let mut cut = room;
                while cut > 0 && !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
                text.push_str("\n… [truncated: instruction budget reached]");
            }
            used += text.len();
            out.push(InstructionFile {
                path,
                scope,
                text,
                bytes,
                truncated,
                imports,
            });
            if used >= BUDGET {
                break;
            }
        }
    }
    out
}

/// Inline `@path` references on their own words, outside code fences.
fn expand_imports(
    text: &str,
    base: &Path,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    count: &mut usize,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fenced = false;
    for line in text.lines() {
        out.push_str(line);
        out.push('\n');
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || depth > MAX_IMPORT_DEPTH {
            continue;
        }
        for word in line.split_whitespace() {
            let Some(rest) = word.strip_prefix('@') else {
                continue;
            };
            let rest = rest.trim_end_matches(['.', ',', ';', ':', ')', '`']);
            if rest.is_empty() || rest.contains('@') {
                continue;
            }
            let path = if let Some(r) = rest.strip_prefix("~/") {
                match home_dir() {
                    Some(h) => h.join(r),
                    None => continue,
                }
            } else {
                base.join(rest)
            };
            let Ok(canon) = std::fs::canonicalize(&path) else {
                continue;
            };
            if !canon.is_file() || !visited.insert(canon.clone()) {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&canon) else {
                continue;
            };
            *count += 1;
            let inner = expand_imports(
                &body,
                canon.parent().unwrap_or(Path::new(".")),
                depth + 1,
                visited,
                count,
            );
            out.push_str(&format!(
                "\n<imported from=\"{}\">\n{}\n</imported>\n\n",
                rest,
                inner.trim_end()
            ));
        }
    }
    out
}

// ----- nested instructions ----------------------------------------------------------------

/// Instruction files in directories *below* the workspace that lie on the way to
/// `target`, not yet in `seen`. The workspace's own chain is already in the
/// system prompt; these are the sub-folder `AGENTS.md` files that apply once the
/// agent starts working in that part of the tree.
pub fn nested_instructions(
    cwd: &Path,
    target: &Path,
    seen: &mut HashSet<PathBuf>,
) -> Vec<(PathBuf, String)> {
    let target = if target.is_relative() {
        cwd.join(target)
    } else {
        target.to_path_buf()
    };
    let dir = if target.is_dir() {
        target
    } else {
        match target.parent() {
            Some(p) => p.to_path_buf(),
            None => return vec![],
        }
    };
    let rel = match dir.strip_prefix(cwd) {
        Ok(r) => r.to_path_buf(),
        Err(_) => match (std::fs::canonicalize(&dir), std::fs::canonicalize(cwd)) {
            (Ok(d), Ok(c)) => match d.strip_prefix(&c) {
                Ok(r) => r.to_path_buf(),
                Err(_) => return vec![],
            },
            _ => return vec![],
        },
    };
    let mut out = vec![];
    let mut d = cwd.to_path_buf();
    for comp in rel.components() {
        d.push(comp);
        for name in ["AGENTS.md", "CLAUDE.md", "CLAUDE.local.md"] {
            let f = d.join(name);
            if !f.is_file() || !seen.insert(f.clone()) {
                continue;
            }
            if let Ok(mut text) = std::fs::read_to_string(&f) {
                if text.trim().is_empty() {
                    continue;
                }
                if text.len() > 16 * 1024 {
                    let mut cut = 16 * 1024;
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text.truncate(cut);
                    text.push_str("\n… [truncated]");
                }
                out.push((f, text));
            }
        }
    }
    out
}

// ----- system prompt assembly -------------------------------------------------------------

/// Today's UTC date as `YYYY-MM-DD` (no calendar dependency).
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    date_from_secs(secs)
}

/// `YYYY-MM-DD` (UTC) for a millisecond Unix timestamp.
pub fn date_from_ms(ms: u64) -> String {
    date_from_secs((ms / 1000) as i64)
}

fn date_from_secs(secs: i64) -> String {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Base prompt + environment + instruction files + skill catalogue.
pub fn assemble(base: &str, cwd: &Path, instr: &[InstructionFile], skills: &[Skill]) -> String {
    let mut s = String::new();
    s.push_str(base.trim_end());
    s.push_str("\n\n# Environment\n");
    s.push_str(&format!("- Working directory: {}\n", cwd.display()));
    s.push_str(&format!(
        "- Platform: {} · shell tool: {}\n",
        std::env::consts::OS,
        if cfg!(windows) {
            "pwsh"
        } else {
            "pwsh (PowerShell)"
        }
    ));
    s.push_str(&format!("- Date: {}\n", today()));
    if !instr.is_empty() {
        s.push_str(
            "\n# Instructions\nThe user and project provided these instructions. Follow them; \
             when they conflict, the later (more specific) file wins.\n",
        );
        for f in instr {
            s.push_str(&format!(
                "\n<instructions source=\"{}\" scope=\"{}\">\n{}\n</instructions>\n",
                f.path.display(),
                f.scope.as_str(),
                f.text.trim_end()
            ));
        }
    }
    if !skills.is_empty() {
        s.push_str(
            "\n# Skills\nSkills are packaged instructions you can load on demand. When a task \
             matches a skill's description, call the `skill` tool with its name before \
             starting, then follow what it returns.\n\n",
        );
        for k in skills {
            s.push_str(&format!("- {}: {}\n", k.name, k.description));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "destro-ctx-{name}-{}-{}",
            std::process::id(),
            hash_of(name)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn loads_chain_dedups_and_imports() {
        let root = tmp("chain");
        let sub = root.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(sub.join("AGENTS.md"), "leaf rules\nsee @extra.md").unwrap();
        std::fs::write(sub.join("extra.md"), "imported body").unwrap();
        // identical content under another name loads once
        std::fs::write(sub.join("CLAUDE.md"), "root rules").unwrap();
        let files = load_instructions(&sub);
        let mine: Vec<_> = files.iter().filter(|f| f.path.starts_with(&root)).collect();
        assert_eq!(mine.len(), 2, "{mine:?}");
        assert!(mine[0].text.contains("root rules"));
        assert!(mine[1].text.contains("imported body"));
        assert_eq!(mine[1].imports, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_cycles_terminate() {
        let root = tmp("cycle");
        std::fs::write(root.join("AGENTS.md"), "a @b.md").unwrap();
        std::fs::write(root.join("b.md"), "b @AGENTS.md").unwrap();
        let files = load_instructions(&root);
        assert!(files.iter().any(|f| f.text.contains("b @AGENTS.md")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_files_load_once_and_only_below_the_workspace() {
        let root = tmp("nested");
        let sub = root.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("AGENTS.md"), "top").unwrap();
        std::fs::write(root.join("a").join("AGENTS.md"), "a rules").unwrap();
        std::fs::write(sub.join("CLAUDE.md"), "b rules").unwrap();
        let mut seen = HashSet::new();
        let got = nested_instructions(&root, &sub.join("file.rs"), &mut seen);
        let texts: Vec<&str> = got.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(
            texts,
            ["a rules", "b rules"],
            "workspace-level file excluded"
        );
        assert!(nested_instructions(&root, &sub.join("other.rs"), &mut seen).is_empty());
        let outside = std::env::temp_dir().join("elsewhere.txt");
        assert!(nested_instructions(&root, &outside, &mut seen).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn date_is_plausible() {
        let d = today();
        assert_eq!(d.len(), 10);
        assert!(d.starts_with("20"));
    }

    #[test]
    fn assemble_includes_every_section() {
        let f = InstructionFile {
            path: PathBuf::from("AGENTS.md"),
            scope: Scope::Project,
            text: "be terse".into(),
            bytes: 8,
            truncated: false,
            imports: 0,
        };
        let sk = Skill {
            name: "demo".into(),
            description: "does demo things".into(),
            path: PathBuf::from("SKILL.md"),
            dir: PathBuf::from("."),
            scope: Scope::User,
        };
        let s = assemble("base", Path::new("."), &[f], &[sk]);
        assert!(s.contains("be terse") && s.contains("- demo: does demo things"));
        assert!(s.starts_with("base"));
    }
}
