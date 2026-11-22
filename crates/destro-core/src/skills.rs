//! Skills and custom slash commands.
//!
//! A **skill** is a folder with a `SKILL.md` (YAML-ish frontmatter with `name`
//! and `description`, then instructions). Only the name and description go in
//! the system prompt; the `skill` tool loads the body when the model asks for
//! it. A **command** is a Markdown file whose body becomes the prompt when the
//! user types `/name args` (`$ARGUMENTS` is replaced).
//!
//! Search paths, user scope first and project scope after it (a later entry
//! with the same name replaces an earlier one):
//!
//! * skills: `~/.destro/skills`, `~/.claude/skills`, `~/.agents/skills`,
//!   `~/.dsh/skills`, then `.destro/skills`, `.claude/skills`, `.agents/skills`
//!   in each directory from the root down to the workspace.
//! * commands: `~/.destro/commands`, `~/.claude/commands`, then
//!   `.destro/commands` and `.claude/commands` the same way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use destro_tools::{BoxFuture, Tool, ToolContext, ToolInvocation, ToolOutput};
use serde_json::{Value, json};

use crate::context::{Scope, ancestors_root_first, destro_home, home_dir};

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The `SKILL.md` file.
    pub path: PathBuf,
    /// Its folder: bundled files resolve against this.
    pub dir: PathBuf,
    pub scope: Scope,
}

#[derive(Clone, Debug)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub scope: Scope,
    pub body: String,
}

/// Split `---` frontmatter from the body. Understands `key: value`, quoted
/// values and `>`/`|` block scalars, which is all skill files use.
pub fn parse_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let text = text.trim_start_matches('\u{feff}');
    let mut map = BTreeMap::new();
    let norm = text.replace("\r\n", "\n");
    let Some(rest) = norm.strip_prefix("---\n") else {
        return (map, norm);
    };
    let Some(end) = rest.find("\n---") else {
        return (map, norm);
    };
    let (head, tail) = rest.split_at(end);
    let body = tail[4..].trim_start_matches('\n').to_string();
    let lines: Vec<&str> = head.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let key = k.trim().to_string();
        let mut val = v.trim().to_string();
        if matches!(val.as_str(), ">" | "|" | ">-" | "|-" | ">+" | "|+") {
            let folded = val.starts_with('>');
            let mut parts = vec![];
            while i < lines.len() && (lines[i].starts_with(' ') || lines[i].is_empty()) {
                parts.push(lines[i].trim().to_string());
                i += 1;
            }
            val = parts
                .join(if folded { " " } else { "\n" })
                .trim()
                .to_string();
        } else if (val.starts_with('"') && val.ends_with('"') && val.len() >= 2)
            || (val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2)
        {
            val = val[1..val.len() - 1].to_string();
        }
        map.insert(key, val);
    }
    (map, body)
}

fn user_dirs(kinds: &[&str]) -> Vec<PathBuf> {
    let mut v = vec![];
    if let Some(d) = destro_home() {
        for k in kinds {
            v.push(d.join(k));
        }
    }
    if let Some(h) = home_dir() {
        for (dir, k) in [
            (".claude", "skills"),
            (".agents", "skills"),
            (".dsh", "skills"),
        ] {
            if kinds.contains(&k) && k == "skills" {
                v.push(h.join(dir).join(k));
            }
        }
        if kinds.contains(&"commands") {
            v.push(h.join(".claude").join("commands"));
        }
    }
    v
}

fn project_dirs(cwd: &Path, kind: &str) -> Vec<PathBuf> {
    let homes: Vec<PathBuf> = home_dir().into_iter().collect();
    let mut v = vec![];
    for dir in ancestors_root_first(cwd) {
        // `~/.claude/skills` is user scope, not a project that happens to live at ~
        if homes.contains(&dir) {
            continue;
        }
        let names: &[&str] = if kind == "skills" {
            &[".destro", ".claude", ".agents"]
        } else {
            &[".destro", ".claude"]
        };
        for n in names {
            v.push(dir.join(n).join(kind));
        }
    }
    v
}

/// All skills visible from `cwd`, sorted by name.
pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
    let all = user_dirs(&["skills"])
        .into_iter()
        .map(|d| (Scope::User, d))
        .chain(
            project_dirs(cwd, "skills")
                .into_iter()
                .map(|d| (Scope::Project, d)),
        );
    for (scope, root) in all {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in rd.flatten() {
            let dir = entry.path();
            let file = dir.join("SKILL.md");
            if !file.is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let (fm, body) = parse_frontmatter(&text);
            let folder = entry.file_name().to_string_lossy().to_string();
            let name = fm
                .get("name")
                .cloned()
                .filter(|n| !n.is_empty())
                .unwrap_or(folder);
            let description = fm.get("description").cloned().unwrap_or_else(|| {
                body.lines()
                    .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
                    .unwrap_or("")
                    .trim()
                    .to_string()
            });
            by_name.insert(
                name.clone(),
                Skill {
                    name,
                    description: one_line(&description, 400),
                    path: file,
                    dir,
                    scope,
                },
            );
        }
    }
    by_name.into_values().collect()
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut t: String = flat.chars().take(max).collect();
        t.push('…');
        t
    }
}

/// The skill's instructions with frontmatter removed.
pub fn read_skill_body(skill: &Skill) -> std::io::Result<String> {
    let text = std::fs::read_to_string(&skill.path)?;
    Ok(parse_frontmatter(&text).1)
}

