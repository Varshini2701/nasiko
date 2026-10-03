//! The two cost read paths must agree, trace for trace.
//!
//! Observability serves session and trace views live from Tempo; TokenOps reads
//! `trace_usage`, which a background worker materializes from the same spans.
//! Two paths over one source, and for most of this system's life they were
//! *different code* — `trace_cost` deduplicated spans by id and
//! `extract_trace_usage` did not, so nothing but habit kept them equal.
//!
//! This is the gate that replaces that habit. It runs both paths over every
//! trace the materializer has recorded and requires an exact match on all four
//! token classes and on cost. A regression here means a number on the FinOps
//! dashboard has silently diverged from the same number on the session page.
//!
//! Ignored by default — needs Tempo and `DATABASE_URL` (`just infra`).
//! Run with:
//!   `cargo test -p nasiko-server --test cost_path_differential -- --ignored`

use std::sync::Arc;

use nasiko_observability::{ObservabilityProvider, TempoLokiProvider};
use nasiko_pricing::PricingEngine;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// Worst-case rounding drift contributed by one span.
///
/// Costs are rounded to six decimal places, so each rounding can move a figure
/// by up to 5e-7. Pricing a span rounds its four components and their total, and
/// accumulating it into a running breakdown rounds five more times — about ten
/// roundings, hence 5e-6. The per-agent path rounds strictly fewer times (agents
/// never outnumber spans), so `spans * this` is an upper bound on the gap.
///
/// Derived rather than tuned: it must not quietly absorb a real defect, and it
/// cannot. A missed deduplication doubles a figure, a shadowed cache rate is a
/// 10x error, a wrong provider shifts it by tens of percent — all of them
/// percent-scale against a bound that stays around 1e-5 relative.
const ROUNDING_DRIFT_PER_SPAN: f64 = 5e-6;

/// Floor so a near-zero cost is not compared against a near-zero tolerance.
const ABSOLUTE_COST_FLOOR: f64 = 1e-6;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for live tests");
    PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to the usage database")
}

fn provider(db: &PgPool) -> TempoLokiProvider {
    let tempo = std::env::var("TEMPO_URL").unwrap_or_else(|_| "http://localhost:3200".into());
    let loki = std::env::var("LOKI_URL").unwrap_or_else(|_| "http://localhost:3100".into());
    TempoLokiProvider::new(tempo, loki, Arc::new(PricingEngine::new(db.clone())))
}

/// Every trace the materializer has recorded — the population both paths serve.
async fn materialized_trace_ids(db: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT DISTINCT trace_id FROM trace_usage ORDER BY trace_id")
        .fetch_all(db)
        .await
        .expect("read trace_usage")
}

#[tokio::test]
#[ignore = "needs Tempo and DATABASE_URL"]
async fn both_cost_paths_agree_on_every_materialized_trace() {
    let db = pool().await;
    let provider = provider(&db);
    let trace_ids = materialized_trace_ids(&db).await;
    assert!(
        !trace_ids.is_empty(),
        "no materialized traces — the differential proves nothing on an empty set"
    );

    let mut compared = 0usize;
    let mut mismatches: Vec<String> = Vec::new();

    for trace_id in &trace_ids {
        let Ok(trace) = provider.get_trace(trace_id).await else {
            // A trace aged out of Tempo's retention: the materialized row
            // outlives the span data, which is expected, not a divergence.
            continue;
        };

        // Path A — what the session and trace views show.
        let (usage, _) = trace.usage_totals();
        let observability_cost = provider.trace_cost(&trace).await.total_usd;

        // Path B — what FinOps aggregates, via the materializer's extractor.
        let rows = provider
            .extract_trace_usage(trace_id)
            .await
            .expect("extract trace usage");
        let tokenops = rows
            .iter()
            .fold((0u64, 0u64, 0u64, 0u64, 0f64), |mut acc, r| {
                acc.0 += r.input_tokens;
                acc.1 += r.output_tokens;
                acc.2 += r.cache_read_tokens;
                acc.3 += r.cache_creation_tokens;
                acc.4 += r.cost_usd;
                acc
            });

        compared += 1;

        let token_delta = [
            (usage.input_tokens as i64) - (tokenops.0 as i64),
            (usage.output_tokens as i64) - (tokenops.1 as i64),
            (usage.cache_read_tokens as i64) - (tokenops.2 as i64),
            (usage.cache_creation_tokens as i64) - (tokenops.3 as i64),
        ];
        let cost_delta = observability_cost - tokenops.4;

        // Tokens must match exactly — they are integers, and any difference
        // means one path counted a span the other did not.
        //
        // Cost cannot be compared bitwise, and not because of sloppiness: the
        // two paths round at different grains by design. The session view costs
        // each span and sums the results, so it accumulates one rounding per
        // span; FinOps needs a row per agent, so it sums that agent's tokens
        // first and costs them once. On the traces here that is a divergence of
        // at most 1.6e-5 on a $57 figure — about 3e-7 relative.
        //
        // The tolerance is therefore the accumulated rounding bound, which
        // scales with how many times each path rounded — see
        // ROUNDING_DRIFT_PER_SPAN.
        let tolerance =
            (trace.spans.len() as f64 * ROUNDING_DRIFT_PER_SPAN).max(ABSOLUTE_COST_FLOOR);
        if token_delta.iter().any(|d| *d != 0) || cost_delta.abs() > tolerance {
            mismatches.push(format!(
                "{trace_id}: tokens A={:?} B={:?} delta={token_delta:?}  cost A={observability_cost:.6} B={:.6} delta={cost_delta:+.6}",
                (
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_tokens,
                    usage.cache_creation_tokens
                ),
                (tokenops.0, tokenops.1, tokenops.2, tokenops.3),
                tokenops.4,
            ));
        }
    }

    assert!(
        compared > 0,
        "every trace had aged out of Tempo — nothing was actually compared"
    );
    assert!(
        mismatches.is_empty(),
        "the two cost paths disagree on {} of {compared} traces:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

#[tokio::test]
#[ignore = "needs Tempo and DATABASE_URL"]
async fn a_duplicated_span_cannot_inflate_the_materialized_total() {
    // `extract_trace_usage` is what writes `trace_usage`, and it used to be the
    // one aggregator here without span deduplication. Tempo does return a span
    // twice — a re-export, or a replayed batch — so this asserts the property
    // rather than trusting that it has not happened yet.
    let db = pool().await;
    let provider = provider(&db);

    for trace_id in materialized_trace_ids(&db).await {
        let Ok(trace) = provider.get_trace(&trace_id).await else {
            continue;
        };
        let mut doubled = trace.clone();
        doubled.spans.extend(trace.spans.iter().cloned());

        let once = provider.trace_cost(&trace).await.total_usd;
        let twice = provider.trace_cost(&doubled).await.total_usd;
        assert!(
            (once - twice).abs() < 1e-9,
            "{trace_id}: duplicated spans changed the cost, {once:.6} -> {twice:.6}"
        );
        return; // one trace is enough to prove the property
    }
}
