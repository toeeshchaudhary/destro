//! Small dependency-free syntax highlighter (no syntect: its grammar set costs
//! several MB resident and a noticeable startup). A line tokenizer with just
//! enough state for block comments and Python triple quotes; good for reading
//! model output, not a parser.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    Plain,
    Kw,
    Str,
    Num,
    Comment,
    Ty,
    Func,
    Add,
    Del,
    Hunk,
}

pub fn style(t: Tok) -> Style {
    let s = Style::default();
    match t {
        Tok::Plain => s,
        Tok::Kw => s.fg(Color::Magenta),
        Tok::Str => s.fg(Color::Green),
        Tok::Num => s.fg(Color::Yellow),
        Tok::Comment => s.fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        Tok::Ty => s.fg(Color::Cyan),
        Tok::Func => s.fg(Color::LightBlue),
        Tok::Add => s.fg(Color::Green),
        Tok::Del => s.fg(Color::Red),
        Tok::Hunk => s.fg(Color::Cyan).add_modifier(Modifier::DIM),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fam {
    Rust,
    Py,
    Js,
    Go,
    C,
    Json,
    Sh,
    Toml,
    Sql,
    Diff,
    Plain,
}

/// Map a fence tag or file extension to a language family.
fn family(tag: &str) -> Fam {
    let t = tag
        .trim()
        .trim_start_matches("language-")
        .to_ascii_lowercase();
    let t = t
        .split(|c: char| c.is_whitespace() || c == '{' || c == ',')
        .next()
        .unwrap_or("");
    match t {
        "rust" | "rs" => Fam::Rust,
        "python" | "py" | "python3" => Fam::Py,
        "js" | "javascript" | "jsx" | "mjs" | "cjs" | "ts" | "typescript" | "tsx" => Fam::Js,
        "go" | "golang" => Fam::Go,
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "c++" | "java" | "cs" | "csharp" | "kt"
        | "kotlin" | "swift" => Fam::C,
        "json" | "jsonc" | "json5" => Fam::Json,
        "sh" | "bash" | "zsh" | "shell" | "console" | "powershell" | "ps1" | "fish" => Fam::Sh,
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" => Fam::Toml,
        "sql" => Fam::Sql,
        "diff" | "patch" => Fam::Diff,
        _ => Fam::Plain,
    }
}

pub fn is_known(tag: &str) -> bool {
    family(tag) != Fam::Plain
}

const RUST: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];
const PY: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield", "self",
];
const JS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "of",
    "private",
    "public",
    "readonly",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "yield",
];
const GO: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
    "nil",
    "true",
    "false",
];
const C: &[&str] = &[
    "auto",
    "bool",
    "break",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "double",
    "else",
    "enum",
    "extern",
    "false",
    "final",
    "float",
    "for",
    "if",
    "include",
    "inline",
    "int",
    "long",
    "namespace",
    "new",
    "null",
    "nullptr",
    "override",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "template",
    "this",
    "throw",
    "true",
    "try",
    "typedef",
    "typename",
    "union",
    "unsigned",
    "using",
    "virtual",
    "void",
    "volatile",
    "while",
    "import",
    "package",
    "interface",
    "fun",
    "val",
    "var",
    "let",
    "func",
];
const SH: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "case", "esac",
    "function", "return", "export", "local", "echo", "cd", "set", "unset",
];
const TOML: &[&str] = &["true", "false", "null", "yes", "no"];
const SQL: &[&str] = &[
    "select",
    "from",
    "where",
    "insert",
    "into",
    "values",
    "update",
    "set",
    "delete",
    "create",
    "table",
    "index",
    "alter",
    "drop",
    "join",
    "left",
    "right",
    "inner",
    "on",
    "group",
    "by",
    "order",
    "limit",
    "and",
    "or",
    "not",
    "null",
    "primary",
    "key",
    "unique",
    "default",
    "references",
    "as",
    "distinct",
    "having",
    "union",
    "SELECT",
    "FROM",
    "WHERE",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "CREATE",
    "TABLE",
    "INDEX",
    "ALTER",
    "DROP",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "ON",
    "GROUP",
    "BY",
    "ORDER",
    "LIMIT",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "PRIMARY",
    "KEY",
    "UNIQUE",
    "DEFAULT",
    "REFERENCES",
    "AS",
    "DISTINCT",
    "HAVING",
    "UNION",
    "INTEGER",
    "TEXT",
    "REAL",
    "BLOB",
    "VARCHAR",
    "TIMESTAMP",
];

pub struct Highlighter {
    fam: Fam,
    block: bool,
    triple: Option<char>,
}

impl Highlighter {
    pub fn new(tag: &str) -> Self {
        Highlighter {
            fam: family(tag),
            block: false,
            triple: None,
        }
    }

