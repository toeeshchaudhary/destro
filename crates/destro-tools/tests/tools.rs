//! Integration tests for the built-in tools.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use destro_tools::tools::{EchoTool, EditTool, GlobTool, GrepTool, PwshTool, ReadTool, WriteTool};
use destro_tools::{Tool, ToolContext, ToolInvocation, ToolOutput};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temporary directory removed on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "destro-tools-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn ctx(&self) -> ToolContext {
        ToolContext {
            cwd: self.path.clone(),
            agent_id: "agent-000000".into(),
        }
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.path.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

async fn call(tool: &dyn Tool, cx: &ToolContext, arguments: serde_json::Value) -> ToolOutput {
    let inv = ToolInvocation {
        call_id: "test-call".to_owned(),
        arguments,
    };
    tool.run(&inv, cx).await
}

/// Returns whether `pwsh` can be spawned in this environment.
fn pwsh_available() -> bool {
    ["pwsh", "powershell.exe"].iter().any(|exe| {
        std::process::Command::new(exe)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "exit 0",
            ])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

#[tokio::test]
async fn read_numbers_lines_and_honours_offset_and_limit() {
    let temp = TempDir::new();
    temp.write("lines.txt", "line 1\nline 2\nline 3\nline 4\nline 5\n");
    let cx = temp.ctx();

    let output = call(
        &ReadTool::new(),
        &cx,
        serde_json::json!({"path": "lines.txt", "limit": 2}),
    )
    .await;
    assert!(!output.is_error, "{}", output.content);
    assert!(output.content.contains("     1 line 1\n"));
    assert!(output.content.contains("     2 line 2\n"));
    assert!(!output.content.contains("line 3"));
    assert!(output.content.contains("[total lines: 5]"));

    let output = call(
        &ReadTool::new(),
        &cx,
        serde_json::json!({"path": "lines.txt", "offset": 4}),
    )
    .await;
    assert!(output.content.contains("     4 line 4"));
    assert!(output.content.contains("     5 line 5"));
    assert!(!output.content.contains("[total lines: 5]"));
}

#[tokio::test]
async fn read_caps_output_and_reports_byte_counts() {
    let temp = TempDir::new();
    let mut content = String::new();
    for index in 0..3000 {
        content.push_str(&format!("{index:0>4} {}\n", "x".repeat(190)));
    }
    temp.write("big.txt", &content);
    let output = call(
        &ReadTool::new(),
        &temp.ctx(),
        serde_json::json!({"path": "big.txt"}),
    )
    .await;
    assert!(!output.is_error);
    assert!(output.content.contains("[truncated: showing "));
    assert!(output.content.contains(" bytes]"));
    let head = output
        .content
        .split('\n')
        .take_while(|line| !line.starts_with('['))
        .count();
    assert!(head > 1000, "expected many lines, got {head}");
    assert!(
        output.content.len() < 256 * 1024 + 200,
        "output was {} bytes",
        output.content.len()
    );
}

#[tokio::test]
async fn read_reports_missing_files_and_directories() {
    let temp = TempDir::new();
    let cx = temp.ctx();
    let missing = call(
        &ReadTool::new(),
        &cx,
        serde_json::json!({"path": "nope.txt"}),
    )
    .await;
    assert!(missing.is_error);
    assert!(
        missing.content.contains("cannot read"),
        "{}",
        missing.content
    );

    std::fs::create_dir_all(temp.path.join("dir")).unwrap();
    let directory = call(&ReadTool::new(), &cx, serde_json::json!({"path": "dir"})).await;
    assert!(directory.is_error);
    assert!(directory.content.contains("not a file"));
}

#[tokio::test]
async fn read_absolute_paths_pass_through() {
    let temp = TempDir::new();
    let path = temp.write("abs.txt", "hello\n");
    let output = call(
        &ReadTool::new(),
        &temp.ctx(),
        serde_json::json!({"path": path.to_string_lossy()}),
    )
    .await;
    assert!(!output.is_error, "{}", output.content);
    assert!(output.content.contains("hello"));
}

#[tokio::test]
async fn write_creates_parent_directories_and_reports_size() {
    let temp = TempDir::new();
    let cx = temp.ctx();
    let output = call(
        &WriteTool::new(),
        &cx,
        serde_json::json!({"path": "nested/deep/a.txt", "content": "abcdé"}),
    )
    .await;
    assert!(!output.is_error, "{}", output.content);
    assert!(output.content.starts_with("wrote 6 bytes to "));
    let written = std::fs::read_to_string(temp.path.join("nested/deep/a.txt")).unwrap();
    assert_eq!(written, "abcdé");
}

#[tokio::test]
async fn edit_round_trip_handles_ambiguous_and_missing_strings() {
    let temp = TempDir::new();
    temp.write("doc.txt", "alpha beta gamma beta");
    let cx = temp.ctx();
    let tool = EditTool::new();

    let ambiguous = call(
        &tool,
        &cx,
        serde_json::json!({"path": "doc.txt", "old_string": "beta", "new_string": "BETA"}),
    )
    .await;
    assert!(ambiguous.is_error);
    assert!(
        ambiguous.content.contains("2 times"),
        "{}",
        ambiguous.content
    );
    assert!(
        std::fs::read_to_string(temp.path.join("doc.txt"))
            .unwrap()
            .contains("beta")
    );

    let all = call(
        &tool,
        &cx,
        serde_json::json!({
            "path": "doc.txt",
            "old_string": "beta",
            "new_string": "BETA",
            "replace_all": true
        }),
    )
    .await;
    assert!(!all.is_error, "{}", all.content);
    assert!(all.content.contains("replaced 2 occurrence(s)"));
    assert_eq!(
        std::fs::read_to_string(temp.path.join("doc.txt")).unwrap(),
        "alpha BETA gamma BETA"
    );

    let single = call(
        &tool,
        &cx,
        serde_json::json!({"path": "doc.txt", "old_string": "alpha", "new_string": "ALPHA"}),
    )
    .await;
    assert!(!single.is_error);
    assert!(single.content.contains("replaced 1 occurrence(s)"));

    let absent = call(
        &tool,
        &cx,
        serde_json::json!({"path": "doc.txt", "old_string": "zeta", "new_string": "Z"}),
    )
    .await;
    assert!(absent.is_error);
    assert!(absent.content.contains("old_string not found"));

    let empty = call(
        &tool,
        &cx,
        serde_json::json!({"path": "doc.txt", "old_string": "", "new_string": "Z"}),
    )
    .await;
    assert!(empty.is_error);
}

#[tokio::test]
async fn glob_returns_sorted_matches_and_caps_at_one_hundred() {
    let temp = TempDir::new();
    for index in 0..130 {
        temp.write(&format!("files/f{index:0>3}.txt"), "x");
    }
    temp.write("files/skip.rs", "x");
    let output = call(
        &GlobTool::new(),
        &temp.ctx(),
        serde_json::json!({"pattern": "*.txt"}),
    )
    .await;
    assert!(!output.is_error, "{}", output.content);
    assert!(
        output
            .content
            .contains("[... more matches truncated at 100]")
    );
    let listed: Vec<&str> = output
        .content
        .lines()
        .filter(|line| line.ends_with(".txt"))
        .collect();
    assert_eq!(listed.len(), 100);
    let mut sorted = listed.clone();
    sorted.sort_unstable();
    assert_eq!(listed, sorted);
    assert!(listed[0].contains("files"));
}

#[tokio::test]
async fn glob_reports_no_matches_as_success() {
    let temp = TempDir::new();
    temp.write("a.txt", "x");
    let output = call(
        &GlobTool::new(),
        &temp.ctx(),
        serde_json::json!({"pattern": "*.zzz"}),
    )
    .await;
    assert!(!output.is_error);
    assert!(output.content.contains("no files matched *.zzz"));
}

#[tokio::test]
async fn grep_filters_by_include_and_caps_results() {
    let temp = TempDir::new();
    let mut lines = String::new();
    for index in 0..300 {
        lines.push_str(&format!("needle {index}\n"));
    }
    temp.write("src/a.rs", &lines);
    temp.write("src/a.txt", "needle\n");
    let cx = temp.ctx();

    let filtered = call(
        &GrepTool::new(),
        &cx,
        serde_json::json!({"pattern": "needle", "include": "*.rs"}),
    )
    .await;
    assert!(!filtered.is_error, "{}", filtered.content);
    assert!(
        filtered
            .content
            .contains("[... truncated: 250 matches shown]")
    );
    let listed: Vec<&str> = filtered
        .content
        .lines()
        .filter(|line| !line.starts_with('['))
        .collect();
    assert_eq!(listed.len(), 250);
    assert!(listed.iter().all(|line| line.contains("a.rs:")));
    assert!(!filtered.content.contains("a.txt"));

    let case_insensitive = call(
        &GrepTool::new(),
        &cx,
        serde_json::json!({"pattern": "NEEDLE", "include": "*.txt", "case_insensitive": true}),
    )
    .await;
    assert!(case_insensitive.content.contains("a.txt:1: needle"));
}

#[tokio::test]
async fn grep_reports_invalid_regex_and_no_matches() {
    let temp = TempDir::new();
    temp.write("a.txt", "hello\n");
    let cx = temp.ctx();
    let invalid = call(&GrepTool::new(), &cx, serde_json::json!({"pattern": "("})).await;
    assert!(invalid.is_error);
    assert!(
        invalid.content.contains("invalid regex"),
        "{}",
        invalid.content
    );

    let none = call(&GrepTool::new(), &cx, serde_json::json!({"pattern": "zzz"})).await;
    assert!(!none.is_error);
    assert!(none.content.contains("no matches for zzz"));
}

#[tokio::test]
async fn grep_skips_binary_files_and_trims_long_lines() {
    let temp = TempDir::new();
    std::fs::write(temp.path.join("bin.dat"), b"needle\0needle\n").unwrap();
    temp.write("long.txt", &format!("needle {}\n", "y".repeat(5000)));
    let output = call(
        &GrepTool::new(),
        &temp.ctx(),
        serde_json::json!({"pattern": "needle"}),
    )
    .await;
    assert!(!output.is_error, "{}", output.content);
    assert!(output.content.contains("long.txt:1: "));
    assert!(!output.content.contains("bin.dat:"));
    let line = output
        .content
        .lines()
        .find(|line| line.starts_with("long.txt:"))
        .unwrap();
    assert!(
        line.len() <= 4 * 1024 + "long.txt:1: ".len() + 2,
        "line was {}",
        line.len()
    );
}

#[tokio::test]
async fn pwsh_runs_commands_and_reports_non_zero_exit() {
    if !pwsh_available() {
        eprintln!("skipping: pwsh is not on PATH");
        return;
    }
    let temp = TempDir::new();
    let cx = temp.ctx();
    let ok = call(
        &PwshTool::new(),
        &cx,
        serde_json::json!({"command": "Write-Output 'hello from pwsh'"}),
    )
    .await;
    assert!(!ok.is_error, "{}", ok.content);
    assert!(ok.content.contains("hello from pwsh"));

    let failed = call(
        &PwshTool::new(),
        &cx,
        serde_json::json!({"command": "Write-Output 'partial'; exit 3"}),
    )
    .await;
    assert!(failed.is_error);
    assert!(failed.content.contains("partial"));
    assert!(
        failed.content.contains("[exit code: 3]"),
        "{}",
        failed.content
    );
}

#[tokio::test]
async fn pwsh_times_out_and_kills_the_child() {
    if !pwsh_available() {
        eprintln!("skipping: pwsh is not on PATH");
        return;
    }
    let temp = TempDir::new();
    let output = call(
        &PwshTool::new(),
        &temp.ctx(),
        serde_json::json!({"command": "Start-Sleep -Seconds 10", "timeout_ms": 1000}),
    )
    .await;
    assert!(output.is_error);
    assert!(output.content.contains("command timed out after 1000ms"));
}

#[tokio::test]
async fn pwsh_either_runs_or_reports_a_spawn_failure() {
    let temp = TempDir::new();
    let output = call(
        &PwshTool::new(),
        &temp.ctx(),
        serde_json::json!({"command": "exit 0"}),
    )
    .await;
    if pwsh_available() {
        assert!(!output.is_error, "{}", output.content);
    } else {
        assert!(output.is_error);
        assert!(
            output.content.contains("failed to spawn pwsh")
                || output.content.contains("failed to spawn powershell.exe"),
            "{}",
            output.content
        );
    }
}

#[tokio::test]
async fn echo_pretty_prints_arguments() {
    let temp = TempDir::new();
    let output = call(
        &EchoTool::new(),
        &temp.ctx(),
        serde_json::json!({"b": 2, "a": [1, true]}),
    )
    .await;
    assert!(!output.is_error);
    assert!(output.content.contains("\"a\": ["));
    assert!(output.content.contains("\"b\": 2"));
}

#[tokio::test]
async fn missing_arguments_name_the_argument() {
    let temp = TempDir::new();
    let cx = temp.ctx();
    let cases: Vec<(Box<dyn Tool>, serde_json::Value, &str)> = vec![
        (Box::new(ReadTool::new()), serde_json::json!({}), "path"),
        (
            Box::new(WriteTool::new()),
            serde_json::json!({"path": "a.txt"}),
            "content",
        ),
        (
            Box::new(EditTool::new()),
            serde_json::json!({"path": "a.txt", "new_string": "y"}),
            "old_string",
        ),
        (
            Box::new(EditTool::new()),
            serde_json::json!({"path": "a.txt", "old_string": "x"}),
            "new_string",
        ),
        (Box::new(GlobTool::new()), serde_json::json!({}), "pattern"),
        (Box::new(GrepTool::new()), serde_json::json!({}), "pattern"),
        (Box::new(PwshTool::new()), serde_json::json!({}), "command"),
    ];
    for (tool, arguments, name) in cases {
        let output = call(tool.as_ref(), &cx, arguments).await;
        assert!(output.is_error, "{} did not error", tool.name());
        assert!(
            output
                .content
                .contains(&format!("missing required argument \"{name}\"")),
            "{} -> {}",
            tool.name(),
            output.content
        );
    }
}

#[tokio::test]
async fn non_object_arguments_are_reported_not_panicked() {
    let temp = TempDir::new();
    let output = call(
        &ReadTool::new(),
        &temp.ctx(),
        serde_json::json!("not an object"),
    )
    .await;
    assert!(output.is_error);
    assert!(
        output
            .content
            .contains("missing required argument \"path\"")
    );

    let echo = call(&EchoTool::new(), &temp.ctx(), serde_json::json!(null)).await;
    assert!(!echo.is_error);
    assert_eq!(echo.content, "null");
}
