//! Web and planning tools: `web_fetch`, `web_search`, `todo_write`.
//!
//! `web_search` needs no key by default (DuckDuckGo's HTML endpoint); set
//! `BRAVE_API_KEY`, `TAVILY_API_KEY` or `SEARXNG_URL` for a real search API.

use std::time::Duration;

use serde_json::{Value, json};

use crate::{BoxFuture, Tool, ToolContext, ToolInvocation, ToolOutput};

const DEFAULT_FETCH_CHARS: usize = 40_000;
const MAX_FETCH_CHARS: usize = 200_000;

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("destro-agent/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| e.to_string())
}

// ----- HTML -> text --------------------------------------------------------------------------

/// Decode the common named entities and numeric references.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "hellip" => Some('…'),
            "copy" => Some('©'),
            n if n.starts_with("#x") || n.starts_with("#X") => u32::from_str_radix(&n[2..], 16)
                .ok()
                .and_then(char::from_u32),
            n if n.starts_with('#') => n[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The value of attribute `name` inside one tag's text, quotes optional.
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        let before_ok = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric();
        let after = tag[at + name.len()..].trim_start();
        if before_ok && let Some(v) = after.strip_prefix('=') {
            let v = v.trim_start();
            return Some(match v.chars().next() {
                Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or("").to_string(),
                _ => v
                    .split(|c: char| c.is_whitespace() || c == '>')
                    .next()
                    .unwrap_or("")
                    .to_string(),
            });
        }
        from = at + name.len();
    }
    None
}

/// A readable plain-text rendering of an HTML page.
pub fn html_to_text(html: &str) -> String {
    const SKIP: [&str; 6] = ["script", "style", "noscript", "head", "svg", "template"];
    const BLOCK: [&str; 22] = [
        "p",
        "div",
        "br",
        "tr",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "ul",
        "ol",
        "table",
        "section",
        "article",
        "header",
        "footer",
        "nav",
        "blockquote",
        "form",
        "hr",
        "dl",
    ];
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0;
    let mut skip: Option<&str> = None;
    let mut pre = false;
    let mut hrefs: Vec<Option<String>> = vec![];
    let b = html.as_bytes();
    while i < b.len() {
        if let Some(name) = skip {
            // inside <script>/<style>…: only the matching close tag matters, and the
            // body may contain a bare `<` (comparisons), so do not tokenise it
            let close = format!("</{name}");
            let found = html[i..].to_ascii_lowercase().find(&close);
            match found {
                Some(n) => {
                    let at = i + n;
                    i = html[at..].find('>').map(|m| at + m + 1).unwrap_or(b.len());
                    skip = None;
                }
                None => break,
            }
            continue;
        }
        if b[i] != b'<' {
            let next = html[i..].find('<').map(|n| i + n).unwrap_or(b.len());
            if skip.is_none() {
                let text = decode_entities(&html[i..next]);
                if pre {
                    // keep indentation through the tidy pass below
                    out.push_str(&text.replace(' ', "\u{E000}"));
                } else {
                    let mut last_space = out.ends_with([' ', '\n']) || out.is_empty();
                    for c in text.chars() {
                        if c.is_whitespace() {
                            if !last_space {
                                out.push(' ');
                                last_space = true;
                            }
                        } else {
                            out.push(c);
                            last_space = false;
                        }
                    }
                }
            }
            i = next;
            continue;
        }
        if html[i..].starts_with("<!--") {
            i = html[i..].find("-->").map(|n| i + n + 3).unwrap_or(b.len());
            continue;
        }
        let Some(end) = html[i..].find('>').map(|n| i + n) else {
            break;
        };
        let inner = &html[i + 1..end];
        i = end + 1;
        let closing = inner.starts_with('/');
        let name: String = inner
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if let Some(s) = skip {
            if closing && name == s {
                skip = None;
            }
            continue;
        }
        if !closing && SKIP.contains(&name.as_str()) && !inner.ends_with('/') {
            skip = SKIP.iter().find(|s| **s == name).copied();
            continue;
        }
        match (name.as_str(), closing) {
            ("pre", false) => {
                pre = true;
                out.push('\n');
            }
            ("pre", true) => {
                pre = false;
                out.push('\n');
            }
            ("li", false) => out.push_str("\n- "),
            ("a", false) => hrefs.push(attr(inner, "href")),
            ("a", true) => {
                if let Some(Some(h)) = hrefs.pop()
                    && !h.is_empty()
                    && !h.starts_with('#')
                    && !h.to_ascii_lowercase().starts_with("javascript:")
                {
                    out.push_str(&format!(" ({})", decode_entities(&h)));
                }
            }
            (n, _) if BLOCK.contains(&n) => out.push('\n'),
            _ => {}
        }
    }
    // tidy: trim line ends, at most one blank line in a row
    let mut tidy = String::with_capacity(out.len());
    let mut blank = 0;
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        tidy.push_str(line);
        tidy.push('\n');
    }
    tidy.trim().replace('\u{E000}', " ")
}

