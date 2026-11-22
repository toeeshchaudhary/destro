# Destro harness (phase 0)

Destro is a coding and general-purpose agent harness. The TUI is one client of
it; the harness itself now runs as Rust, in-process, with no Node runtime on the
path. This document records what exists today, how the pieces fit, and what the
next phases must add to reach feature parity with dsh.

## Status

| Piece | State |
|---|---|
| Agent loop (model → tools → observe) | working |
| OpenAI-compatible SSE provider + provider registry | working (needs an API key) |
| Session event log + wire envelopes | working, dsh-compatible names |
| Steering control plane (queue / inject / interrupt / kill / model swap) | working, tested |
| Nested agents and tree kill | working, tested |
| Advisor mode (planner + worker routes) | working, tested |
| Subagent + control tools (`subagent`, `list_agents`, `send_message`, `interrupt_agent`, `wait_agent`) | working |
| Filesystem, search, shell tools | working (`read`, `write`, `edit`, `glob`, `grep`, `pwsh`) |
| Self-updater + release workflow | working |
| Context accounting | usage only |
| Instruction files (`AGENTS.md`/`CLAUDE.md`), skills, custom commands, MCP client | working — see [CONVENTIONS.md](CONVENTIONS.md) |
| Session persistence, `/resume`, dsh chat import, model switching | working |
| Compaction, sandbox, approvals | not yet — phase 3 |

Provider vendors are configuration, not code: see
[PROVIDERS.md](PROVIDERS.md). Advisor mode is described in
[ADVISOR.md](ADVISOR.md).

Everything below is verified by `cargo test --workspace`, the protocol smoke
script, and the TUI probe. Nothing requires an API key yet.

## Crates

```
Cargo.toml                 workspace root
crates/destro-proto/       JSON-RPC frames, method names, session event shapes
crates/destro-core/        agent actor, provider trait, gate, harness registry, control tools
crates/destro-tools/       model-facing tools over the filesystem and shell
crates/destro-runtime/     `destro-harness` binary: dsh SDK protocol over stdio
destro-tui/                ratatui client; `Local` backend runs the harness in-process
```

Directions of dependency are one-way: `proto → tools → core → runtime|tui`.

## How a turn runs

```
operator/TUI ──JSON-RPC──► runtime (or Local backend)
                              │  session/query, agent/send
                              ▼
                        AgentHandle ──mailbox──► AgentTask
                                                   │
                          ┌────────────────────────┼─────────────────────────┐
                          ▼                        ▼                         ▼
                    Gate (priority)          Provider::stream          Tool registry
                    caps concurrent          text/tool deltas          read/write/edit/
                    model requests           usage, done               glob/grep/pwsh
                          │                        │                         │
                          └──────────────► session events + status ───────────┘
                                                   │
                                             EventBus (bounded)
                                                   │
                                    notifications to every observer
```

One agent is one Tokio task. Its mailbox carries **every** operator action, so
steering never mutates agent state from another thread. The observable state
(`Live`) is a handful of atomics plus two small mutexes, so listing a thousand
agents is cheap and lock-free on the hot path.

### Message semantics

| Command | Effect | Used by |
|---|---|---|
| `Queue` | Delivered at the next turn boundary; never preempts | normal prompts, `send_message` default |
| `Inject` | Cancels the in-flight request, commits the partial assistant message, starts a fresh turn with the message | the steering verb |
| `Interrupt` | Cancels the in-flight request; agent stays alive and addressable | TUI, `interrupt_agent` |
| `Kill` | Cancels the agent and, through token parentage, every descendant | TUI, `agent/kill` |
| `SetModel` | Re-routes the next request | TUI, future inspector action |
| `State` | One-shot inspector snapshot | `agent/state`, `wait_agent` |

Agent ids are `agent-000123`; each owns `session-000123`. Clients may address an
agent by either form. A client session id (for example `tui-…`) is bound to an
agent on first use and echoed back on every notification.

### Priority and backpressure

