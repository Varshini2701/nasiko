//! Prompt comment parser and stripper.
//!
//! Implements the "prompt comments" method from Chakrabarti (2025) — structured annotations
//! recording rationale alongside instructions. Comments are stripped before the instruction
//! text reaches the LLM, preserving the clean prompt while retaining provenance for
//! future maintenance.
//!
//! Format:
//! ```text
//! <!-- @prompt-comment
//!   added: 2026-08-13
//!   trigger: <what failure/observation led to this instruction>
//!   hypothesis: <why this instruction should help>
//!   outcome: pending | confirmed | revoked
//! -->
//! <instruction text that the agent sees>
//! ```

use chrono::Utc;

/// A single instruction with its associated prompt comment metadata.
#[derive(Debug, Clone)]
pub struct AnnotatedInstruction {
    pub text: String,
    pub comment: Option<PromptComment>,
}

/// Metadata block attached to an instruction.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PromptComment {
    pub added: String,
    pub trigger: String,
    pub hypothesis: String,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Confirmed,
    Revoked,
}

impl Outcome {
    #[allow(dead_code)]
    fn as_str(&self) -> &'static str {
        match self {
            Outcome::Pending => "pending",
            Outcome::Confirmed => "confirmed",
            Outcome::Revoked => "revoked",
        }
    }

    fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "confirmed" => Outcome::Confirmed,
            "revoked" => Outcome::Revoked,
            _ => Outcome::Pending,
        }
    }
}

const COMMENT_START: &str = "<!-- @prompt-comment";
const COMMENT_END: &str = "-->";

/// Strip all prompt-comment blocks from the raw instruction file content, returning only
/// the instruction text the LLM should see. Also removes revoked instructions entirely.
pub fn strip_comments(raw: &str) -> String {
    let instructions = parse(raw);
    instructions
        .into_iter()
        .filter(|inst| {
            inst.comment
                .as_ref()
                .map(|c| c.outcome != Outcome::Revoked)
                .unwrap_or(true)
        })
        .map(|inst| inst.text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse an instruction file into annotated instructions.
pub fn parse(raw: &str) -> Vec<AnnotatedInstruction> {
    let mut results = Vec::new();
    let mut lines = raw.lines().peekable();

    while let Some(line) = lines.next() {
        if line.trim_start().starts_with(COMMENT_START) {
            let comment = parse_comment_block(&mut lines);
            let text = collect_instruction_text(&mut lines);
            if !text.is_empty() {
                results.push(AnnotatedInstruction {
                    text,
                    comment: Some(comment),
                });
            }
        } else if !line.trim().is_empty() {
            let mut text = line.to_string();
            let rest = collect_instruction_text(&mut lines);
            if !rest.is_empty() {
                text.push('\n');
                text.push_str(&rest);
            }
            results.push(AnnotatedInstruction {
                text,
                comment: None,
            });
        }
    }
    results
}

/// Generate a prompt-comment block for a new or modified instruction.
pub fn generate_comment(trigger: &str, hypothesis: &str) -> String {
    let date = Utc::now().format("%Y-%m-%d").to_string();
    format!(
        "{COMMENT_START}\n  added: {date}\n  trigger: {trigger}\n  hypothesis: {hypothesis}\n  outcome: pending\n{COMMENT_END}"
    )
}

/// Render annotated instructions back to file format (instructions + comments).
#[allow(dead_code)]
pub fn render(instructions: &[AnnotatedInstruction]) -> String {
    let mut out = String::new();
    for inst in instructions {
        if let Some(ref comment) = inst.comment {
            out.push_str(&format!(
                "{COMMENT_START}\n  added: {}\n  trigger: {}\n  hypothesis: {}\n  outcome: {}\n{COMMENT_END}\n",
                comment.added, comment.trigger, comment.hypothesis, comment.outcome.as_str()
            ));
        }
        out.push_str(&inst.text);
        out.push('\n');
    }
    out
}

fn parse_comment_block(lines: &mut std::iter::Peekable<std::str::Lines<'_>>) -> PromptComment {
    let mut added = String::new();
    let mut trigger = String::new();
    let mut hypothesis = String::new();
    let mut outcome = Outcome::Pending;

    for line in lines.by_ref() {
        let trimmed = line.trim();
        if trimmed.starts_with(COMMENT_END) || trimmed.ends_with(COMMENT_END) {
            break;
        }
        if let Some(val) = trimmed.strip_prefix("added:") {
            added = val.trim().to_string();
        } else if let Some(val) = trimmed.strip_prefix("trigger:") {
            trigger = val.trim().to_string();
        } else if let Some(val) = trimmed.strip_prefix("hypothesis:") {
            hypothesis = val.trim().to_string();
        } else if let Some(val) = trimmed.strip_prefix("outcome:") {
            outcome = Outcome::parse(val);
        }
    }

    PromptComment {
        added,
        trigger,
        hypothesis,
        outcome,
    }
}

fn collect_instruction_text(lines: &mut std::iter::Peekable<std::str::Lines<'_>>) -> String {
    let mut text = String::new();
    while let Some(line) = lines.peek() {
        if line.trim().is_empty() || line.trim_start().starts_with(COMMENT_START) {
            break;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(lines.next().unwrap());
    }
    // Skip trailing blank lines between instructions.
    while lines.peek().is_some_and(|l| l.trim().is_empty()) {
        lines.next();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# Project Instructions

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
"#;

    #[test]
    fn strip_removes_comments_and_revoked() {
        let clean = strip_comments(SAMPLE);
        assert!(clean.contains("Always add explicit return type annotations"));
        assert!(clean.contains("Run `cargo fix`"));
        assert!(!clean.contains("Disable clippy::pedantic"));
        assert!(!clean.contains("@prompt-comment"));
        assert!(!clean.contains("trigger:"));
    }

    #[test]
    fn parse_extracts_metadata() {
        let instructions = parse(SAMPLE);
        // Header line + 3 annotated instructions; revoked one is still parsed.
        assert_eq!(instructions.len(), 4);

        let first_annotated = &instructions[1];
        let comment = first_annotated.comment.as_ref().unwrap();
        assert_eq!(
            comment.trigger,
            "test failures from missing type annotations"
        );
        assert_eq!(comment.outcome, Outcome::Pending);

        let revoked = &instructions[3];
        assert_eq!(revoked.comment.as_ref().unwrap().outcome, Outcome::Revoked);
    }

    #[test]
    fn generate_comment_has_today() {
        let c = generate_comment("tests broke", "adding a guard clause fixes it");
        assert!(c.contains("@prompt-comment"));
        assert!(c.contains("trigger: tests broke"));
        assert!(c.contains("outcome: pending"));
    }

    #[test]
    fn roundtrip_render() {
        let instructions = parse(SAMPLE);
        let rendered = render(&instructions);
        let re_parsed = parse(&rendered);
        assert_eq!(instructions.len(), re_parsed.len());
        for (a, b) in instructions.iter().zip(re_parsed.iter()) {
            assert_eq!(a.text, b.text);
        }
    }
}
