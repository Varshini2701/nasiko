//! Keeps file headers, hunk headers and every changed line; collapses context beyond ±N.
//!
//! Never drops a `+`/`-` line — those are the diff. Context exists to orient the reader, and a
//! model orients from the hunk header just as well.

use crate::marker;
use crate::policy::Policy;

pub(super) fn compress(input: &str, policy: &Policy<'_>) -> Option<String> {
    let lines: Vec<&str> = input.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let context = policy.level.diff_context();

    let mut keep = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        if is_structural(line) || is_change(line) {
            keep[i] = true;
        }
    }

    // Widen around each change so the model sees where it landed.
    let changed: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_change(l))
        .map(|(i, _)| i)
        .collect();
    for i in changed {
        let lo = i.saturating_sub(context);
        let hi = (i + context + 1).min(lines.len());
        for slot in keep.iter_mut().take(hi).skip(lo) {
            *slot = true;
        }
    }

    if keep.iter().all(|k| *k) {
        return None;
    }

    let mut out = String::with_capacity(input.len());
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
    Some(out)
}

fn is_structural(line: &str) -> bool {
    line.starts_with("diff --git")
        || line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("@@")
        || line.starts_with("new file mode")
        || line.starts_with("deleted file mode")
        || line.starts_with("rename ")
        || line.starts_with("similarity index")
}

fn is_change(line: &str) -> bool {
    (line.starts_with('+') || line.starts_with('-')) && !is_structural(line)
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

    fn wide_diff() -> String {
        let mut s = String::from(
            "diff --git a/x.rs b/x.rs\nindex abc..def 100644\n--- a/x.rs\n+++ b/x.rs\n@@ -1,60 +1,60 @@\n",
        );
        for i in 0..30 {
            s.push_str(&format!(" context line {i}\n"));
        }
        s.push_str("-removed line\n+added line\n");
        for i in 30..60 {
            s.push_str(&format!(" context line {i}\n"));
        }
        s
    }

    #[test]
    fn keeps_every_changed_line_and_all_headers() {
        let input = wide_diff();
        let out = compress(&input, &policy()).unwrap();

        assert!(out.contains("-removed line"));
        assert!(out.contains("+added line"));
        assert!(out.contains("diff --git a/x.rs b/x.rs"));
        assert!(out.contains("@@ -1,60 +1,60 @@"));
        assert!(out.contains("index abc..def 100644"));
    }

    #[test]
    fn collapses_context_far_from_a_change() {
        let input = wide_diff();
        let out = compress(&input, &policy()).unwrap();

        assert!(out.len() < input.len());
        assert!(out.contains("lines elided"));
        assert!(!out.contains("context line 0"), "distant context survived");
    }

    #[test]
    fn keeps_context_adjacent_to_a_change() {
        let input = wide_diff();
        let out = compress(&input, &policy()).unwrap();
        assert!(out.contains("context line 29"), "adjacent context dropped");
        assert!(out.contains("context line 30"), "adjacent context dropped");
    }

    #[test]
    fn a_diff_that_is_all_changes_is_left_alone() {
        let input = "diff --git a/x b/x\n@@ -1,2 +1,2 @@\n-a\n+b\n";
        assert!(compress(input, &policy()).is_none());
    }

    #[test]
    fn multibyte_context_does_not_panic() {
        let mut input = String::from("diff --git a/x b/x\n@@ -1,40 +1,40 @@\n");
        for i in 0..40 {
            input.push_str(&format!(" café · naïve {i}\n"));
        }
        input.push_str("-før\n+etter\n");
        assert!(compress(&input, &policy()).is_some());
    }
}
