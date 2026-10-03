# Prompt Comments: Automatic Instruction Maintenance

Based on "Why Does CLAUDE.md Keep Growing? Catastrophic Remembering in Agentic Coding"
(Chakrabarti, 2025). Implemented in the Nasiko coding agent as a system-level feature that
prevents unbounded growth of workspace instruction files.

## Opt-in / Opt-out

This feature is **disabled by default**. The coding agent behaves exactly as before unless
the user explicitly opts in.

### How to enable

**Option A: In the instruction file** (per-workspace, recommended):

Add this line to your `NASIKO.md` (or `CLAUDE.md`, `.nasiko/instructions.md`):
```markdown
<!-- @prompt-comments enabled -->
```

**Option B: Via environment variable** (platform-level, all workspaces):

Set on the coding agent container at deploy time:
```
NASIKO_PROMPT_COMMENTS=enabled
```

**Option C: Via the control plane UI** (the same thing as Option B, without a redeploy edit):

Agent detail page > Settings > Features > "Prompt comments". The toggle writes
`metadata.features.prompt_comments` on the agent, and `AppState::agent_env` turns every
`features.<key>` entry into `NASIKO_<KEY>` on the container. Takes effect on next restart.

### How to disable

**If enabled at the platform level and you want a specific workspace to opt out:**

Add this to the workspace's instruction file:
```markdown
<!-- @prompt-comments disabled -->
```
The file directive always wins over the env var.

**If enabled at the file level:** Change `enabled` to `disabled`, or remove the line entirely.

**If enabled at the env var level:** Unset `NASIKO_PROMPT_COMMENTS` or set it to `disabled`.

### Resolution order

| Platform env var | File directive | Result |
|---|---|---|
| not set | not set | **Disabled** (default) |
| `enabled` | not set | Enabled |
| `enabled` | `disabled` | **Disabled** (file wins) |
| not set | `enabled` | Enabled |
| `disabled` | `enabled` | **Enabled** (file wins) |

### When the flag takes effect

The feature state is read **once at session start**. If the user changes the directive
mid-session (by editing the file directly), the change takes effect on the next session.
There is no runtime toggle within an active session.

### What changes when enabled vs disabled

When disabled (the default):
- Workspace instruction files are still discovered and injected into the prompt (that
  behavior is independent of this feature)
- But the `update_instructions` and `prune_instructions` tools are NOT exposed to the agent
- No automatic pruning occurs
- The agent cannot add or modify instructions on its own

When enabled:
- The agent gains `update_instructions` and `prune_instructions` tools
- Auto-pruning can be configured (still off by default within the feature)
- Instruction addition mode can be configured (manual by default)

## Problem

Instruction files (NASIKO.md, CLAUDE.md) grow unbounded. Adding rules is cheap, but deletion
is expensive once the original rationale disappears. The paper terms this "catastrophic
remembering": the inverse of catastrophic forgetting.

Empirically (247,694 instruction lifetimes across 1,867 repos): prompts triple in size
(+226%), gaining 4.9 net instructions per commit. Older instructions face exponentially
lower deletion probability. The fix: structured annotations encoding rationale reduce excess
growth from +211.3% to +1.4% and improve agent performance by up to 23.1%.

## How It Works

### Instruction file format

Users place a `NASIKO.md` (or `.nasiko/instructions.md`, `CLAUDE.md`, `.claude/instructions.md`)
in their workspace root. The file contains instructions for the coding agent, optionally
annotated with prompt comments:

```markdown
<!-- @prompt-comments enabled -->
<!-- @instructions auto -->
<!-- @pruning auto -->

# Project Instructions

<!-- @prompt-comment
  added: 2026-08-13
  trigger: test failures from missing type annotations
  hypothesis: explicit types prevent inference errors in generics
  outcome: pending
-->
- Always add explicit return type annotations to public functions.

<!-- @prompt-comment
  added: 2026-08-10
  trigger: user reported stale imports after refactors
  hypothesis: running organize-imports after edits catches dead imports
  outcome: confirmed
-->
- Run `cargo fix` after any refactor that moves or removes items.

<!-- @prompt-comment
  added: 2026-08-01
  trigger: old lint rule that caused more noise than value
  hypothesis: disabling the lint would reduce churn
  outcome: revoked
-->
- Disable clippy::pedantic globally.
```

### What the agent sees

Prompt comments are stripped before injection. Revoked instructions are excluded entirely.
The agent receives only:

```
- Always add explicit return type annotations to public functions.
- Run `cargo fix` after any refactor that moves or removes items.
```

### What happens at session start

1. The agent probes the sandbox for instruction files (priority order: NASIKO.md,
   .nasiko/instructions.md, CLAUDE.md, .claude/instructions.md).
2. Parses the file, separates instructions from `<!-- @prompt-comment -->` blocks.
3. Drops any instruction marked `outcome: revoked`.
4. Strips all comment metadata.
5. If auto-pruning is enabled and the threshold is exceeded, runs one LLM call to
   review stale instructions.
6. Injects the clean instruction text as a preamble before the hardcoded system prompt.

### Extra LLM calls

| Scenario | Extra LLM calls | Cost |
|----------|----------------|------|
| Normal session (under threshold) | 0 | Zero. Pure string processing. |
| Auto-prune triggered (over threshold) | 1 | Small prompt (instruction listing only, no codebase context). |
| User says "prune my instructions" | 1 | Same as above, on demand. |
| Agent adds an instruction | 0 | Tool-call arguments within an existing turn. |