`Gate` is a counting gate with a reserved interactive lane: background subagent
requests can never take the last `interactive_reserve` slots, so a wall of
children cannot starve the prompt a human just typed. Mailboxes are bounded at
64 commands; the event bus is bounded and lossy-by-lag (subscribers re-sync from
`agent/list` rather than slowing agents down).

## Running it

```powershell
# protocol server on stdio (initialize, session/query, agent/* …)
cargo run -p destro-runtime -- --cwd . --model mock-1

# interactive protocol smoke test, including a spawned child
pwsh -File crates/destro-runtime/smoke.ps1

# TUI driving the harness in-process (no dsh child)
cargo run -p destro-tui

# headless proof that the in-process path produces a transcript
cargo run -p destro-tui -- --probe

# scripted demo, and the render benchmark
cargo run -p destro-tui -- --demo
cargo run -p destro-tui -- --bench 2000

# the old dsh path is still available
cargo run -p destro-tui -- --dsh "node path/to/dsh.js" --project .
```

## Evidence

Measured on this machine. Debug builds for the first four rows, release for the
last two:

| Claim | Measurement |
|---|---|
| 1,000 agents on one runtime (debug) | spawn 24.4 ms; every first turn complete in 151.5 ms (~6.6k agents/s) |
| 1,000 agents on one runtime (release) | spawn 3.3 ms; every first turn complete in 20.3 ms (~49k agents/s) |
| Deep tree inspectable | 50-deep chain lists and steers by id; tree kill reaches all 51 |
| Steering preempts | injected message cancels a streaming turn; transcript keeps the interrupted turn and answers the new one |
| Protocol surface | initialize, query, wait, spawn, list, state all answered; clean exit code 0 |
| TUI render (release) | 256 µs average frame over 70,000 cached lines; 76k events/s ingest |
| TUI render (debug) | 2.98 ms average frame over 7,000 cached lines; 11.8k events/s ingest |

The numbers above are shape evidence, not a benchmark claim against dsh; the
cross-check is part of the next measurement step.

## Roadmap

**Phase 1 — a real model.** Done: `OpenAiCompatProvider` streams SSE, accumulates
tool calls, maps usage, honours cancellation, and is reachable through the
provider registry (`--provider deepseek|openai-compatible|…`). Approval and
`user-question` round-trips land next, because tools must be able to ask.

**Phase 2 — the operator experience.** Agent inspector pane in the TUI (filter by
running/failed, per-agent transcript, token/context readout), steer prompt
targeted at the selected agent (queue vs inject on a modifier), broadcast, and a
1k-agent benchmark in the UI loop. Advisor mode currently fans out through
tools; the inspector is how a human supervises it at scale.

**Phase 3 — dsh parity.** Context assembly and compaction, skills with
progressive disclosure, MCP client, sandbox/permission policy, durable session
log with resume and search, `apply_patch` and `read_image`, jobs, todos, goals.
The feature list to close against is `upstream-deepseek-harness/docs/tool-catalog.md`
plus the research brief in `_in/harness_paper.txt`.

## Known limits

- Live model calls need a key (`DEEPSEEK_API_KEY` / `OPENAI_API_KEY`, or your own
  provider entry). Without one, the default route is the mock provider.
- Transcripts and the agent registry are not pruned. Fine for thousands of
  agents, not for an all-day daemon; a window/cap policy belongs with the
  inspector work.
- `messages` is cloned per request. Correct, but O(history) per call; switch to
  `Arc` sharing plus an append-only conversation before long sessions.
- Events are message-granular; the TUI does not yet render streaming deltas.
- Killing an agent mid-tool drops the tool future; `pwsh` sets `kill_on_drop`, but
  a future tool must opt in or handle cancellation itself.
- `dsh`'s approval and `user-question` round-trips are not implemented; the
  request shapes exist in `destro-proto`.
- Advisor mode's quality depends on the planner model; it is prompt-driven, so a
  weak advisor produces weak briefs. There is no automatic retry when a worker
  ignores its brief.
