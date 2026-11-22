# Destro

A coding and general-purpose agent harness written in Rust, with a ratatui TUI.

- `destro` — the TUI driving the Rust harness in-process.
- `destro --advisor` — advisor mode: one planner/judge delegates to worker agents.
- `destro --providers` — list model providers; add your own in a JSON file.
- `destro update` — self-update from the latest GitHub release.
- `destro-harness --cwd .` — the harness as a JSON-RPC server on stdio, speaking
  the dsh SDK protocol plus `agent/*` control methods.
- `destro import-dsh` — import your DeepSeek Harness chats (also `/import` in the TUI).
- `destro --demo` — scripted demo with no harness.
- `destro --probe` — headless check of the in-process path.

`AGENTS.md` / `CLAUDE.md`, skills, custom commands, MCP servers, `/resume`,
`/model` and dsh migration: [CONVENTIONS.md](docs/CONVENTIONS.md).

Install, update, and provider docs: [INSTALL.md](docs/INSTALL.md),
[UPDATES.md](docs/UPDATES.md), [PROVIDERS.md](docs/PROVIDERS.md).

How it works, the control plane, measurements, and the phase plan:
[HARNESS.md](docs/HARNESS.md). Advisor mode: [ADVISOR.md](docs/ADVISOR.md).

```powershell
cargo test --workspace
```
