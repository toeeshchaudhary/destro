# Conventions: instructions, skills, commands, MCP, sessions

Destro follows the conventions other agent tools set, so an existing repo works
with no setup. Everything below is loaded when a workspace opens.

## Instruction files

Every agent (planner and workers in advisor mode too) gets these in its system
prompt, user scope first, then project scope from the filesystem root down to the
workspace, so the nearest file reads as the most specific:

| Scope | Files |
|---|---|
| user | `~/.destro/AGENTS.md`, `~/.destro/CLAUDE.md`, `~/.claude/CLAUDE.md`, `~/.agents/AGENTS.md`, `~/.dsh/AGENTS.md` |
| project, in every directory up to the workspace | `AGENTS.md`, `CLAUDE.md`, `CLAUDE.local.md`, `.destro/AGENTS.md`, `.claude/CLAUDE.md` |

* `$DESTRO_HOME` replaces `~/.destro`, `$DSH_HOME` replaces `~/.dsh`.
* `@path/to/file.md` on a line inlines that file (four levels deep, cycles
  ignored), like `CLAUDE.md` imports.
* Identical content loads once (a `CLAUDE.md` symlinked to `AGENTS.md`).
* Each chain is capped at 64 KiB; the cut is marked in the prompt.
* `/context` shows exactly which files are in use.

**Nested files:** an `AGENTS.md` / `CLAUDE.md` in a sub-folder below the workspace is
delivered the first time a tool touches a path in that folder: it rides along
with that tool's result (once per session), so the agent sees `pkg/AGENTS.md`
when it first reads `pkg/lib.rs`.

## Skills

A skill is a folder with `SKILL.md`:

```markdown
---
name: release-notes
description: Draft release notes from merged PRs. Use when asked for a changelog.
---
Steps…
```

Only the name and description go in the system prompt; the model calls the
`skill` tool to load the body (progressive disclosure). Search paths (project
overrides user by name): `~/.destro/skills`, `~/.claude/skills`,
`~/.agents/skills`, `~/.dsh/skills`, then `.destro/skills`, `.claude/skills`,
`.agents/skills` from the filesystem root down to the workspace.

Browse them with `/skills`; typing `/<skill-name> task…` asks the model to use it.

## Custom commands

`name.md` in `.destro/commands` or `.claude/commands` (project) or
`~/.destro/commands`, `~/.claude/commands` (user). `/name args` sends the file's
body with `$ARGUMENTS` replaced (or the arguments appended).

## MCP servers

`.mcp.json` / `.destro/mcp.json` in the project (any directory up to the
workspace) and `~/.destro/mcp.json`. The format matches other tools:

```json
{ "mcpServers": {
  "files":  { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."] },
  "remote": { "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ${TOKEN}" } }
} }
```

`${VAR}` and `${VAR:-default}` expand in commands, args, URLs, headers and env.
Stdio, streamable-HTTP and the older HTTP+SSE (`"type": "sse"`) transports work.

* **Tools** appear as `mcp__<server>__<tool>` and refresh live when the server
  announces `notifications/tools/list_changed`.
* **Resources**: servers that offer them get `mcp__<server>__list_resources` and
  `__read_resource` tools.
* **Prompts** show up in the `/` popup as `/mcp:<server>:<prompt>`; arguments are
  `key=value` or positional.
* **Auth**: put a token in `headers`. OAuth login flows are not supported; a
  401/403 says so instead of dumping the response.

**Trust:** a project's `.mcp.json` can launch arbitrary commands, so each
project-scope server waits for approval once (`/mcp`, Enter). The decision is
remembered in `mcp-trust.json`. User-scope servers start on their own.
`/mcp` also enables, disables and reconnects servers while you work.

## Sessions

Every session is saved as it happens to
`%APPDATA%\destro\sessions\<workspace>\<id>.jsonl` (`~/.config/destro/…` elsewhere);
subagent sessions are filed beside their root as `<root>.sub.<child>.jsonl`.

* `/resume` (Alt+R) reopens one with its full conversation, its subagent tree and, for
  advisor sessions, advisor mode. In that list: Ctrl+R renames, Ctrl+F forks, Ctrl+D
  deletes (press twice).
* `/search <words>` searches the text of every saved session in every workspace and
  opens the hit (opening its workspace if needed).
* `/rename <title>`, `/fork` (continue a copy independently), `/export` (Markdown).
* Prompt history (Up/Down) persists too.

## Approvals, questions, compaction

* **Approvals** are a toggle: `/approvals` (remembered between runs). When on, the
  shell, file writes and edits, and MCP tools wait for **y** allow once, **a** always
  (this run) or **n** reject. Off by default.
* **`ask_user`**: the model can stop and ask you a question; you pick an option or
  type an answer, or Esc to skip.
* **Compaction**: when a request's context passes 100k tokens, older messages are folded
  into one summary and the last couple of turns are kept verbatim. `/compact` does it
  on demand. Resuming a compacted session replays the compaction.

## Web and planning tools

`web_search` works without a key (DuckDuckGo); set `BRAVE_API_KEY`, `TAVILY_API_KEY`
or `SEARXNG_URL` to use one of those instead. `web_fetch` reads a URL as text.
`todo_write` keeps a visible checklist for multi-step work.

## Importing DeepSeek Harness chats

```text
destro import-dsh --list      # what would be imported
destro import-dsh             # import every root chat
destro import-dsh --force     # re-import (overwrites earlier imports)
```

or `/import` inside the TUI to pick workspaces and chats (imports run on all cores).
dsh logs
(`~/.dsh/sessions/<workspace>/<id>/session.v4.jsonl.zstd`) are converted to
Destro sessions under the same workspace path, keeping prompts, assistant text
and reasoning, tool calls and results, titles and token usage. dsh bookkeeping
(runtime-context snapshots, skill catalogues, sandbox/approval records, jobs) is
dropped. Subagent sessions are imported as child sessions of the chat that spawned
them (nested ones keep their nesting), original timestamps are kept, and images
become `[image: <path>]` pointing at dsh's attachment store. Imported chats resume
on the workspace's current model.

## Models

`/model` (Alt+M) switches the current session and every new one in the workspace;
type any `provider/model` route. Routes for configured providers are listed.
`/provider` lists every provider; picking one that has no key asks for it and stores
it in `keys.json` in your Destro config (an environment variable still wins), and the
provider becomes usable immediately in running sessions.
