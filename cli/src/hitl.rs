//! Shared inline HITL (Human-in-the-Loop) prompt/resolve logic, reused by every interactive
//! surface that can hit a pause — `commands::chat` (both the orchestrator and `-a`/agent_proxy
//! paths) and `commands::maf`'s `--wait` poll loop. One shared `HitlPause` + `prompt_and_resolve_hitl`
//! keeps the prompt wording, colors, and `/hitl/{id}/resolve` semantics identical everywhere a
//! pause can surface, rather than each surface reimplementing its own.

use anyhow::{Context, Result};
use dialoguer::theme::ColorfulTheme;
use nasiko_utils::term;

use crate::api::Client;

/// Wraps `text` in the ANSI SGR `code` when colors are enabled (honors `NO_COLOR`, via
/// [`term::use_color`]); returns `text` unchanged otherwise. Centralizes what
/// `nasiko-utils::term`'s own status/box helpers already do, so every hand-colored HITL prompt
/// string here — the ones `term::print_box` can't reach because they're inside the body it
/// prints verbatim — respects `NO_COLOR` the same way.
fn colorize(code: &str, text: &str) -> String {
    if term::use_color() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// RAII guard that installs a no-op `SIGINT` handler for its lifetime, restoring the default
/// disposition on drop. Exists because every `dialoguer` widget used in this file — via
/// `console::Term::read_key`, which is hardcoded to `ctrlc_key: false` — turns a Ctrl+C keypress
/// into `libc::raise(SIGINT)` rather than a catchable `Key::CtrlC` (`console`'s own
/// `read_single_key`, `unix_term.rs`; there is no dialoguer-level flag to opt out of this). With
/// the default disposition, that raise kills the whole process instantly — before `console` ever
/// returns control to `dialoguer`, before `dialoguer` can return an `Err` for this module's `?`
/// fixes to catch. Confirmed live: an interrupted `auth_required`/free-text prompt silently
/// killed the entire `nasiko chat` session with zero output, though the HITL row itself still
/// stayed correctly `pending` either way — this guard fixes the "silently kills your whole
/// session" half, not a correctness gap in the S1/C1 fixes themselves.
///
/// With a no-op handler installed instead, `raise(SIGINT)` returns normally rather than
/// terminating, so the interrupted read genuinely returns `Err(Interrupted)` up through
/// `console`/`dialoguer` like any other I/O failure — which the `?` on every
/// `interact()`/`interact_text()` call in this file already handles correctly. Scoped narrowly
/// (installed only around the prompts that need it, restored the instant they're done) rather
/// than applied CLI-wide, so Ctrl+C keeps behaving normally — quitting immediately — everywhere
/// else in `nasiko` (a long-running deploy, `commands::chat`'s own outer per-turn prompt, etc.).
/// A no-op on non-Unix targets: POSIX `SIGINT` doesn't apply there, and this is not a platform
/// this crate is otherwise built for (see `oss/cli/Cargo.toml`'s `cfg(unix)` gate on `libc`).
#[cfg(unix)]
struct SigintGuard;

#[cfg(unix)]
impl SigintGuard {
    fn install() -> Self {
        // SAFETY: `sigint_noop` is `extern "C" fn(c_int)`, the exact signature `signal(2)`
        // requires; it touches no shared state, so it's sound to run at any point, including
        // mid-syscall on another thread.
        unsafe {
            libc::signal(libc::SIGINT, sigint_noop as *const () as libc::sighandler_t);
        }
        Self
    }
}

#[cfg(unix)]
extern "C" fn sigint_noop(_signum: libc::c_int) {}

#[cfg(unix)]
impl Drop for SigintGuard {
    fn drop(&mut self) {
        // SAFETY: restores the disposition every process starts with; sound for the same reason
        // as `install`'s call.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
    }
}

#[cfg(not(unix))]
struct SigintGuard;

#[cfg(not(unix))]
impl SigintGuard {
    fn install() -> Self {
        Self
    }
}

/// A pause raised mid-turn/mid-step, wherever it was discovered — the
/// `"type":"hitl"` SSE data part for `chat`, or the `hitl[]` entry on a MAF
/// execution response for `maf`. `id` is always the real `hitl_requests.id`
/// to resolve against — even when this pause is an MCP tool-approval
/// mirrored onto another conversation, the substitution already happened
/// server-side (`resolve_display_row`) before either shape was built.
pub struct HitlPause {
    pub id: String,
    pub kind: String,
    pub question: serde_json::Value,
    /// Who's asking, if known — an agent name for `chat`, a step label
    /// (e.g. "step 2") for `maf`. Falls back to a generic "agent" label.
    pub agent: Option<String>,
}

/// Shows the pause inline and blocks on the human's response, then POSTs the
/// resolution to `/api/hitl/{id}/resolve`. A pause someone else already
/// resolved (`already_resolved: true` in a 200 response) is a normal
/// outcome, not an error — printed and treated as done.
pub fn prompt_and_resolve_hitl(pause: &HitlPause) -> Result<()> {
    // Covers every dialoguer-based prompt below (including `prompt_single_select`/
    // `prompt_multi_select`, called from within this function) so a Ctrl+C on any of them is a
    // catchable error instead of an instant, silent process kill — see `SigintGuard`'s own doc
    // comment. Harmless overlap with `run_combo_select`'s own crossterm raw-mode Ctrl+C
    // trapping: raw mode already stops the terminal from generating SIGINT at all, so this
    // guard's handler simply never fires during that widget's turn.
    let _sigint_guard = SigintGuard::install();
    let who = pause.agent.as_deref().unwrap_or("agent");
    let message = |key: &str| {
        pause
            .question
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
    };

    let body = match pause.kind.as_str() {
        "tool_approval" => {
            let tool = pause.question.get("tool_name").and_then(|v| v.as_str());
            let connector = pause.question.get("connector_id").and_then(|v| v.as_str());
            let msg = message("message");
            let mut panel = format!(
                "{who} wants to run  {}",
                colorize("1", tool.unwrap_or("(unknown tool)"))
            );
            if let Some(c) = connector {
                panel.push_str(&format!("\nvia connector  {c}"));
            }
            if !msg.is_empty() {
                panel.push_str(&format!("\n{msg}"));
            }
            println!();
            term::print_box(Some("⏸ APPROVAL NEEDED"), &panel, "33");
            // Same three choices the web UI's tool-approval card offers — "session"
            // scope (§5 of the HITL plan) is a real, separately-meaningful option, not
            // just a variant of "once", so it needs its own place in the prompt rather
            // than being folded into a yes/no.
            let options = ["Allow once", "Always allow", "Deny"];
            let choice = dialoguer::Select::new()
                .with_prompt("Approve?")
                .items(&options)
                .default(0)
                .interact()
                // Fail closed on an interrupted/unreadable prompt — never treat "couldn't
                // ask" as permission granted.
                .unwrap_or(2);
            let (decision, scope) = match choice {
                0 => ("approve", "once"),
                1 => ("approve", "session"),
                _ => ("reject", "once"),
            };
            serde_json::json!({ "decision": decision, "scope": scope })
        }
        "auth_required" => {
            let msg = message("message");
            let mut panel = format!(
                "{who} needs authorization: {}",
                if msg.is_empty() {
                    "re-authentication required"
                } else {
                    msg
                }
            );
            let connector_id = pause.question.get("connector_id").and_then(|v| v.as_str());
            if let Some(c) = pause
                .question
                .get("connector")
                .and_then(|v| v.as_str())
                .or(connector_id)
            {
                panel.push_str(&format!("\nconnector  {c}"));
            }
            // Two different shapes carry this, depending on which layer raised the
            // pause: an agent's own token-URL flow nests it under "metadata" (its
            // own event's metadata, forwarded as-is into `question`), while the MCP
            // gateway's connector auth_required puts it top-level on `question`
            // itself (`handle_auth_required` in oss/mcp-gateway/src/protocol.rs).
            if let Some(url) = pause
                .question
                .get("auth_url")
                .or_else(|| pause.question.pointer("/metadata/auth_url"))
                .and_then(|v| v.as_str())
            {
                panel.push_str(&format!("\nopen this to authenticate: {url}"));
            }
            // `connector_id` only appears on a real MCP-gateway-raised pause
            // (`handle_auth_required`) — an agent's own fixture/demo auth_required
            // has no connector to reconnect, so there's no CLI fix to suggest.
            if let Some(id) = connector_id {
                panel.push_str(&format!(
                    "\nor from the CLI: nasiko mcp connect --connector-id {id}\nthen confirm with: nasiko mcp connections"
                ));
            }
            println!();
            term::print_box(Some("⏸ AUTHORIZATION NEEDED"), &panel, "33");
            term::read_text_line("Once you've finished, press Enter to continue")?;
            serde_json::json!({ "auth_action": "confirm" })
        }
        // "input_required" and any forward-compatible unknown kind: a plain question, or —
        // when the agent's `ask_human` attached the selectable-options extension — a
        // single/multi-select question with optional free-text custom input (mirrors
        // `hitl-card.js`'s rendering; `StructuredOptions::parse`/`resolve_structured_answer`
        // in `router/hitl.rs` are the shared source of truth this must stay wire-compatible
        // with).
        _ => {
            let msg = message("message");
            println!();
            let panel_body = match (pause.question.get("header").and_then(|v| v.as_str()), msg) {
                (Some(h), "") => h.to_string(),
                (Some(h), m) => format!("{h}\n{m}"),
                (None, "") => "(needs your input)".to_string(),
                (None, m) => m.to_string(),
            };
            term::print_box(Some(&format!("⏸ {who}")), &panel_body, "33");

            match StructuredOptions::parse(&pause.question) {
                Some(opts) if opts.multi_select => prompt_multi_select(&opts)?,
                Some(opts) => prompt_single_select(&opts)?,
                None => {
                    let answer = term::read_text_line(&colorize("1;36", "❯ you"))?;
                    serde_json::json!({ "answer": answer })
                }
            }
        }
    };

    let resp: serde_json::Value =
        Client::from_active_cluster()?.post_json(&format!("/hitl/{}/resolve", pause.id), &body)?;

    if resp.get("already_resolved").and_then(|v| v.as_bool()) == Some(true) {
        eprintln!("  {}", colorize("2", "(already resolved by someone else)"));
    } else {
        eprintln!(
            "  {}",
            colorize(
                "2",
                &format!("(HITL ID: {} — for reference only)", pause.id)
            )
        );
    }
    println!();
    Ok(())
}

/// A `question`'s selectable-options extension, parsed the same way
/// `router/hitl.rs::StructuredOptions::parse` does server-side — `None` means this is a plain
/// `input_required` question (every pre-extension row, and any row whose agent never set
/// `options`), which must fall back to the bare free-text prompt exactly as before this
/// extension existed.
struct StructuredOptions {
    labels: Vec<String>,
    multi_select: bool,
    allow_custom_input: bool,
}

impl StructuredOptions {
    fn parse(question: &serde_json::Value) -> Option<Self> {
        let options = question.get("options")?.as_array()?;
        let labels: Vec<String> = options
            .iter()
            .filter_map(|o| o.get("label")?.as_str().map(str::to_string))
            .collect();
        if labels.is_empty() {
            return None;
        }
        Some(Self {
            labels,
            multi_select: question
                .get("multi_select")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            allow_custom_input: question
                .get("allow_custom_input")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        })
    }
}

/// Single-select: the offered labels, plus a trailing "Something else" choice when custom input
/// is allowed. Unlike a plain list, landing the cursor on that trailing row opens its free-text
/// field immediately (no separate confirm-then-prompt step) via [`select_with_inline_custom`];
/// the answer sent (not the literal "Something else" string) matches `resolve_structured_answer`'s
/// "a bare `answer` string, custom or predefined — membership in `opts.labels` is what
/// distinguishes the two" contract either way.
fn prompt_single_select(opts: &StructuredOptions) -> Result<serde_json::Value> {
    let answer = if opts.allow_custom_input {
        select_with_inline_custom(&opts.labels)?
    } else {
        let choice = dialoguer::Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Choose one")
            .items(&opts.labels)
            .default(0)
            .interact()?;
        opts.labels[choice].clone()
    };
    eprintln!("  {} {answer}", colorize("32", "✓"));
    Ok(serde_json::json!({ "answer": answer }))
}

/// Combo select+inline-text prompt used only when the question allows custom input. A plain
/// `dialoguer::Select` blocks on Enter before any custom-text step can start, forcing a visible
/// select-then-separate-prompt handoff; this instead opens the free-text field the instant the
/// cursor reaches the trailing row, so picking "Something else" and typing it feel like one
/// motion. Hand-rolled over `crossterm` (already a CLI dependency for the TUI, so no new crate)
/// since dialoguer has no combo-box primitive. Errors out — rather than fabricating an answer —
/// on a non-interactive terminal, Ctrl+C, or Esc: this widget can select a real predefined label,
/// so silently guessing one on failure would submit something the human never chose. The HITL
/// request is left `pending`, resolvable later via the API, the same way an interrupted
/// `tool_approval`/`auth_required` prompt already leaves its request unresolved.
fn select_with_inline_custom(labels: &[String]) -> Result<String> {
    // stderr, not stdout: matches `term::print_box`'s own convention for HITL pause prompts
    // (see its module doc) so `nasiko chat ... | tee log` never gets raw cursor/clear codes
    // mixed into piped stdout.
    eprintln!(
        "{} {}",
        colorize("1", "? Choose one"),
        colorize("2", "(\u{2191}/\u{2193} to move, enter to confirm)")
    );
    let (selected, custom) = run_combo_select(labels, false)?;
    Ok(match selected.first() {
        Some(&i) => labels[i].clone(),
        None => custom.unwrap_or_default(),
    })
}

/// RAII guard so raw mode (and bracketed paste) are always turned back off — on a normal
/// return, an error, or a panic — rather than leaving the user's shell in a broken (no-echo,
/// no-line-buffering, raw-paste) state.
struct RawMode;

impl RawMode {
    fn enable() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()
            .context("this question needs an interactive terminal to answer")?;
        // Bracketed paste delivers a paste as one `Event::Paste`, so an embedded newline is
        // never misread as Enter — which would submit early and leak the rest of the paste as
        // stray keystrokes into whatever runs next. Best-effort: not every terminal supports
        // it, and this widget still works via plain typing if it's unavailable.
        let _ = crossterm::execute!(std::io::stderr(), crossterm::event::EnableBracketedPaste);
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stderr(), crossterm::event::DisableBracketedPaste);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Multi-select: checkboxes for the offered labels, plus — when custom input is allowed — a
/// trailing "Other" checkbox whose free-text field opens the instant it's checked (via
/// [`multi_select_with_inline_custom`]) rather than as a separate step after confirming the whole
/// list. "Other" itself is never included in the submitted `answer` labels, since
/// `resolve_structured_answer` rejects any ticked item not in `opts.labels` outright.
fn prompt_multi_select(opts: &StructuredOptions) -> Result<serde_json::Value> {
    let (selected, custom) = if opts.allow_custom_input {
        multi_select_with_inline_custom(&opts.labels)?
    } else {
        let chosen = dialoguer::MultiSelect::with_theme(&ColorfulTheme::default())
            .with_prompt("Select all that apply (space to toggle, enter to confirm)")
            .items(&opts.labels)
            .interact()?;
        let selected = chosen.into_iter().map(|i| opts.labels[i].clone()).collect();
        (selected, None)
    };

    let summary = match (selected.is_empty(), &custom) {
        (true, None) => "(none)".to_string(),
        (true, Some(c)) => c.clone(),
        (false, None) => selected.join(", "),
        (false, Some(c)) => format!("{}, {c}", selected.join(", ")),
    };
    eprintln!("  {} {summary}", colorize("32", "✓"));

    Ok(match custom {
        Some(custom) => serde_json::json!({ "answer": selected, "custom_answer": custom }),
        None => serde_json::json!({ "answer": selected }),
    })
}

/// Combo multi-select+inline-text prompt used only when the question allows custom input.
/// Checking the trailing "Other" row opens its free-text field immediately — type right away, no
/// separate confirm-then-prompt step — mirroring [`select_with_inline_custom`]'s single-select
/// behavior. Backspacing past an empty field unchecks "Other" again, so there's no dead field
/// left checked with nothing in it. Errors out — rather than fabricating an answer — on a
/// non-interactive terminal, Ctrl+C, or Esc, the same as the single-select combo: silently
/// submitting `{"answer": []}` for an aborted prompt would look like a deliberate "none of
/// these apply," discarding whatever the human had already checked.
fn multi_select_with_inline_custom(labels: &[String]) -> Result<(Vec<String>, Option<String>)> {
    // stderr, not stdout: same reasoning as `select_with_inline_custom`.
    eprintln!(
        "{} {}",
        colorize("1", "? Select all that apply"),
        colorize(
            "2",
            "(space to toggle, \u{2191}/\u{2193} to move, enter to confirm)"
        )
    );
    let (selected, custom) = run_combo_select(labels, true)?;
    let selected = selected.into_iter().map(|i| labels[i].clone()).collect();
    Ok((selected, custom))
}

/// Shared raw-mode combo loop behind both [`select_with_inline_custom`] (`multi: false`) and
/// [`multi_select_with_inline_custom`] (`multi: true`) — the redraw (relative-cursor-movement
/// framing, tracked via `prev_rows`), Ctrl+C/Esc/arrow-key handling, and paste-into-buffer logic
/// are identical between the two widgets; only the row glyph, what Enter returns, Space, and the
/// Backspace/typing activation rule differ, and those are branched on `multi` below. Returns the
/// indices of predefined labels the human picked (single-select: at most one, via Enter on a
/// non-custom row) plus the free-text custom answer if the trailing row was used instead
/// (single-select) or in addition (multi-select, "Other" plus any checked labels).
fn run_combo_select(labels: &[String], multi: bool) -> Result<(Vec<usize>, Option<String>)> {
    use crossterm::cursor::{MoveToColumn, MoveUp};
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    use crossterm::execute;
    use crossterm::terminal::{Clear, ClearType};
    use std::io::{Write, stderr};

    let custom_idx = labels.len();
    let row_count = labels.len() + 1;
    let mut checked = vec![false; labels.len()];
    let mut custom_checked = false;
    let mut cursor_idx = 0usize;
    let mut buffer = String::new();
    let mut out = stderr();

    let _raw = RawMode::enable()?;
    // Tracks exactly how many physical terminal rows the previous frame occupied — including a
    // long custom answer or label wrapping to more than one physical row — so the next redraw can
    // move the cursor up by that many rows before clearing. This replaced a `SavePosition`/
    // `RestorePosition` scheme (ANSI DECSC/DECRC): those save an *absolute* screen coordinate,
    // which goes stale the moment the terminal scrolls — a real risk here, since 8+ option rows
    // routinely exceed a modest terminal's visible height — leaving stale rows on screen instead
    // of being overwritten (confirmed live: a multi-select with several options showed the first
    // couple of rows duplicated many times over). Relative cursor movement has no such ambiguity.
    let mut prev_rows = 0usize;
    loop {
        let cols = term::terminal_cols();
        if prev_rows > 0 {
            execute!(
                out,
                MoveUp(prev_rows.min(u16::MAX as usize) as u16),
                MoveToColumn(0),
                Clear(ClearType::FromCursorDown)
            )?;
        } else {
            execute!(out, Clear(ClearType::FromCursorDown))?;
        }

        // Multi-select matches dialoguer's own `ColorfulTheme` checkbox styling (green ✔ /
        // magenta ⬚, cyan label when focused); single-select has no checkbox, just a `❯` cursor.
        let mut lines: Vec<String> = Vec::with_capacity(row_count);
        for (i, label) in labels.iter().enumerate() {
            if multi {
                let glyph = if checked[i] {
                    colorize("32", "✔")
                } else {
                    colorize("35", "⬚")
                };
                let text = if i == cursor_idx {
                    colorize("36", label)
                } else {
                    label.clone()
                };
                lines.push(format!("{glyph} {text}"));
            } else if i == cursor_idx {
                lines.push(colorize("36", &format!("❯ {label}")));
            } else {
                lines.push(format!("  {label}"));
            }
        }
        if multi {
            let glyph = if custom_checked {
                colorize("32", "✔")
            } else {
                colorize("35", "⬚")
            };
            let text = if cursor_idx == custom_idx {
                colorize("36", "Other")
            } else {
                "Other".to_string()
            };
            let mut line = format!("{glyph} {text}");
            if custom_checked {
                line.push_str(&colorize("36", &format!(": {buffer}")));
                line.push('▏');
            }
            lines.push(line);
        } else if cursor_idx == custom_idx {
            lines.push(format!(
                "{}▏",
                colorize("36", &format!("❯ Something else: {buffer}"))
            ));
        } else {
            lines.push("  Something else…".to_string());
        }

        prev_rows = 0;
        for line in &lines {
            write!(out, "{line}\r\n")?;
            prev_rows += term::visible_width(line).max(1).div_ceil(cols);
        }
        out.flush()?;

        // Single-select: the custom field is "active" exactly when the cursor sits on it —
        // there's no separate checked state. Multi-select: activity is the persistent
        // "Other" checkbox, independent of where the cursor currently is.
        let custom_active = if multi {
            custom_checked
        } else {
            cursor_idx == custom_idx
        };

        match read()? {
            Event::Paste(text) if cursor_idx == custom_idx => {
                if multi {
                    custom_checked = true;
                }
                buffer.push_str(&text.replace(['\n', '\r'], " "));
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    anyhow::bail!("cancelled");
                }
                KeyCode::Up => cursor_idx = cursor_idx.checked_sub(1).unwrap_or(row_count - 1),
                KeyCode::Down => cursor_idx = (cursor_idx + 1) % row_count,
                KeyCode::Enter => {
                    if multi {
                        let selected = (0..labels.len()).filter(|&i| checked[i]).collect();
                        let custom =
                            (custom_checked && !buffer.trim().is_empty()).then_some(buffer);
                        return Ok((selected, custom));
                    }
                    return Ok(if cursor_idx == custom_idx {
                        (vec![], Some(buffer))
                    } else {
                        (vec![cursor_idx], None)
                    });
                }
                // Multi-select only: Space is a toggle everywhere, except it types a literal
                // space once "Other" is already checked (so free text can contain spaces).
                // Single-select has no toggle concept, so Space falls through to the generic
                // typing arm below like any other character.
                KeyCode::Char(' ') if multi => {
                    if cursor_idx == custom_idx {
                        if custom_checked {
                            buffer.push(' ');
                        } else {
                            custom_checked = true;
                        }
                    } else {
                        checked[cursor_idx] = !checked[cursor_idx];
                    }
                }
                KeyCode::Backspace if cursor_idx == custom_idx && custom_active => {
                    // A separate `let` (not `buffer.pop().is_none() && multi` inlined into an
                    // `if`) deliberately, so clippy's `collapsible_if` can't fold this into the
                    // match guard above — `pop()`'s removal is a side effect that must run
                    // whenever this arm is reached, not something conditional on `multi`, which
                    // collapsing into the guard would make it (a guard that evaluates false
                    // never commits to the arm, even though the pop already happened).
                    let popped = buffer.pop();
                    if popped.is_none() && multi {
                        custom_checked = false;
                    }
                }
                // Multi-select only: typing on "Other" before it's checked starts editing right
                // away — matching the single-select combo's "type immediately" promise — instead
                // of being silently swallowed until the human happens to press Space first.
                KeyCode::Char(c) if cursor_idx == custom_idx && multi && !custom_checked => {
                    custom_checked = true;
                    buffer.push(c);
                }
                KeyCode::Char(c) if cursor_idx == custom_idx && custom_active => buffer.push(c),
                KeyCode::Esc => anyhow::bail!("cancelled"),
                _ => {}
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod structured_options_tests {
    use super::StructuredOptions;

    #[test]
    fn parse_reads_labels_multi_select_and_allow_custom_input() {
        let question = serde_json::json!({
            "message": "Which sections?",
            "options": [{"label": "Intro"}, {"label": "Conclusion"}],
            "multi_select": true,
            "allow_custom_input": true,
        });
        let opts = StructuredOptions::parse(&question).unwrap();
        assert_eq!(opts.labels, vec!["Intro", "Conclusion"]);
        assert!(opts.multi_select);
        assert!(opts.allow_custom_input);
    }

    #[test]
    fn parse_defaults_multi_select_and_allow_custom_input_to_false() {
        let question = serde_json::json!({
            "message": "Pick one",
            "options": [{"label": "A"}],
        });
        let opts = StructuredOptions::parse(&question).unwrap();
        assert!(!opts.multi_select);
        assert!(!opts.allow_custom_input);
    }

    /// A plain (pre-extension) `input_required` row has no `options` at all — must fall back
    /// to the bare free-text prompt, not a zero-item select.
    #[test]
    fn parse_is_none_for_a_plain_question() {
        let question = serde_json::json!({ "message": "What's your name?" });
        assert!(StructuredOptions::parse(&question).is_none());
    }

    /// Mirrors `hoist_structured_options`'s own "drop the whole block rather than render a
    /// broken prompt" rule — an empty `options` array reaching this far (e.g. a future
    /// server bug) must not surface as a selectable question with nothing to select.
    #[test]
    fn parse_is_none_for_an_empty_options_array() {
        let question = serde_json::json!({ "message": "Pick one", "options": [] });
        assert!(StructuredOptions::parse(&question).is_none());
    }
}

#[cfg(test)]
mod colorize_tests {
    use super::colorize;
    use std::sync::Mutex;

    // `colorize` reads `NO_COLOR` (via `term::use_color`) from the process environment, which
    // `cargo test`'s default multi-threaded runner shares across every test in this binary. This
    // mutex serializes only the two tests below against each other; no other test in this crate
    // reads `NO_COLOR`, so nothing else can observe the mutation mid-flight.
    static NO_COLOR_LOCK: Mutex<()> = Mutex::new(());

    /// Restores `NO_COLOR` to its state from before the test on drop, including on panic — same
    /// "always undo, even on the unwind path" shape as this file's own `RawMode` guard.
    struct NoColorGuard(Option<String>);

    impl NoColorGuard {
        fn set(value: Option<&str>) -> Self {
            let previous = std::env::var("NO_COLOR").ok();
            // SAFETY: serialized by `NO_COLOR_LOCK`, held for this guard's whole lifetime.
            unsafe {
                match value {
                    Some(v) => std::env::set_var("NO_COLOR", v),
                    None => std::env::remove_var("NO_COLOR"),
                }
            }
            Self(previous)
        }
    }

    impl Drop for NoColorGuard {
        fn drop(&mut self) {
            // SAFETY: same as `set` — still under `NO_COLOR_LOCK`.
            unsafe {
                match &self.0 {
                    Some(v) => std::env::set_var("NO_COLOR", v),
                    None => std::env::remove_var("NO_COLOR"),
                }
            }
        }
    }

    #[test]
    fn wraps_in_the_given_sgr_code_when_color_is_enabled() {
        let _lock = NO_COLOR_LOCK.lock().unwrap();
        let _guard = NoColorGuard::set(None);
        assert_eq!(colorize("32", "✓"), "\x1b[32m✓\x1b[0m");
        assert_eq!(colorize("1;36", "❯ you"), "\x1b[1;36m❯ you\x1b[0m");
    }

    #[test]
    fn returns_plain_text_when_no_color_is_set() {
        let _lock = NO_COLOR_LOCK.lock().unwrap();
        let _guard = NoColorGuard::set(Some("1"));
        assert_eq!(colorize("32", "✓"), "✓");
        assert_eq!(colorize("1;36", "❯ you"), "❯ you");
    }
}