/// All custom commands visible from `cwd`, sorted by name.
pub fn discover_commands(cwd: &Path) -> Vec<CustomCommand> {
    let mut by_name: BTreeMap<String, CustomCommand> = BTreeMap::new();
    let all = user_dirs(&["commands"])
        .into_iter()
        .map(|d| (Scope::User, d))
        .chain(
            project_dirs(cwd, "commands")
                .into_iter()
                .map(|d| (Scope::Project, d)),
        );
    for (scope, root) in all {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (fm, body) = parse_frontmatter(&text);
            let description = fm.get("description").cloned().unwrap_or_else(|| {
                body.lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim_start_matches('#')
                    .trim()
                    .to_string()
            });
            by_name.insert(
                name.clone(),
                CustomCommand {
                    name,
                    description: one_line(&description, 120),
                    path,
                    scope,
                    body,
                },
            );
        }
    }
    by_name.into_values().collect()
}

/// Body with `$ARGUMENTS` replaced (appended when the body has no placeholder).
pub fn expand_command(cmd: &CustomCommand, args: &str) -> String {
    if cmd.body.contains("$ARGUMENTS") {
        cmd.body.replace("$ARGUMENTS", args)
    } else if args.trim().is_empty() {
        cmd.body.clone()
    } else {
        format!("{}\n\n{}", cmd.body.trim_end(), args)
    }
}

/// A prompt that carries a skill's instructions inline (used when the user
/// invokes a skill by hand instead of waiting for the model to ask).
pub fn skill_prompt(skill: &Skill, args: &str) -> std::io::Result<String> {
    let body = read_skill_body(skill)?;
    let mut s = format!(
        "Use the `{}` skill. Its instructions follow; bundled files live in {}.\n\n<skill name=\"{}\">\n{}\n</skill>",
        skill.name,
        skill.dir.display(),
        skill.name,
        body.trim_end()
    );
    if !args.trim().is_empty() {
        s.push_str(&format!("\n\nTask: {args}"));
    }
    Ok(s)
}

/// The `skill` tool: loads a skill's instructions on demand.
pub struct SkillTool {
    skills: Vec<Skill>,
}

impl SkillTool {
    pub fn new(skills: Vec<Skill>) -> Self {
        SkillTool { skills }
    }
}

fn list_bundled(dir: &Path) -> Vec<String> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n != "SKILL.md")
                && let Ok(rel) = p.strip_prefix(dir)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
            if out.len() >= 60 {
                out.sort();
                return out;
            }
        }
    }
    out.sort();
    out
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load a skill's full instructions by name. Use it when a task matches a skill listed \
         in the system prompt."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "name": { "type": "string", "description": "Skill name." } },
            "required": ["name"],
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        _cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let Some(name) = inv.arguments["name"].as_str() else {
                return ToolOutput::err("missing required argument `name`");
            };
            let Some(skill) = self
                .skills
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(name))
            else {
                let known: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
                return ToolOutput::err(format!(
                    "unknown skill {name:?}; available: {}",
                    known.join(", ")
                ));
            };
            match read_skill_body(skill) {
                Ok(body) => {
                    let mut out = format!(
                        "Skill directory: {} (resolve relative paths against it)\n\n{}",
                        skill.dir.display(),
                        body.trim_end()
                    );
                    let files = list_bundled(&skill.dir);
                    if !files.is_empty() {
                        out.push_str("\n\nBundled files:\n");
                        for f in files {
                            out.push_str(&format!("- {f}\n"));
                        }
                    }
                    ToolOutput::ok(out)
                }
                Err(e) => ToolOutput::err(format!("cannot read {}: {e}", skill.path.display())),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_handles_quotes_and_block_scalars() {
        let (fm, body) = parse_frontmatter(
            "---\nname: demo\ndescription: >\n  first line\n  second line\ntitle: \"quoted: yes\"\n---\n# Body\ntext",
        );
        assert_eq!(fm["name"], "demo");
        assert_eq!(fm["description"], "first line second line");
        assert_eq!(fm["title"], "quoted: yes");
        assert!(body.starts_with("# Body"));
    }

    #[test]
    fn no_frontmatter_returns_whole_text() {
        let (fm, body) = parse_frontmatter("just text");
        assert!(fm.is_empty());
        assert_eq!(body, "just text");
    }

    #[test]
    fn discovers_project_skills_and_commands() {
        let root = std::env::temp_dir().join(format!("destro-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sk = root.join(".claude").join("skills").join("alpha");
        std::fs::create_dir_all(&sk).unwrap();
        std::fs::write(
            sk.join("SKILL.md"),
            "---\nname: alpha\ndescription: does alpha\n---\nsteps",
        )
        .unwrap();
        std::fs::write(sk.join("helper.py"), "x=1").unwrap();
        let cm = root.join(".destro").join("commands");
        std::fs::create_dir_all(&cm).unwrap();
        std::fs::write(cm.join("fix.md"), "Fix $ARGUMENTS now").unwrap();

        let skills = discover_skills(&root);
        let alpha = skills.iter().find(|s| s.name == "alpha").expect("alpha");
        assert_eq!(alpha.description, "does alpha");
        assert_eq!(list_bundled(&alpha.dir), vec!["helper.py".to_string()]);
        let cmds = discover_commands(&root);
        let fix = cmds.iter().find(|c| c.name == "fix").expect("fix");
        assert_eq!(expand_command(fix, "the bug"), "Fix the bug now");
        let _ = std::fs::remove_dir_all(&root);
    }
}
