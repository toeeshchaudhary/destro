mod app;
mod backend;
mod balance;
mod config;
mod demo;
mod highlight;
mod local;
mod md;
mod model;
mod theme;
mod ui;
mod updater;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use serde_json::json;

use app::{App, SnipState};
use backend::{Backend, Msg, ProjectSpec, Real};
use balance::Balance;
use local::Local;
use model::Project;

const USAGE: &str = "destro — Rust TUI for the Destro harness

USAGE
  destro                                run the built-in Rust harness in-process
  destro --demo                         scripted multi-project demo, no harness
  destro --dsh \"node path/to/dsh.js\" [--project <dir> ...]
                                        drive real dsh runtimes instead (one per workspace)
                                        (no --project: reopen saved workspaces, else cwd)
IN THE TUI
  /open <path>      add a workspace (Alt+O prefills, Tab completes directories)
  /close            close the current workspace
  Ctrl+S            snippet viewer     F1  all keys
OPTIONS
  --provider <id>   provider route (default: first configured provider, else mock)
  --model <id>      model (default: the provider's default)
  update            self-update from the latest GitHub release
  providers         list providers and exit
  --snapshot [done|snippets|open]
                    render a demo frame to stdout and exit
  --bench <N>       ingest N synthetic events, time rendering, exit
  --probe           run the in-process harness once, print the transcript, exit
  --prompt <text>   prompt used by --probe
  --advisor         start in advisor mode (prompts for planner + worker models)
  --advisor-model <id>  advisor model, skips its picker field
  --slave-model <id>    worker model, skips its picker field
  --update          same as `update`
  --providers       same as `providers`
  --theme <name>    colour theme (else the saved choice; Ctrl+T in the TUI)
  --version         print the version and exit
ENV
  DEEPSEEK_API_KEY  enables the balance readout (env or .env)
";

struct Args {
    demo: bool,
    dsh: Vec<String>,
    projects: Vec<String>,
    provider: String,
    model: String,
    snapshot: Option<String>,
    bench: Option<usize>,
    hold: bool,
    probe: bool,
    advisor: bool,
    advisor_model: Option<String>,
    slave_model: Option<String>,
    update: bool,
    providers: bool,
    theme: Option<String>,
    prompt: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        demo: false,
        dsh: vec![],
        projects: vec![],
        provider: "deepseek-official".into(),
        model: "deepseek-v4-pro".into(),
        snapshot: None,
        bench: None,
        hold: false,
        probe: false,
        advisor: false,
        advisor_model: None,
        slave_model: None,
        update: false,
        providers: false,
        theme: None,
        prompt: None,
    };
    let mut it = std::env::args().skip(1).peekable();
    while let Some(x) = it.next() {
        match x.as_str() {
            "--demo" => a.demo = true,
            "--dsh" => {
                a.dsh = it
                    .next()
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(String::from)
                    .collect()
            }
            "--project" => a.projects.push(it.next().unwrap_or_default()),
            "--provider" => a.provider = it.next().unwrap_or_default(),
            "--model" => a.model = it.next().unwrap_or_default(),
            "--theme" => a.theme = it.next(),
            "--snapshot" => {
                let mode = match it.peek().map(String::as_str) {
                    Some(
                        "done" | "snippets" | "open" | "palette" | "themes" | "help" | "slash"
                        | "fresh",
                    ) => it.next().unwrap(),
                    _ => String::new(),
                };
                a.snapshot = Some(mode);
            }
            "--bench" => a.bench = it.next().and_then(|n| n.parse().ok()),
            "--hold" => a.hold = true,
            "--probe" => a.probe = true,
            "--advisor" => a.advisor = true,
            "--advisor-model" => a.advisor_model = it.next(),
            "--slave-model" => a.slave_model = it.next(),
            "--update" | "update" => a.update = true,
            "--providers" | "providers" => a.providers = true,
            "--prompt" => a.prompt = it.next(),
            "--version" | "-V" => {
                println!("destro {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    if a.dsh.is_empty()
        && let Ok(v) = std::env::var("DESTRO_DSH")
    {
        a.dsh = v.split_whitespace().map(String::from).collect();
    }
    Ok(a)
}

fn demo_projects() -> Vec<Project> {
    vec![
        Project::new("kuro-site", "~/Documents/kuro-site", "deepseek-v4-pro"),
        Project::new(
            "voidnix-naf",
            "~/Documents/reverse-mortgage",
            "deepseek-v4-pro",
        ),
        Project::new("roar-mesh-fw", "~/Documents/roar-mesh", "deepseek-v4-flash"),
    ]
}

fn render_to_string(app: &mut App, w: u16, h: u16) -> Result<String> {
    let mut term = Terminal::new(TestBackend::new(w, h))?;
    term.draw(|f| ui::draw(f, app))?;
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut row = String::new();
        for x in 0..w {
            row.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }
    Ok(out)
}

struct NullBackend(#[allow(dead_code)] mpsc::Sender<Msg>);
impl Backend for NullBackend {
    fn send(&self, _: usize, _: serde_json::Value) {}
    fn kill(&self, _: usize) {}
    fn open(&self, _: usize, _: &ProjectSpec) {}
}

/// Drive the demo script synchronously through the same `on_frame` parser the live path uses.
fn snapshot(mode: &str) -> Result<()> {
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(demo_projects(), Box::new(NullBackend(tx)));
    app.on_msg(Msg::Balance(balance::parse(
        r#"{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"42.17","granted_balance":"10.00","topped_up_balance":"32.17"}]}"#,
    )));
    let mut scr = demo::Gen::default();
    let mut next_approval = 100;
    let finished = !mode.is_empty();
    for p in 0..3 {
        app.on_frame(p, &json!({"jsonrpc":"2.0","id":0,"result":{}}).to_string());
        let mut steps: Vec<demo::Step> = demo::boot_steps(&mut scr, p, &mut next_approval)
            .into_iter()
            .flatten()
            .collect();
        steps.sort_by_key(|s| s.t);
        for s in steps.into_iter().filter(|s| finished || s.gate.is_none()) {
            app.on_frame(p, &s.frame);
        }
    }
    if finished {
        app.projects[0].approvals.clear();
    }
    match mode {
        "snippets" => {
            // the last diff the agents produced sits near the top of the list
            app.snip = Some(SnipState { sel: 1, v: 0, h: 0 });
        }
        "open" => {
            app.open_workspace("~/Documents/destro-tui")
                .map_err(anyhow::Error::msg)?;
        }
        "palette" => {
            app.palette = Some(app::Palette {
                query: String::new(),
                sel: 1,
            })
        }
        "themes" => {
            app.themes = Some(app::ThemePick {
                sel: 2,
                original: theme::index(),
            })
        }
        "help" => app.help = true,
        "slash" => app.input = "/th".into(),
        "fresh" => app.cur_session = None,
        _ => {}
    }
    app.tick = 3;
    println!("{}", render_to_string(&mut app, 150, 42)?);
    Ok(())
}

/// Ingest N synthetic events into one session, then time full-frame renders.
fn bench(n: usize, hold: bool) -> Result<()> {
    let (tx, _rx) = mpsc::channel();
    let mut app = App::new(
        vec![Project::new("bench", "/tmp", "m")],
        Box::new(NullBackend(tx)),
    );
    let mut scr = demo::Gen::default();
    let t0 = Instant::now();
    let mut frames = 0usize;
    let mut turn = 0u32;
    while frames < n {
        turn += 1;
        let mut batch = vec![scr.user(
            "b",
            turn,
            "please continue with the next item in the queue and keep the diff small",
            "user",
        )];
        let (id, call) = scr.call(
            "b",
            turn,
            "read_file",
            json!({"path": format!("src/mod_{turn}.rs")}),
        );
        batch.push(call);
        batch.push(scr.result(
            "b",
            turn,
            &id,
            "fn main() {}\n// body of the file…\nlet x = 1;",
            false,
        ));
        batch.push(scr.assistant("b", turn, None, Some("Done; normalised parsing and added a regression test:\n```rust\nfn parse(s: &str) -> u32 { s.parse().unwrap_or(0) }\n```"), (50_000, 80, 40_000)));
        for f in batch {
            app.on_frame(0, &f);
            frames += 1;
        }
    }
    let ingest = t0.elapsed();
    app.cur_session = Some(0);
    let s = &app.projects[0].sessions[0];
    println!(
        "events ingested : {frames}  in {ingest:.2?}  ({:.0} ev/s)",
        frames as f64 / ingest.as_secs_f64()
    );
    println!(
        "items retained  : {}  (evicted {})",
        s.items.len(),
        s.dropped
    );
    println!("snippets kept   : {}", s.snippets.len());
    let mut term = Terminal::new(TestBackend::new(160, 50))?;
    term.draw(|f| ui::draw(f, &mut app))?; // builds the line cache
    let lines = app.projects[0].sessions[0].cache.lines.len();
    let t1 = Instant::now();
    for _ in 0..300 {
        term.draw(|f| ui::draw(f, &mut app))?;
    }
    println!("cached lines    : {lines}");
    println!(
        "steady frame    : {:.3?} avg over 300 draws",
        t1.elapsed() / 300
    );
    if hold {
        println!("holding 4s for external memory measurement...");
        std::thread::sleep(Duration::from_secs(4));
    }
    Ok(())
}

/// Build a route set from every configured provider. `preferred` names the
/// default; unconfigured providers are skipped, and mock is the last resort.
fn provider_set(
    registry: &destro_core::ProviderRegistry,
    preferred: &str,
) -> Result<std::sync::Arc<destro_core::ProviderSet>> {
    use std::collections::HashMap;
    use std::sync::Arc;
    let default = match preferred {
        "" | "deepseek-official" => registry.default_id(),
        id => id.to_string(),
    };
    let mut map: HashMap<String, Arc<dyn destro_core::Provider>> = HashMap::new();
    for info in registry.list() {
        if !info.configured {
            continue;
        }
        if let Ok(provider) = registry.build(&info.id) {
            map.insert(info.id, provider);
        }
    }
    if map.is_empty() {
        map.insert(
            "mock".into(),
            Arc::new(destro_core::MockProvider::echo()) as Arc<dyn destro_core::Provider>,
        );
        return Ok(destro_core::ProviderSet::new("mock", map));
    }
    if !map.contains_key(&default) {
        let fallback = map.keys().next().cloned().unwrap_or_else(|| "mock".into());
        return Ok(destro_core::ProviderSet::new(fallback, map));
    }
    Ok(destro_core::ProviderSet::new(default, map))
}

/// End-to-end check of the in-process path without a terminal: drive the local
/// backend exactly as the TUI would and assert the transcript arrives.
fn probe(args: &Args) -> Result<()> {
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let specs = vec![ProjectSpec {
        name: "probe".into(),
        cwd: cwd.clone(),
    }];
    let registry = destro_core::ProviderRegistry::load();
    let providers = provider_set(&registry, &args.provider)?;
    let (tx, rx) = mpsc::channel();
    let (projects, local) = Local::start(&specs, &args.model, providers, tx);
    let mut app = App::new(projects, Box::new(local));
    if let (Some(a), Some(b)) = (&args.advisor_model, &args.slave_model) {
        app.arm_advisor(a, b);
    } else if args.advisor {
        app.enter_advisor();
    }

    app.backend.send(
        0,
        json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"cwd": cwd}}),
    );
    let prompt = args
        .prompt
        .clone()
        .unwrap_or_else(|| "probe run please".into());
    app.backend.send(
        0,
        json!({"jsonrpc":"2.0","id":1,"method":"session/query","params":{
            "sessionId": "probe-session",
            "contentBlocks": [{"type": "text", "text": prompt}]
        }}),
    );

    let window = if app.advisor.is_some() { 75 } else { 20 };
    let deadline = Instant::now() + Duration::from_secs(window);
    let mut replied = false;
    // In advisor mode keep listening after the first reply so delegations land too.
    while Instant::now() < deadline && (!replied || app.advisor.is_some()) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(msg) => app.on_msg(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        replied = app.projects[0].sessions.iter().any(|s| {
            s.items.iter().any(|item| match item {
                model::Item::Assistant(text) => !text.trim().is_empty(),
                _ => false,
            })
        });
    }

    for session in &app.projects[0].sessions {
        for item in &session.items {
            println!("{item:?}");
        }
    }
    println!("connection: {:?}", app.projects[0].conn);
    if let Some((advisor, slaves)) = app.advisor_models() {
        println!("advisor: {advisor}  slaves: {slaves}");
    }
    if !replied {
        anyhow::bail!("in-process harness produced no assistant transcript within 20s");
    }
    Ok(())
}

fn run_update() -> Result<()> {
    match updater::update("destro", updater::target_triple()) {
        Ok(updater::UpdateOutcome::UpToDate { current, latest }) => {
            println!("destro {current} is current (latest {latest})");
        }
        Ok(updater::UpdateOutcome::Updated { from, to, path }) => {
            println!("updated {from} -> {to} ({})", path.display());
        }
        Err(e) => {
            println!("update failed: {e}");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn list_providers() -> Result<()> {
    let registry = destro_core::ProviderRegistry::load();
    println!("provider               state      default model");
    for info in registry.list() {
        println!(
            "{:<22} {:<10} {}{}",
            info.id,
            if info.configured {
                "configured"
            } else {
                "needs key"
            },
            info.default_model,
            if info.models.is_empty() {
                String::new()
            } else {
                format!("  [{}]", info.models.join(", "))
            }
        );
    }
    if let Some(path) = destro_core::ProviderRegistry::path() {
        println!("\nadd your own in {}", path.display());
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = parse_args()?;
    match &args.theme {
        Some(name) => {
            if !theme::set_by_name(name) {
                anyhow::bail!("unknown theme {name:?}; available: {}", theme::names());
            }
        }
        None => {
            if let Some(saved) = config::load_theme() {
                theme::set_by_name(&saved);
            }
        }
    }
    if let Some(mode) = &args.snapshot {
        return snapshot(mode);
    }
    if let Some(n) = args.bench {
        return bench(n, args.hold);
    }
    if args.update {
        return run_update();
    }
    if args.providers {
        return list_providers();
    }
    if args.probe {
        return probe(&args);
    }

    let (tx, rx) = mpsc::channel::<Msg>();
    let demo_mode = args.demo;
    let local_mode = !demo_mode && args.dsh.is_empty();
    let mut suggestions: Vec<String> = Vec::new();
    let mut provider_label = String::new();
    let (projects, backend, balance): (Vec<Project>, Box<dyn Backend>, Balance) = if demo_mode {
        let ps = demo_projects();
        let n = ps.len();
        balance::spawn_demo(tx.clone());
        (ps, Box::new(demo::Demo::start(tx, n)), Balance::Loading)
    } else {
        let mut dirs = args.projects.clone();
        if dirs.is_empty() {
            dirs = config::load()
                .into_iter()
                .filter(|d| std::path::Path::new(d).is_dir())
                .collect();
        }
        if dirs.is_empty() {
            dirs.push(std::env::current_dir()?.to_string_lossy().to_string());
        }
        let specs: Vec<ProjectSpec> = dirs
            .iter()
            .map(|p| {
                let abs = std::fs::canonicalize(p)
                    .map(|x| x.to_string_lossy().trim_start_matches(r"\\?\").to_string())
                    .unwrap_or(p.clone());
                let name = std::path::Path::new(&abs)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or(abs.clone());
                ProjectSpec { name, cwd: abs }
            })
            .collect();
        let bal = if args.provider.contains("deepseek") {
            let cwds: Vec<String> = specs.iter().map(|s| s.cwd.clone()).collect();
            match balance::find_key(&cwds) {
                Some(key) => {
                    balance::spawn_poller(key, tx.clone());
                    Balance::Loading
                }
                None => Balance::NoKey,
            }
        } else {
            Balance::Unsupported
        };
        if local_mode {
            let registry = destro_core::ProviderRegistry::load();
            let providers = provider_set(&registry, &args.provider)?;
            provider_label = args.provider.clone();
            for info in registry.list() {
                if !info.configured || info.id == "mock" {
                    continue;
                }
                for model in &info.models {
                    suggestions.push(format!("{}/{}", info.id, model));
                }
            }
            let (ps, local) = Local::start(&specs, &args.model, providers, tx.clone());
            (
                ps,
                Box::new(local) as Box<dyn Backend>,
                Balance::Unsupported,
            )
        } else {
            let ps = specs
                .iter()
                .map(|s| Project::new(&s.name, &s.cwd, &args.model))
                .collect();
            (
                ps,
                Box::new(Real::start(
                    &specs,
                    &args.dsh,
                    &args.provider,
                    &args.model,
                    tx,
                )),
                bal,
            )
        }
    };

    let mut app = App::new(projects, backend);
    app.balance = balance;
    app.default_model = args.model.clone();
    app.strict_paths = !demo_mode;
    app.persist = !demo_mode;
    if !suggestions.is_empty() {
        app.model_suggestions = suggestions;
    }
    if local_mode {
        app.flash(format!("rust harness · provider {provider_label}"));
    }
    match (&args.advisor_model, &args.slave_model) {
        (Some(advisor), Some(slaves)) => app.arm_advisor(advisor, slaves),
        _ if args.advisor => app.enter_advisor(),
        _ => {}
    }
    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app, rx);
    ratatui::restore();
    res
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: mpsc::Receiver<Msg>,
) -> Result<()> {
    let mut dirty = true;
    let mut last_tick = Instant::now();
    while !app.quit {
        while let Ok(m) = rx.try_recv() {
            app.on_msg(m);
            dirty = true;
        }
        if last_tick.elapsed() >= Duration::from_millis(100) {
            last_tick = Instant::now();
            if app.housekeeping() {
                dirty = true;
            }
            if app.any_running() {
                app.tick += 1;
                dirty = true;
            }
        }
        if dirty {
            terminal.draw(|f| ui::draw(f, app))?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => app.on_key(k),
                Event::Resize(..) => {}
                _ => continue,
            }
            dirty = true;
        }
    }
    Ok(())
}
