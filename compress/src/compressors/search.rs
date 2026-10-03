//! Keeps the first and last hits plus anything diagnostic-looking; counts the rest.
//!
//! Off by default (`TypeMask::DEFAULT` excludes it) and that is deliberate: for a coding agent,
//! ripgrep output frequently *is* the answer, and dropping the middle drops the answer. Enabled
//! by config only, once a workload has been shown to tolerate it.

use crate::detect::is_search_hit;
use crate::marker;
use crate::policy::Policy;

const DIAGNOSTIC: [&str; 7] = [
    "error",
    "panic",
    "unsafe",
    "warning",
    "FIXME",
    "TODO",
    "deprecated",
];

pub(super) fn compress(input: &str, policy: &Policy<'_>) -> Option<String> {
    let lines: Vec<&str> = input.lines().collect();
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_search_hit(l))
        .map(|(i, _)| i)
        .collect();

    let (head, tail) = policy.level.search_head_tail();
    if hits.len() <= head + tail + 1 {
        return None;
    }

    let keep_head: Vec<usize> = hits.iter().take(head).copied().collect();
    let keep_tail: Vec<usize> = hits.iter().skip(hits.len() - tail).copied().collect();

    let mut keep = vec![false; lines.len()];
    for i in keep_head.iter().chain(keep_tail.iter()) {
        keep[*i] = true;
    }
    for i in &hits {
        if is_diagnostic(lines[*i]) {
            keep[*i] = true;
        }
    }
    // Anything that is not a hit at all is structure (counts, headers) — keep it.
    for (i, line) in lines.iter().enumerate() {
        if !is_search_hit(line) {
            keep[i] = true;
        }
    }

    let elided: Vec<usize> = hits.iter().copied().filter(|i| !keep[*i]).collect();
    if elided.is_empty() {
        return None;
    }
    let files = distinct_files(&lines, &elided);

    let mut out = String::with_capacity(input.len());
    let mut pending = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if keep[i] {
            if pending > 0 {
                out.push_str(&marker::elided_matches(pending, files, policy.recovery_ref));
                out.push('\n');
                pending = 0;
            }
            out.push_str(line);
            out.push('\n');
        } else {
            pending += 1;
        }
    }
    if pending > 0 {
        out.push_str(&marker::elided_matches(pending, files, policy.recovery_ref));
        out.push('\n');
    }
    Some(out)
}

fn is_diagnostic(line: &str) -> bool {
    let lowered = line.to_ascii_lowercase();
    DIAGNOSTIC
        .iter()
        .any(|t| lowered.contains(&t.to_ascii_lowercase()))
}

fn distinct_files(lines: &[&str], indices: &[usize]) -> usize {
    let mut seen: Vec<&str> = Vec::new();
    for i in indices {
        if let Some(path) = lines[*i].split(':').next()
            && !seen.contains(&path)
        {
            seen.push(path);
        }
    }
    seen.len()
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

    fn hits(n: usize) -> String {
        (0..n)
            .map(|i| format!("src/file{i}.rs:{i}:let value = {i};\n"))
            .collect()
    }

    #[test]
    fn keeps_first_and_last_hits_and_counts_the_middle() {
        let input = hits(100);
        let out = compress(&input, &policy()).unwrap();

        assert!(out.len() < input.len());
        assert!(out.contains("src/file0.rs"));
        assert!(out.contains("src/file99.rs"));
        assert!(out.contains("more matches in"));
    }

    #[test]
    fn keeps_diagnostic_hits_from_the_middle() {
        let mut input = hits(50);
        input.push_str("src/bad.rs:7:panic!(\"boom\");\n");
        input.push_str(&hits(50));

        let out = compress(&input, &policy()).unwrap();

        assert!(out.contains("src/bad.rs:7"), "dropped a diagnostic hit");
    }

    #[test]
    fn short_result_sets_are_left_alone() {
        assert!(compress(&hits(3), &policy()).is_none());
    }

    #[test]
    fn non_hit_lines_survive() {
        let input = format!("Searched 4212 files\n{}", hits(100));
        let out = compress(&input, &policy()).unwrap();
        assert!(out.contains("Searched 4212 files"));
    }

    #[test]
    fn counts_distinct_files_in_the_marker() {
        let input: String = (0..100)
            .map(|i| format!("src/same.rs:{i}:let value = {i};\n"))
            .collect();
        let out = compress(&input, &policy()).unwrap();
        assert!(out.contains("in 1 files"), "wrong file count in: {out}");
    }
}