    fn keywords(&self) -> &'static [&'static str] {
        match self.fam {
            Fam::Rust => RUST,
            Fam::Py => PY,
            Fam::Js => JS,
            Fam::Go => GO,
            Fam::C => C,
            Fam::Sh => SH,
            Fam::Toml | Fam::Json => TOML,
            Fam::Sql => SQL,
            _ => &[],
        }
    }

    /// Tokenize one line (tabs are expanded by the caller).
    pub fn line(&mut self, s: &str) -> Vec<(Tok, String)> {
        let mut out: Vec<(Tok, String)> = vec![];
        let mut push = |t: Tok, text: String| {
            if text.is_empty() {
                return;
            }
            match out.last_mut() {
                Some((lt, ls)) if *lt == t => ls.push_str(&text),
                _ => out.push((t, text)),
            }
        };
        let ch: Vec<char> = s.chars().collect();
        let n = ch.len();

        if self.fam == Fam::Diff {
            let t = if s.starts_with("+++") || s.starts_with("---") {
                Tok::Kw
            } else if s.starts_with('+') {
                Tok::Add
            } else if s.starts_with('-') {
                Tok::Del
            } else if s.starts_with("@@") {
                Tok::Hunk
            } else {
                Tok::Plain
            };
            push(t, s.to_string());
            return out;
        }
        if self.fam == Fam::Plain {
            push(Tok::Plain, s.to_string());
            return out;
        }

        let slash_comments = matches!(self.fam, Fam::Rust | Fam::Js | Fam::Go | Fam::C);
        let hash_comments = matches!(self.fam, Fam::Py | Fam::Sh | Fam::Toml);
        let mut i = 0usize;

        // continuation of a multi-line construct
        if self.block {
            let mut j = 0;
            while j + 1 < n && !(ch[j] == '*' && ch[j + 1] == '/') {
                j += 1;
            }
            if j + 1 < n {
                push(Tok::Comment, ch[..j + 2].iter().collect());
                i = j + 2;
                self.block = false;
            } else {
                push(Tok::Comment, s.to_string());
                return out;
            }
        } else if let Some(q) = self.triple {
            let mut j = 0;
            while j + 2 < n && !(ch[j] == q && ch[j + 1] == q && ch[j + 2] == q) {
                j += 1;
            }
            if j + 2 < n {
                push(Tok::Str, ch[..j + 3].iter().collect());
                i = j + 3;
                self.triple = None;
            } else {
                push(Tok::Str, s.to_string());
                return out;
            }
        }

        // toml/yaml: key at line start
        if self.fam == Fam::Toml {
            let trimmed = s.trim_start();
            if trimmed.starts_with('[') && !trimmed.starts_with("[[") || trimmed.starts_with("[[") {
                push(Tok::Kw, s.to_string());
                return out;
            }
            let lead = n - trimmed.chars().count();
            if let Some(pos) = trimmed.find(|c| c == '=' || c == ':') {
                let key = &trimmed[..pos];
                if !key.is_empty()
                    && !key.contains('"')
                    && !key.starts_with('#')
                    && !key.starts_with('-')
                {
                    push(Tok::Plain, " ".repeat(lead));
                    push(Tok::Func, key.to_string());
                    i = lead + key.chars().count();
                }
            }
        }

        let kws = self.keywords();
        while i < n {
            let c = ch[i];
            // comments
            if slash_comments && c == '/' && i + 1 < n && ch[i + 1] == '/' {
                push(Tok::Comment, ch[i..].iter().collect());
                break;
            }
            if slash_comments && c == '/' && i + 1 < n && ch[i + 1] == '*' {
                let mut j = i + 2;
                while j + 1 < n && !(ch[j] == '*' && ch[j + 1] == '/') {
                    j += 1;
                }
                if j + 1 < n {
                    push(Tok::Comment, ch[i..j + 2].iter().collect());
                    i = j + 2;
                    continue;
                }
                push(Tok::Comment, ch[i..].iter().collect());
                self.block = true;
                break;
            }
            if hash_comments && c == '#' && (i == 0 || ch[i - 1].is_whitespace()) {
                push(Tok::Comment, ch[i..].iter().collect());
                break;
            }
            if self.fam == Fam::Sql && c == '-' && i + 1 < n && ch[i + 1] == '-' {
                push(Tok::Comment, ch[i..].iter().collect());
                break;
            }
            // python triple quote
            if self.fam == Fam::Py
                && (c == '"' || c == '\'')
                && i + 2 < n
                && ch[i + 1] == c
                && ch[i + 2] == c
            {
                let mut j = i + 3;
                while j + 2 < n && !(ch[j] == c && ch[j + 1] == c && ch[j + 2] == c) {
                    j += 1;
                }
                if j + 2 < n {
                    push(Tok::Str, ch[i..j + 3].iter().collect());
                    i = j + 3;
                    continue;
                }
                push(Tok::Str, ch[i..].iter().collect());
                self.triple = Some(c);
                break;
            }
            // strings
            if c == '"' || c == '`' || (c == '\'' && self.fam != Fam::Sql) {
                if c == '\'' && self.fam == Fam::Rust {
                    // lifetime ('a) vs char literal ('a')
                    let life = i + 1 < n
                        && (ch[i + 1].is_alphabetic() || ch[i + 1] == '_')
                        && !(i + 2 < n && ch[i + 2] == '\'');
                    if life {
                        let mut j = i + 1;
                        while j < n && (ch[j].is_alphanumeric() || ch[j] == '_') {
                            j += 1;
                        }
                        push(Tok::Ty, ch[i..j].iter().collect());
                        i = j;
                        continue;
                    }
                }
                let mut j = i + 1;
                while j < n && ch[j] != c {
                    if ch[j] == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
                let end = (j + 1).min(n);
                let text: String = ch[i..end].iter().collect();
                // json object key
                let is_key = self.fam == Fam::Json
                    && ch[end..].iter().find(|x| !x.is_whitespace()) == Some(&':');
                push(if is_key { Tok::Func } else { Tok::Str }, text);
                i = end;
                continue;
            }
            // numbers
            if c.is_ascii_digit() && (i == 0 || !(ch[i - 1].is_alphanumeric() || ch[i - 1] == '_'))
            {
                let mut j = i;
                while j < n && (ch[j].is_ascii_alphanumeric() || ch[j] == '_' || ch[j] == '.') {
                    j += 1;
                }
                push(Tok::Num, ch[i..j].iter().collect());
                i = j;
                continue;
            }
            // shell variables
            if self.fam == Fam::Sh && c == '$' {
                let mut j = i + 1;
                if j < n && ch[j] == '{' {
                    while j < n && ch[j] != '}' {
                        j += 1;
                    }
                    j = (j + 1).min(n);
                } else {
                    while j < n && (ch[j].is_alphanumeric() || ch[j] == '_') {
                        j += 1;
                    }
                }
                push(Tok::Ty, ch[i..j].iter().collect());
                i = j;
                continue;
            }
            // identifiers
            if c.is_alphabetic() || c == '_' {
                let mut j = i;
                while j < n && (ch[j].is_alphanumeric() || ch[j] == '_') {
                    j += 1;
                }
                let word: String = ch[i..j].iter().collect();
                let next = ch.get(j).copied();
                let t = if kws.contains(&word.as_str()) {
                    Tok::Kw
                } else if next == Some('(') || (self.fam == Fam::Rust && next == Some('!')) {
                    Tok::Func
                } else if word.chars().next().is_some_and(|f| f.is_uppercase())
                    && word.chars().any(|x| x.is_lowercase())
                {
                    Tok::Ty
                } else {
                    Tok::Plain
                };
                push(t, word);
                i = j;
                continue;
            }
            push(Tok::Plain, c.to_string());
            i += 1;
        }
        out
    }
}

