# Prompt Comments: User Guide

This guide covers how to enable, configure, and use the prompt comments feature on any coding agent deployed on the Nasiko platform.

## What This Does

Coding agents accumulate instructions over time ("always run tests", "use tabs", etc.). Without maintenance, these files grow unbounded and eventually degrade agent performance. Prompt comments solve this by:

1. Recording **why** each instruction was added (trigger + hypothesis)
2. Stripping that metadata before the agent sees the prompt (zero noise)
3. Automatically pruning stale instructions when the list gets too long (one cheap LLM call)
4. Excluding revoked instructions from the prompt entirely

The result: your instruction file stays lean and every rule in it has documented justification.

## Enabling the Feature

The feature is **off by default**. You must explicitly opt in.

### Option 1: From the Nasiko UI (recommended)

1. Open the Nasiko dashboard
2. Navigate to **Agents** and select your coding agent
3. Go to the **Settings** tab
4. Toggle **Prompt Comments** on
5. Click **Restart** on the Overview tab (or run `nasiko restart <agent-name>`)

The setting persists across restarts. To disable later, uncheck the toggle and restart.

### Option 2: From the CLI

```sh
nasiko secrets set NASIKO_PROMPT_COMMENTS enabled --agent <agent-name>
nasiko restart <agent-name>
```

To disable:
```sh
nasiko secrets set NASIKO_PROMPT_COMMENTS disabled --agent <agent-name>
nasiko restart <agent-name>
```

### Option 3: From the workspace instruction file

Add this line to your `NASIKO.md` (or `CLAUDE.md`) in the workspace root:

```markdown
<!-- @prompt-comments enabled -->
```

This takes effect on the next agent session. No restart needed for agents that read instructions at session start (like the Rust coding agent). For containerized agents like opencode, a restart is required.

To disable at the workspace level (overrides a platform-level enable):
```markdown
<!-- @prompt-comments disabled -->
```

### Priority order

File directive > Platform setting (UI/CLI) > Default (disabled)

A workspace-level `disabled` will override a platform-level `enabled`, and vice versa. This lets admins enable it fleet-wide while individual workspaces opt out.

## Configuring Behavior

Once the feature is enabled, two additional directives control how it operates.

### Instruction addition mode

Controls whether the agent adds instructions on its own or only when you ask.

| Directive | Behavior |
|-----------|----------|
| `<!-- @instructions manual -->` | Agent only adds instructions when you say "remember this" or "add a rule" |
| `<!-- @instructions auto -->` | Agent adds instructions at its own judgment after fixing bugs or discovering conventions |
| No directive | Defaults to `manual` |

### Pruning mode

Controls automatic cleanup of stale instructions.

| Directive | Behavior |
|-----------|----------|
| `<!-- @pruning manual -->` | No automatic pruning (you can still say "prune my instructions") |
| `<!-- @pruning auto -->` | Auto-prune when instruction count exceeds 20 |
| `<!-- @pruning 15 -->` | Auto-prune at a custom threshold |
| No directive | Defaults to `manual` |

### Full example

```markdown
<!-- @prompt-comments enabled -->
<!-- @instructions auto -->
<!-- @pruning 15 -->

# My Project Rules

<!-- @prompt-comment
  added: 2026-08-10
  trigger: CI kept failing because tests weren't run locally first
  hypothesis: requiring local test pass before commits prevents CI failures
  outcome: confirmed
-->
- Always run the test suite before committing changes.

<!-- @prompt-comment
  added: 2026-08-12
  trigger: user requested this convention
  hypothesis: consistent formatting reduces review friction
  outcome: pending
-->
- Use 2-space indentation in all TypeScript files.
```

## Using the Feature

### The agent adds instructions automatically (auto mode)

When `@instructions auto` is set, the agent will call `update_instructions` on its own when it:
- Fixes a recurring bug and identifies a preventive rule
- Discovers a project convention from the codebase
- Observes a pattern that should persist across sessions

Each instruction is recorded with a trigger (what went wrong) and hypothesis (why the rule helps).

### You tell the agent to remember something (manual mode)

When `@instructions manual` is set (the default), the agent only adds instructions when you explicitly ask:

