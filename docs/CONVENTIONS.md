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

Not done yet: nested `AGENTS.md` files in sub-directories the agent reads into
later (they are only loaded if they sit on the path to the workspace).

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
Stdio and streamable-HTTP transports work; tools appear as `mcp__<server>__<tool>`.
Resources and prompts are not used yet.

**Trust:** a project's `.mcp.json` can launch arbitrary commands, so each
project-scope server waits for approval once (`/mcp`, Enter). The decision is
remembered in `mcp-trust.json`. User-scope servers start on their own.
`/mcp` also enables, disables and reconnects servers while you work.

## Sessions

Every root session is saved as it happens to
`%APPDATA%\destro\sessions\<workspace>\<id>.jsonl` (`~/.config/destro/…` elsewhere)
and `/resume` (Alt+R) reopens one with its full conversation. `/export` writes the
current session to Markdown. Prompt history (Up/Down) persists too.

## Importing DeepSeek Harness chats

```text
destro import-dsh --list      # what would be imported
destro import-dsh             # import every root chat
destro import-dsh --force     # re-import (overwrites earlier imports)
```

or `/import` inside the TUI to pick workspaces and chats. dsh logs
(`~/.dsh/sessions/<workspace>/<id>/session.v4.jsonl.zstd`) are converted to
Destro sessions under the same workspace path, keeping prompts, assistant text
and reasoning, tool calls and results, titles and token usage. dsh bookkeeping
(runtime-context snapshots, skill catalogues, sandbox/approval records, jobs) is
dropped, and subagent sessions are folded into their parent's tool calls. Images
become `[image]`. Imported chats can be resumed and continued on any model.

## Models

`/model` (Alt+M) switches the current session and every new one in the workspace;
type any `provider/model` route. Routes for configured providers are listed.