fn truncate_chars(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}\n\n[truncated: {} more characters]", total - max)
}

// ----- web_fetch -----------------------------------------------------------------------------

/// Fetches a URL and returns readable text.
#[derive(Clone, Copy, Debug, Default)]
pub struct WebFetchTool;

impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a web page or API URL (http/https GET) and return its content as readable text. \
         HTML is converted to plain text with link targets kept; JSON and text pass through. \
         Use it to read documentation or a page found with web_search. Output is truncated to \
         max_chars (default 40000)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "http:// or https:// URL." },
                "max_chars": { "type": "integer", "description": "Truncate output to this many characters (max 200000)." }
            },
            "required": ["url"]
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        _cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let Some(url) = inv.arguments["url"].as_str() else {
                return ToolOutput::err("missing required argument `url`");
            };
            let max = inv.arguments["max_chars"]
                .as_u64()
                .map(|n| (n as usize).clamp(500, MAX_FETCH_CHARS))
                .unwrap_or(DEFAULT_FETCH_CHARS);
            match fetch(url, max).await {
                Ok(t) => ToolOutput::ok(t),
                Err(e) => ToolOutput::err(e),
            }
        })
    }
}

async fn fetch(url: &str, max: usize) -> Result<String, String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("only http:// and https:// URLs are supported".into());
    }
    let resp = client()?
        .get(url)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/json,text/plain,*/*;q=0.8",
        )
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let final_url = resp.url().to_string();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !status.is_success() {
        return Err(format!("HTTP {status} for {final_url}"));
    }
    let textual = ctype.is_empty()
        || ["text/", "json", "xml", "javascript", "x-www-form"]
            .iter()
            .any(|t| ctype.contains(t));
    if !textual {
        return Err(format!("not a text content type: {ctype}"));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    let body = String::from_utf8_lossy(&bytes[..bytes.len().min(4 * 1024 * 1024)]).to_string();
    let text = if ctype.contains("html") || (ctype.is_empty() && body.trim_start().starts_with('<'))
    {
        html_to_text(&body)
    } else {
        body
    };
    Ok(format!(
        "URL: {final_url}\n\n{}",
        truncate_chars(&text, max)
    ))
}

// ----- web_search ----------------------------------------------------------------------------

/// One search result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Page title.
    pub title: String,
    /// Result URL.
    pub url: String,
    /// Short excerpt.
    pub snippet: String,
}

fn hits_from_json(v: &Value, key: &str, snippet_key: &str) -> Vec<Hit> {
    let arr = key
        .split('.')
        .try_fold(v, |acc, k| acc.get(k))
        .and_then(Value::as_array);
    arr.into_iter()
        .flatten()
        .filter_map(|r| {
            Some(Hit {
                title: r["title"].as_str()?.to_string(),
                url: r["url"].as_str()?.to_string(),
                snippet: r[snippet_key].as_str().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// Brave Search API response.
pub fn parse_brave(v: &Value) -> Vec<Hit> {
    hits_from_json(v, "web.results", "description")
}

/// Tavily response.
pub fn parse_tavily(v: &Value) -> Vec<Hit> {
    hits_from_json(v, "results", "content")
}

/// SearXNG JSON response.
pub fn parse_searxng(v: &Value) -> Vec<Hit> {
    hits_from_json(v, "results", "content")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// DuckDuckGo redirect links carry the real target in `uddg`.
fn ddg_target(href: &str) -> String {
    let href = decode_entities(href);
    let full = if let Some(rest) = href.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        href.clone()
    };
    if let Ok(u) = reqwest::Url::parse(&full)
        && let Some((_, v)) = u.query_pairs().find(|(k, _)| k == "uddg")
    {
        return v.into_owned();
    }
    full
}

/// DuckDuckGo's HTML results page.
pub fn parse_ddg(html: &str) -> Vec<Hit> {
    let mut hits = vec![];
    let mut from = 0;
    while let Some(i) = html[from..].find("result__a") {
        let at = from + i;
        let Some(tag_start) = html[..at].rfind("<a") else {
            from = at + 9;
            continue;
        };
        let Some(tag_end) = html[at..].find('>').map(|n| at + n) else {
            break;
        };
        let tag = &html[tag_start..tag_end];
        let close = html[tag_end..]
            .find("</a>")
            .map(|n| tag_end + n)
            .unwrap_or(html.len());
        let title = strip_tags(&html[tag_end + 1..close]);
        let url = attr(tag, "href")
            .map(|h| ddg_target(&h))
            .unwrap_or_default();
        // the snippet sits before the next result's anchor
        let next = html[close..]
            .find("result__a")
            .map(|n| close + n)
            .unwrap_or(html.len());
        let snippet = html[close..next]
            .find("result__snippet")
            .and_then(|n| {
                let s = close + n;
                let open = html[s..].find('>')? + s + 1;
                let end = ["</a>", "</div>", "</td>"]
                    .iter()
                    .filter_map(|t| html[open..].find(t))
                    .min()?
                    + open;
                Some(strip_tags(&html[open..end]))
            })
            .unwrap_or_default();
        if !title.is_empty() && !url.is_empty() {
            hits.push(Hit {
                title,
                url,
                snippet,
            });
        }
        from = close.max(at + 9);
    }
    hits
}

/// Searches the web and lists results.
#[derive(Clone, Copy, Debug, Default)]
pub struct WebSearchTool;

impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web and return titled results with URLs and snippets. Use it for current \
         information, documentation and anything not in the workspace; then read promising \
         results with web_fetch. Keep queries specific."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to search for." },
                "max_results": { "type": "integer", "description": "How many results (default 8, max 20)." }
            },
            "required": ["query"]
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        _cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let Some(query) = inv.arguments["query"]
                .as_str()
                .filter(|q| !q.trim().is_empty())
            else {
                return ToolOutput::err("missing required argument `query`");
            };
            let max = inv.arguments["max_results"]
                .as_u64()
                .map(|n| (n as usize).clamp(1, 20))
                .unwrap_or(8);
            match search(query.trim(), max).await {
                Ok((backend, hits)) if hits.is_empty() => {
                    ToolOutput::ok(format!("no results (via {backend})"))
                }
                Ok((backend, hits)) => ToolOutput::ok(format_hits(&hits, backend)),
                Err(e) => ToolOutput::err(e),
            }
        })
    }
}

fn format_hits(hits: &[Hit], backend: &str) -> String {
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, h.title, h.url));
        if !h.snippet.is_empty() {
            out.push_str(&format!("   {}\n", h.snippet));
        }
    }
    out.push_str(&format!("(via {backend})"));
    out
}

fn with_params(base: &str, params: &[(&str, &str)]) -> Result<reqwest::Url, String> {
    reqwest::Url::parse_with_params(base, params).map_err(|e| format!("bad search URL: {e}"))
}

/// `application/x-www-form-urlencoded` body.
fn form_body(params: &[(&str, &str)]) -> String {
    let mut u = reqwest::Url::parse("http://form.invalid/").expect("static url");
    u.query_pairs_mut().extend_pairs(params);
    u.query().unwrap_or("").to_string()
}

fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

async fn search(query: &str, max: usize) -> Result<(&'static str, Vec<Hit>), String> {
    let http = client()?;
    let fail = |e: reqwest::Error| format!("search request failed: {e}");
    let (name, mut hits) = if let Some(key) = env_nonempty("BRAVE_API_KEY") {
        let url = with_params(
            "https://api.search.brave.com/res/v1/web/search",
            &[("q", query), ("count", &max.to_string())],
        )?;
        let v: Value = http
            .get(url)
            .header("X-Subscription-Token", key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(fail)?
            .error_for_status()
            .map_err(fail)?
            .json()
            .await
            .map_err(fail)?;
        ("brave", parse_brave(&v))
    } else if let Some(key) = env_nonempty("TAVILY_API_KEY") {
        let v: Value = http
            .post("https://api.tavily.com/search")
            .json(&json!({"api_key": key, "query": query, "max_results": max}))
            .send()
            .await
            .map_err(fail)?
            .error_for_status()
            .map_err(fail)?
            .json()
            .await
            .map_err(fail)?;
        ("tavily", parse_tavily(&v))
    } else if let Some(base) = env_nonempty("SEARXNG_URL") {
        let url = with_params(
            &format!("{}/search", base.trim_end_matches('/')),
            &[("q", query), ("format", "json")],
        )?;
        let v: Value = http
            .get(url)
            .send()
            .await
            .map_err(fail)?
            .error_for_status()
            .map_err(fail)?
            .json()
            .await
            .map_err(fail)?;
        ("searxng", parse_searxng(&v))
    } else {
        let html = http
            .post("https://html.duckduckgo.com/html/")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form_body(&[("q", query)]))
            .send()
            .await
            .map_err(fail)?
            .error_for_status()
            .map_err(fail)?
            .text()
            .await
            .map_err(fail)?;
        ("duckduckgo", parse_ddg(&html))
    };
    hits.truncate(max);
    Ok((name, hits))
}

// ----- todo_write ----------------------------------------------------------------------------

/// Keeps a visible checklist: the model sends the whole list each time.
#[derive(Clone, Copy, Debug, Default)]
pub struct TodoWriteTool;

impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn description(&self) -> &str {
        "Maintain a checklist for a multi-step task. Send the complete list every time, with \
         each item's status (pending, in_progress, completed). Keep one item in_progress at a \
         time and mark items completed as soon as they are done."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                            "id": { "type": "string" }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn run<'a>(
        &'a self,
        inv: &'a ToolInvocation,
        _cx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move { render_todos(&inv.arguments) })
    }
}

fn render_todos(args: &Value) -> ToolOutput {
    let Some(items) = args["todos"].as_array() else {
        return ToolOutput::err("`todos` must be an array of {content, status}");
    };
    let mut lines = vec![];
    let mut active = 0;
    for (i, t) in items.iter().enumerate() {
        let Some(content) = t["content"].as_str().filter(|c| !c.trim().is_empty()) else {
            return ToolOutput::err(format!("todo {} has no content", i + 1));
        };
        let mark = match t["status"].as_str() {
            Some("pending") => "[ ]",
            Some("in_progress") => {
                active += 1;
                "[~]"
            }
            Some("completed") => "[x]",
            other => {
                return ToolOutput::err(format!(
                    "todo {} has invalid status {:?}; use pending, in_progress or completed",
                    i + 1,
                    other.unwrap_or("")
                ));
            }
        };
        lines.push(format!("{mark} {content}"));
    }
    let mut out = format!(
        "todo list updated ({} items)\n{}",
        items.len(),
        lines.join("\n")
    );
    if active > 1 {
        out.push_str("\nnote: more than one item is in_progress; focus on one at a time");
    }
    ToolOutput::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_readable_text() {
        let t = html_to_text(
            "<html><head><title>x</title><style>p{}</style></head><body>\
             <h1>Title &amp; more</h1><p>Hello   <b>world</b>.</p>\
             <script>var a = 1 < 2;</script><ul><li>one</li><li>two <a href=\"https://e.com/x\">link</a></li></ul>\
             <!-- hidden --><pre>a  b\n c</pre><a href=\"#top\">top</a></body></html>",
        );
        assert!(t.starts_with("Title & more"), "{t}");
        assert!(t.contains("Hello world."));
        assert!(
            t.contains("- one") && t.contains("- two link (https://e.com/x)"),
            "{t:?}"
        );
        assert!(t.contains("a  b\n c"), "pre keeps whitespace: {t}");
        assert!(!t.contains("var a") && !t.contains("hidden") && !t.contains("p{}"));
        assert!(!t.contains("(#top)"));
    }

    #[test]
    fn entities_decode() {
        assert_eq!(
            decode_entities("a &lt;b&gt; &amp; &#65;&#x42; &bogus; &"),
            "a <b> & AB &bogus; &"
        );
    }

    #[test]
    fn truncation_reports_the_rest() {
        let t = truncate_chars(&"x".repeat(50), 10);
        assert!(t.starts_with("xxxxxxxxxx") && t.contains("40 more characters"));
        assert_eq!(truncate_chars("short", 10), "short");
    }

    #[test]
    fn duckduckgo_results_parse_with_real_targets() {
        let html = r#"
          <div class="result results_links">
            <h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=abc">Rust <b>Programming</b> Language</a></h2>
            <a class="result__snippet" href="//duckduckgo.com/l/?uddg=x">A language empowering everyone to build <b>reliable</b> software.</a>
          </div>
          <div class="result">
            <a class="result__a" href="https://doc.rust-lang.org/book/">The Book &amp; more</a>
            <div class="result__snippet">Learn Rust.</div>
          </div>"#;
        let hits = parse_ddg(html);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust Programming Language");
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
        assert_eq!(
            hits[0].snippet,
            "A language empowering everyone to build reliable software."
        );
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/book/");
        assert_eq!(hits[1].title, "The Book & more");
        assert_eq!(hits[1].snippet, "Learn Rust.");
    }

    #[test]
    fn api_backends_parse() {
        let brave = json!({"web":{"results":[{"title":"T","url":"https://a","description":"D"}]}});
        assert_eq!(
            parse_brave(&brave),
            vec![Hit {
                title: "T".into(),
                url: "https://a".into(),
                snippet: "D".into()
            }]
        );
        let tav =
            json!({"results":[{"title":"T","url":"https://b","content":"C"},{"url":"no title"}]});
        assert_eq!(parse_tavily(&tav).len(), 1);
        let sx = json!({"results":[{"title":"S","url":"https://c","content":"X"}]});
        assert_eq!(parse_searxng(&sx)[0].snippet, "X");
        assert!(parse_brave(&json!({})).is_empty());
    }

    #[test]
    fn form_and_query_encoding() {
        assert_eq!(form_body(&[("q", "rust & go")]), "q=rust+%26+go");
        let u = with_params("https://x.test/s", &[("q", "a b"), ("n", "3")]).unwrap();
        assert_eq!(u.as_str(), "https://x.test/s?q=a+b&n=3");
    }

    #[test]
    fn hits_format_as_a_numbered_list() {
        let s = format_hits(
            &[Hit {
                title: "T".into(),
                url: "https://a".into(),
                snippet: "S".into(),
            }],
            "x",
        );
        assert_eq!(s, "1. T\n   https://a\n   S\n(via x)");
    }

    #[test]
    fn todo_list_validates_and_renders() {
        let ok = render_todos(&json!({"todos":[
            {"content":"a","status":"completed"},
            {"content":"b","status":"in_progress"},
            {"content":"c","status":"pending"}]}));
        assert!(!ok.is_error);
        assert_eq!(
            ok.content,
            "todo list updated (3 items)\n[x] a\n[~] b\n[ ] c"
        );
        assert!(render_todos(&json!({"todos":[{"content":"a","status":"bogus"}]})).is_error);
        assert!(render_todos(&json!({"todos":[{"content":" ","status":"pending"}]})).is_error);
        assert!(render_todos(&json!({})).is_error);
        let two = render_todos(&json!({"todos":[
            {"content":"a","status":"in_progress"},{"content":"b","status":"in_progress"}]}));
        assert!(two.content.contains("more than one"));
    }
}