- "Remember to always run lint before committing"
- "Add a rule: never use `any` types in this project"
- "Record that we use snake_case for file names"

### Pruning instructions

**Automatic pruning** (when enabled): At session start, if the instruction count exceeds the threshold, the agent makes one LLM call to review each instruction's rationale and revokes stale ones. Revoked instructions are excluded from future sessions.

**Manual pruning** (always available): Just tell the agent:
- "Prune my instructions"
- "Clean up stale rules"
- "Review the instruction file for outdated entries"

The agent will review all instructions and revoke any that are no longer relevant.

### Confirming or revoking instructions manually

You can directly edit the instruction file to change an instruction's outcome:

- Change `outcome: pending` to `outcome: confirmed` to mark it as validated
- Change `outcome: pending` or `outcome: confirmed` to `outcome: revoked` to remove it from the prompt

Revoked instructions stay in the file (for audit trail) but are never shown to the agent.

## How It Works Internally

```
Session start:
  1. Agent reads NASIKO.md (or CLAUDE.md) from the workspace
  2. Parses prompt-comment blocks
  3. Drops revoked instructions
  4. Strips all comment metadata
  5. If auto-pruning enabled and threshold exceeded: one LLM call to identify stale rules
  6. Injects clean instruction text into the system prompt
  7. Normal agent session begins

During session:
  - Agent sees only the clean instructions (no metadata)
  - If auto mode: agent may add new instructions with rationale
  - If manual mode: agent only adds when explicitly asked
  - User can request manual pruning at any time
```

### Cost

| Action | Extra LLM calls |
|--------|----------------|
| Reading and stripping instructions | 0 (pure text processing) |
| Agent adding an instruction | 0 (tool-call within existing turn) |
| Auto-pruning (when triggered) | 1 (small prompt, short response) |
| Manual pruning (when user asks) | 1 (same as above) |
| Normal session with feature disabled | 0 |

## Supported Agents

| Agent | How it works |
|-------|-------------|
| Nasiko Coding Agent (Rust) | Built-in: reads instructions at session start, has `update_instructions` and `prune_instructions` tools |
| opencode agent | Pre-start hook: `instructions-init.js` reads instructions and rewrites the system prompt before opencode starts. Opencode uses its built-in file tools to write instructions guided by system prompt. |
| Any future agent | Implement the same pattern: read instruction file at start, strip comments, inject into prompt |

## Troubleshooting

**Feature is enabled but agent doesn't seem to use instructions:**
- Check that the instruction file exists in the workspace root (NASIKO.md, .nasiko/instructions.md, CLAUDE.md, or .claude/instructions.md)
- For opencode: ensure the agent was restarted after enabling
- Check agent logs for `[instructions-init]` messages

**Agent adds too many instructions:**
- Switch from `auto` to `manual` mode: `<!-- @instructions manual -->`
- Or lower the pruning threshold: `<!-- @pruning 10 -->`

**Pruning removes instructions you want to keep:**
- Mark them as confirmed: change `outcome: pending` to `outcome: confirmed` in the file
- Confirmed instructions are still reviewed during pruning but the LLM is less likely to revoke them

**Want to disable for one workspace but keep it on platform-wide:**
- Add `<!-- @prompt-comments disabled -->` to that workspace's instruction file

## File Format Reference

### Directives (single-line, at top of file)

```
<!-- @prompt-comments enabled|disabled -->
<!-- @instructions auto|manual -->
<!-- @pruning auto|manual|<number> -->
```

### Instruction annotation (multi-line block)

```
<!-- @prompt-comment
  added: YYYY-MM-DD
  trigger: <what failure or observation led to this>
  hypothesis: <why this instruction should help>
  outcome: pending|confirmed|revoked
-->
<instruction text (one or more lines)>
```

### Environment variables (platform-level defaults)

| Variable | Values | Purpose |
|----------|--------|---------|
| `NASIKO_PROMPT_COMMENTS` | `enabled` / `disabled` | Master switch |
| `NASIKO_INSTRUCTION_MODE` | `auto` / `manual` | Default addition mode |
| `NASIKO_PRUNE_MODE` | `auto` / `manual` / `<number>` | Default pruning mode |

File directives always override environment variables.