## User Configuration

Three directives control behavior. All can be set in the instruction file (highest priority)
or via environment variables on the agent container (platform-level default).

### Feature gate (required)

The entire feature must be opted into before anything else applies.

| Setting | Behavior |
|---------|----------|
| `<!-- @prompt-comments enabled -->` | Feature is active: tools exposed, pruning available |
| `<!-- @prompt-comments disabled -->` | Feature is off (same as omitting the directive) |
| No directive | **Disabled by default** |

Environment variable fallback: `NASIKO_PROMPT_COMMENTS=enabled|disabled`

### Instruction addition mode

Controls whether the agent adds instructions proactively or only when explicitly asked.

| Setting | Behavior |
|---------|----------|
| `<!-- @instructions manual -->` | Agent only adds instructions when user explicitly asks ("remember this", "add a rule") |
| `<!-- @instructions auto -->` | Agent adds instructions at its own judgment (after fixing bugs, discovering conventions) |
| No directive | Defaults to `manual` |

Environment variable fallback: `NASIKO_INSTRUCTION_MODE=auto|manual`

### Pruning mode

Controls automatic pruning of stale instructions.

| Setting | Behavior |
|---------|----------|
| `<!-- @pruning manual -->` | No automatic pruning. User can still trigger manually. |
| `<!-- @pruning auto -->` | Auto-prune when annotated instructions exceed 20 |
| `<!-- @pruning 30 -->` | Auto-prune at a custom threshold (any integer) |
| No directive | Defaults to `manual` |

Environment variable fallback: `NASIKO_PRUNE_MODE=auto|manual|<N>`

### Manual pruning

Regardless of the `@pruning` directive, the user can always say "prune my instructions"
(or similar). The agent calls the `prune_instructions` tool, which triggers the review
LLM call immediately.

## Tools

Two new tools are available to the coding agent:

### `update_instructions`

Adds a new instruction with a prompt comment recording rationale.

Parameters:
- `instruction` (required): The rule text for future sessions
- `trigger` (required): What failure or observation led to this instruction
- `hypothesis` (required): Why this instruction should help

### `prune_instructions`

Reviews all annotated instructions and revokes stale ones. No parameters.
Triggers one LLM call that receives the instruction listing with rationales and returns
a JSON array of indices to revoke.

## Prompt Comment Format

```
<!-- @prompt-comment
  added: <YYYY-MM-DD>
  trigger: <what went wrong or was observed>
  hypothesis: <why this instruction should help>
  outcome: pending | confirmed | revoked
-->
<instruction text>
```

Fields:
- `added`: Date the instruction was created (auto-generated)
- `trigger`: The failure, bug, or observation that motivated this rule
- `hypothesis`: The expected benefit of following this rule
- `outcome`: Lifecycle state
  - `pending`: Not yet validated
  - `confirmed`: Working as expected (kept in prompt)
  - `revoked`: No longer relevant (excluded from prompt)

## Architecture

```
src/
  prompt_comments.rs   Parser, stripper, generator, renderer for @prompt-comment blocks
  instructions.rs      Discovery, injection, pruning, mode parsing, CRUD
  main.rs              Wiring: discovery at startup, pruning gate, tool dispatch
  tools.rs             update_instructions and prune_instructions tool definitions
```

No external services, database changes, or new dependencies. Pure text processing at the
agent level. Works with any OpenAI-compatible LLM backend.

## Environment Variables

| Var | Default | Purpose |
|-----|---------|---------|
| `NASIKO_PROMPT_COMMENTS` | `disabled` | Master switch: `enabled` activates the feature |
| `NASIKO_INSTRUCTION_MODE` | `manual` | Whether agent auto-adds instructions (`auto` or `manual`) |
| `NASIKO_PRUNE_MODE` | `manual` | Whether auto-pruning is enabled (`auto`, `manual`, or a threshold number) |

File-level directives take priority over environment variables.

## Example Instruction File

A complete example showing all directives and multiple instructions at various lifecycle stages:

```markdown
<!-- @prompt-comments enabled -->
<!-- @instructions auto -->
<!-- @pruning 15 -->

# My Project Rules

<!-- @prompt-comment
  added: 2026-08-01
  trigger: repeated test failures from async race conditions
  hypothesis: wrapping shared state in Arc<Mutex<>> prevents data races
  outcome: confirmed
-->
- All shared mutable state must use Arc<Mutex<T>> or Arc<RwLock<T>>.

<!-- @prompt-comment
  added: 2026-08-05
  trigger: user asked to always use tracing instead of println
  hypothesis: structured logging is more useful in production
  outcome: pending
-->
- Use tracing::info!/debug!/error! instead of println! for all output.

<!-- @prompt-comment
  added: 2026-07-20
  trigger: thought disabling borrow checker warnings would speed iteration
  hypothesis: fewer warnings = faster development
  outcome: revoked
-->
- Add #[allow(unused)] to all modules.
```

In this example:
- The feature is explicitly enabled (`@prompt-comments enabled`)
- The agent will proactively add new instructions (auto mode)
- Auto-pruning triggers at 15 annotated instructions
- The first instruction is confirmed and stays in the prompt
- The second is pending (not yet validated)
- The third is revoked and excluded from the prompt entirely

Without `<!-- @prompt-comments enabled -->`, none of the instruction management
behavior activates. The file is still read and injected into the prompt (that is basic
agent behavior), but the agent cannot add, modify, or prune instructions on its own.