pub fn expand_tabs(s: &str) -> String {
    s.replace('\t', "    ").replace('\r', "")
}

/// Cut `skip` chars off the left and fit the rest into `max` columns of spans.
pub fn to_spans(tokens: &[(Tok, String)], skip: usize, max: usize) -> Vec<Span<'static>> {
    let mut out = vec![];
    let mut skipped = 0usize;
    let mut used = 0usize;
    for (t, text) in tokens {
        let mut piece = String::new();
        for c in text.chars() {
            if skipped < skip {
                skipped += 1;
                continue;
            }
            if used >= max {
                break;
            }
            piece.push(c);
            used += 1;
        }
        if !piece.is_empty() {
            out.push(Span::styled(piece, style(*t)));
        }
        if used >= max {
            break;
        }
    }
    out
}

#[allow(dead_code)]
pub fn token_len(tokens: &[(Tok, String)]) -> usize {
    tokens.iter().map(|(_, s)| s.chars().count()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(tag: &str, line: &str) -> Vec<Tok> {
        Highlighter::new(tag)
            .line(line)
            .into_iter()
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn rust_fn_line() {
        let k = kinds("rust", "pub fn main() { // hi");
        assert_eq!(k[0], Tok::Kw);
        assert!(k.contains(&Tok::Func));
        assert_eq!(*k.last().unwrap(), Tok::Comment);
    }

    #[test]
    fn block_comment_spans_lines() {
        let mut h = Highlighter::new("c");
        assert_eq!(h.line("/* open")[0].0, Tok::Comment);
        assert_eq!(h.line("still */ int x")[0].0, Tok::Comment);
        assert!(h.line("int y;").iter().any(|(t, _)| *t == Tok::Kw));
    }

    #[test]
    fn rust_lifetime_is_not_a_string() {
        let k = kinds("rs", "fn f<'a>(x: &'a str)");
        assert!(!k.contains(&Tok::Str));
    }

    #[test]
    fn diff_lines() {
        assert_eq!(kinds("diff", "+added"), vec![Tok::Add]);
        assert_eq!(kinds("diff", "-gone"), vec![Tok::Del]);
    }
}
