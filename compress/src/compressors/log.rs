//! Keeps the head, the tail, and everything that went wrong. Drops the INFO/DEBUG middle.
//!
//! A log reaches a model because something in it failed, so severity is the retention signal —
//! never position alone.

use crate::marker;
use crate::policy::Policy;

const SEVERE: [&str; 5] = ["ERROR", "WARN", "WARNING", "FATAL", "PANIC"];

pub(super) fn compress(input: &str, policy: &Policy<'_>) -> Option<String> {
    let lines: Vec<&str> = input.lines().collect();
    let (head, tail) = policy.level.log_head_tail();
    if lines.len() <= head + tail + 1 {
        return None;
    }

    let mut keep = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        keep[i] = i < head || i >= lines.len() - tail || is_severe(line) || is_stack_frame(line);
    }

    // A stack trace is only useful attached to the line that raised it.
    for i in 0..lines.len() {
        if keep[i] && is_severe(lines[i]) {
            for follower in lines.iter().enumerate().skip(i + 1) {
                let (j, line) = follower;
                if !is_stack_frame(line) {
                    break;
                }
                keep[j] = true;
            }
        }
    }

    Some(render(&lines, &keep, policy))
}

fn render(lines: &[&str], keep: &[bool], policy: &Policy<'_>) -> String {
    let mut out = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum::<usize>());
    let mut dropped = 0usize;

    for (i, line) in lines.iter().enumerate() {
        if keep[i] {
            if dropped > 0 {
                out.push_str(&marker::elided_lines(dropped, policy.recovery_ref));
                out.push('\n');
                dropped = 0;
            }
            out.push_str(line);
            out.push('\n');
        } else {
            dropped += 1;
        }
    }
    if dropped > 0 {
        out.push_str(&marker::elided_lines(dropped, policy.recovery_ref));
        out.push('\n');
    }
    out
}

fn is_severe(line: &str) -> bool {
    let prefix = crate::text::take_chars(line, 120);
    SEVERE.iter().any(|lvl| prefix.contains(lvl))
}

fn is_stack_frame(line: &str) -> bool {
    let trimmed = line.trim_start();
    if line.len() == trimmed.len() {
        // Unindented lines are frames only when they announce one.
        return trimmed.starts_with("Caused by") || trimmed.starts_with("Traceback");
    }
    trimmed.starts_with("at ")
        || trimmed.starts_with("File \"")
        || trimmed.starts_with("Caused by")
        || (trimmed.contains(':') && trimmed.contains(".rs"))
        || (trimmed.contains(':') && trimmed.contains(".py"))
        || (trimmed.contains(':') && trimmed.contains(".js"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_type::TypeMask;
    use crate::policy::Level;

    fn policy() -> Policy<'static> {
        Policy {
            enabled: true,
            min_bytes: 0,
            types: TypeMask::ALL,
            level: Level::Balanced,
            ..Default::default()
        }
    }

    fn noisy(errors_at: &[usize], total: usize) -> String {
        (0..total)
            .map(|i| {
                if errors_at.contains(&i) {
                    format!("2026-01-01T00:00:00Z ERROR failure number {i}\n")
                } else {
                    format!("2026-01-01T00:00:00Z INFO handled request {i}\n")
                }
            })
            .collect()
    }

    #[test]
    fn keeps_errors_from_the_middle_and_drops_info_noise() {
        let input = noisy(&[100], 200);
        let out = compress(&input, &policy()).unwrap();

        assert!(out.len() < input.len());
        assert!(out.contains("failure number 100"), "dropped the error");
        assert!(out.contains("lines elided"));
        assert!(!out.contains("handled request 100"));
    }

    #[test]
    fn keeps_head_and_tail() {
        let input = noisy(&[], 200);
        let out = compress(&input, &policy()).unwrap();
        assert!(out.contains("handled request 0"));
        assert!(out.contains("handled request 199"));
    }

    #[test]
    fn keeps_stack_frames_attached_to_their_error() {
        let mut input = noisy(&[], 100);
        input.push_str("2026-01-01T00:00:00Z ERROR boom\n");
        input.push_str("    at src/main.rs:42\n");
        input.push_str("    at src/lib.rs:7\n");
        input.push_str(&noisy(&[], 100));

        let out = compress(&input, &policy()).unwrap();

        assert!(out.contains("at src/main.rs:42"));
        assert!(out.contains("at src/lib.rs:7"));
    }

    #[test]
    fn short_log_is_left_alone() {
        let input = noisy(&[], 4);
        assert!(compress(&input, &policy()).is_none());
    }

    #[test]
    fn elision_counts_are_accurate() {
        let input = noisy(&[], 200);
        let out = compress(&input, &policy()).unwrap();
        let (head, tail) = Level::Balanced.log_head_tail();
        let expected = 200 - head - tail;
        assert!(
            out.contains(&format!("{expected} lines elided")),
            "wrong count in: {out}"
        );
    }

    #[test]
    fn multibyte_log_lines_do_not_panic() {
        let input: String = (0..200)
            .map(|i| format!("2026-01-01T00:00:00Z INFO naïve · café · {i}\n"))
            .collect();
        assert!(compress(&input, &policy()).is_some());
    }
}
