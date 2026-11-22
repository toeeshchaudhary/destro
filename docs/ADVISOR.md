# Advisor mode

Advisor mode splits one task across two model routes: a **planner/judge** that
owns the objective, and **workers** that execute independent subtasks. The
advisor never does bulk work itself; it decomposes, delegates, judges the
reports, and synthesises the final answer.

## Starting it

- In the TUI press `Alt+A`, or type `/advisor`.
- On first entry a picker appears with two fields:
  - **advisor model** — plans, judges, writes the final synthesis.
  - **slave models** — the route every spawned worker uses.
- `Tab` switches fields, type to replace a value, `Enter` starts, `Esc` returns
  to normal mode. `/normal` also leaves advisor mode.
- Flags skip the picker: `destro --advisor-model deepseek-v4-pro --slave-model
  deepseek-v4-flash` (with `--advisor` only when one field still needs a choice).

Nothing changes about how you submit a task: type it and press Enter. The
advisor session is an ordinary session in the transcript, so every delegation
is visible.

## What happens to a task

```
your task ──▶ advisor (plans, writes briefs)
                │  subagent(wait:false) × N      ← parallel delegation
                ▼
             workers (fresh context, slave model)
                │  report: result / changes / evidence / blockers
                ▼
             advisor (judges each report)
                │  accept · send_message correction · focused re-delegation
                ▼
             final synthesis: Outcome · Changes · Evidence · Open risks
```

The mechanics are ordinary tools, not a hidden scheduler:

| Step | Tool |
|---|---|
| Delegate an independent subtask | `subagent` with `wait:false` |
| Delegate one blocking step | `subagent` with `wait:true` |
| Collect results | `wait_agent` |
| Correct or refine a worker | `send_message` (queue) or force it with `mode:"inject"` |
| Inspect anything | `list_agents`, or the TUI inspector |

Because delegation is prompt-driven, advisor mode is provider-agnostic: it works
on any `Provider` implementation, and a mock provider can exercise the whole
flow in tests (`crates/destro-core/tests/advisor.rs`).

## Routing

`HarnessConfig` carries the split:

- `model` + `system` → the advisor route and prompt.
- `slave_model` + `slave_system` → what `Harness::spawn` uses for any agent with
  a parent, unless the spawn names a model explicitly.

This keeps one harness per workspace and one rule for children: **if an agent
has a parent, it is a worker**. An explicit `model` argument on `subagent` still
wins, which is how the advisor can escalate one hard subtask to a stronger
route.

## The UI

Advisor mode changes the frame, not just the header:

- A reversed `ADVISOR` badge with `planner ▸ workers` in the header.
- A board at the top of the right panel: both routes and `reported/workers`
  progress.
- The agent list below it is the task list: one row per worker, with its status
  glyph, turn count, and live tool.

Monochrome is deliberate — the TUI has to read the same over SSH and in any
terminal theme.

## Rules the prompts enforce

- Independent subtasks fan out; work that writes the same files stays ordered.
- Every worker gets a self-contained brief; it cannot see the advisor's chat.
- A report is accepted only with changes, evidence, and blockers.
- Failed commands stay in the report so the advisor can act on them.
- The final answer follows the four-section format above, with paths.
