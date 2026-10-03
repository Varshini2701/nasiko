//! Prints the fixture corpus as a before/after table, at every level.
//!
//! `cargo run -p nasiko-compress --example report`
//!
//! This is the offline half of the eval harness: it measures bytes, which is all that can be
//! measured before a seam is wired. Token deltas, cost, answer quality and turns-per-flow need a
//! live seam and belong with the ReAct wiring.
//!
//! Rows where compression did not help are printed too, and flagged. Hiding them would make the
//! rows that did help worthless.

use nasiko_compress::{Level, Policy, TypeMask, compress, detect};

/// Read these before quoting any number below.
const CAVEATS: &str = "\
Caveats:\n\
  * Fixtures are synthetic and more uniform than real traffic. Real payloads compress less.\n\
  * The JSON row conflates two effects: re-serializing pretty JSON compactly (-25% on this\n\
    fixture, structural elision contributes the rest). A caller already sending compact JSON\n\
    sees only the second.\n\
  * Bytes are not tokens. A ratio here is an upper bound on the billed saving, not a forecast.\n";

const FIXTURES: [(&str, &str); 6] = [
    (
        "deployment_status.json",
        include_str!("../tests/fixtures/deployment_status.json"),
    ),
    (
        "container.log",
        include_str!("../tests/fixtures/container.log"),
    ),
    (
        "feature.diff",
        include_str!("../tests/fixtures/feature.diff"),
    ),
    (
        "ripgrep_hits.txt",
        include_str!("../tests/fixtures/ripgrep_hits.txt"),
    ),
    (
        "dashboard.html",
        include_str!("../tests/fixtures/dashboard.html"),
    ),
    (
        "incident_summary.md",
        include_str!("../tests/fixtures/incident_summary.md"),
    ),
];

fn main() {
    println!("{CAVEATS}");
    for (label, types) in [
        ("default mask (json,log,diff)", TypeMask::DEFAULT),
        ("all types", TypeMask::ALL),
    ] {
        for level in [Level::Conservative, Level::Balanced, Level::Aggressive] {
            report(label, types, level);
        }
    }
}

fn report(label: &str, types: TypeMask, level: Level) {
    println!("\n## {label} · {}\n", level.as_label());
    println!(
        "| {:<24} | {:>8} | {:>9} | {:>8} | {:>8} | note",
        "fixture", "type", "bytes in", "bytes out", "delta"
    );
    println!(
        "|{:-<26}|{:-<10}|{:-<11}|{:-<10}|{:-<10}|{:-<8}",
        "", "", "", "", "", ""
    );

    let mut total_in = 0usize;
    let mut total_out = 0usize;

    for (name, body) in FIXTURES {
        let policy = Policy {
            enabled: true,
            types,
            level,
            ..Default::default()
        };
        let out = compress(body, &policy);
        total_in += out.original_bytes();
        total_out += out.compressed_bytes();

        let delta = pct(out.original_bytes(), out.compressed_bytes());
        let note = if out.is_changed() {
            ""
        } else if types.contains(detect(body)) {
            "no saving found"
        } else {
            "type not enabled"
        };
        println!(
            "| {:<24} | {:>8} | {:>9} | {:>8} | {:>7.1}% | {}",
            name,
            out.content_type().as_label(),
            out.original_bytes(),
            out.compressed_bytes(),
            delta,
            note
        );
    }

    println!(
        "| {:<24} | {:>8} | {:>9} | {:>8} | {:>7.1}% |",
        "**total**",
        "",
        total_in,
        total_out,
        pct(total_in, total_out)
    );
}

fn pct(before: usize, after: usize) -> f64 {
    if before == 0 {
        return 0.0;
    }
    (after as f64 - before as f64) / before as f64 * 100.0
}
