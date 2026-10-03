//! Level 2.5 — the salience gate.
//!
//! Sits between the decision cache (Level 2) and the classifier (Level 3): it judges
//! whether the latest user query is *substantive* enough to run the classifier and pin the
//! session's model, or is small talk that should be answered cheaply without pinning. See
//! [`super::route_model`].
//!
//! **When it runs.** Only on a cache **miss** at a **fireable boundary** — the exact turns
//! that would otherwise classify. Cache hits (Level 2) and continuation/tool-loop turns
//! never reach it.
//!
//! **What decides.** An in-process classifier ([`super::salience_classifier`]) — a hashed
//! n-gram + logistic-regression model embedded in the binary. The verdict costs a few
//! microseconds of CPU and no network call, so the gate adds no latency to the hot path and
//! has no provider that can fail underneath it.
//!
//! **Fail-safe direction.** The gate distinguishes *two* failure modes that a single
//! boolean conflates:
//!
//! - The classifier is **confident** the turn is small talk ⇒ defer. This is the case the
//!   gate exists for, and the only one that suppresses classification.
//! - The classifier is **uncertain**, or could not run at all (a corrupt or unloadable
//!   weights file) ⇒ proceed to route, exactly as if the gate were not there.
//!
//! Deferring is only safe when it is *earned* by a confident verdict. Deferring on
//! uncertainty instead is self-reinforcing and does not heal: a deferred turn is never
//! pinned, so nothing is cached, so the next turn hits the same unsure gate and defers
//! again — a conversation can be starved indefinitely. Routing on uncertainty degrades to
//! ordinary pre-gate behaviour, which is merely unremarkable.

use std::path::Path;

use async_trait::async_trait;

use super::salience_classifier::{self, Band, Weights};

/// Decides whether a query is substantive enough to classify + pin (proceed to Level 3), or
/// is small talk to be served cheaply without pinning (short-circuit at Level 2.5).
#[async_trait]
pub trait SalienceGate: Send + Sync {
    /// `true` ⇒ substantive (run the classifier); `false` ⇒ small talk (don't pin).
    ///
    /// Implementations MUST return `false` only on a *positive, confident* judgement of
    /// small talk. Anything an implementation cannot decide — an uncertain score, an
    /// unavailable model — MUST return `true`, so an unsure gate never starves a
    /// conversation of routing. See the module docs.
    async fn is_substantive(&self, query: &str) -> bool;
}

/// Always-substantive gate — the router classifies at every fireable boundary, exactly as
/// it did before the gate existed.
///
/// Wired when `SALIENCE_GATE_ENABLED` is false, and as the fallback when a model cannot be
/// loaded: it is the "gate absent" behaviour, which is precisely what the fail-safe
/// direction calls for when nothing can render a confident verdict.
pub struct AllowAllGate;

#[async_trait]
impl SalienceGate for AllowAllGate {
    async fn is_substantive(&self, _query: &str) -> bool {
        true
    }
}

/// The gate: scores the query with the in-process classifier and defers only on a confident
/// small-talk verdict.
///
/// Thresholds come from `SALIENCE_LOW_THRESHOLD` / `SALIENCE_HIGH_THRESHOLD`. Both bands
/// above `low` route, so `high` does not change any routing outcome on its own — it splits
/// the routed turns into "confident" and "uncertain" for logging, which is what makes the
/// uncertain band measurable before anyone tunes `low`.
#[derive(Debug)]
pub struct ClassifierSalienceGate {
    weights: Weights,
    low_threshold: f64,
    high_threshold: f64,
}

impl ClassifierSalienceGate {
    /// Build from the model embedded in this binary — the normal path.
    pub fn embedded(low_threshold: f64, high_threshold: f64) -> Result<(Self, String), String> {
        let model = salience_classifier::embedded_model()?;
        Ok((
            Self::new(model.weights, low_threshold, high_threshold),
            model.trained_at,
        ))
    }

    /// Build from a weights file on disk — the `SALIENCE_WEIGHTS_PATH` override, for trying
    /// a candidate model without a rebuild.
    pub fn from_path(
        path: &str,
        low_threshold: f64,
        high_threshold: f64,
    ) -> Result<(Self, String), String> {
        let model = salience_classifier::load_model_from_file(Path::new(path))?;
        Ok((
            Self::new(model.weights, low_threshold, high_threshold),
            model.trained_at,
        ))
    }

    fn new(weights: Weights, low_threshold: f64, high_threshold: f64) -> Self {
        Self {
            weights,
            low_threshold,
            high_threshold,
        }
    }
}

#[async_trait]
impl SalienceGate for ClassifierSalienceGate {
    async fn is_substantive(&self, query: &str) -> bool {
        let probability = salience_classifier::score(query, &self.weights);
        let band = Band::of(probability, self.low_threshold, self.high_threshold);

        // Only a confident small-talk verdict defers; uncertainty routes. See module docs.
        let substantive = !matches!(band, Band::ConfidentSmallTalk);

        tracing::info!(
            target: "nasiko::llm_router::salience",
            probability,
            band = band.as_str(),
            low_threshold = self.low_threshold,
            high_threshold = self.high_threshold,
            substantive,
            "salience gate: classifier verdict"
        );

        substantive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Thresholds wide enough that the band is chosen by the test, not by the real model's
    /// opinion of the query text.
    const LOW: f64 = 0.20;
    const HIGH: f64 = 0.80;

    fn gate_with(weights: Weights) -> ClassifierSalienceGate {
        ClassifierSalienceGate::new(weights, LOW, HIGH)
    }

    #[tokio::test]
    async fn allow_all_gate_is_always_substantive() {
        assert!(AllowAllGate.is_substantive("hi").await);
        assert!(AllowAllGate.is_substantive("").await);
    }

    #[tokio::test]
    async fn zero_weights_score_one_half_and_therefore_route() {
        // Zero weights score exactly 0.5 — squarely in the uncertain band, which must
        // route rather than defer.
        let gate = gate_with(Weights::zeros());
        assert!(gate.is_substantive("anything at all").await);
    }

    #[tokio::test]
    async fn confident_small_talk_defers() {
        // A large negative bias drives the score below LOW for any input.
        let mut weights = Weights::zeros();
        weights.bias = -20.0;
        let gate = gate_with(weights);
        assert!(!gate.is_substantive("hello there").await);
    }

    #[tokio::test]
    async fn confident_substantive_routes() {
        let mut weights = Weights::zeros();
        weights.bias = 20.0;
        let gate = gate_with(weights);
        assert!(gate.is_substantive("refactor this function").await);
    }

    #[test]
    fn band_splits_on_the_configured_thresholds() {
        assert_eq!(Band::of(0.05, LOW, HIGH), Band::ConfidentSmallTalk);
        assert_eq!(Band::of(0.50, LOW, HIGH), Band::Uncertain);
        assert_eq!(Band::of(0.95, LOW, HIGH), Band::ConfidentSubstantive);
    }

    #[test]
    fn band_boundaries_are_inclusive_toward_routing() {
        // Exactly at a threshold is not "confident" either way, so both land in the
        // uncertain band and therefore route.
        assert_eq!(Band::of(LOW, LOW, HIGH), Band::Uncertain);
        assert_eq!(Band::of(HIGH, LOW, HIGH), Band::Uncertain);
    }

    #[tokio::test]
    async fn embedded_model_loads_and_separates_small_talk_from_tasks() {
        let (gate, trained_at) = ClassifierSalienceGate::embedded(LOW, HIGH)
            .expect("the embedded weights asset must load");
        assert!(!trained_at.is_empty(), "provenance should be populated");

        // The shipped model's actual verdicts, not a synthetic stand-in: a bare greeting
        // must defer, and a concrete task must not.
        assert!(
            !gate.is_substantive("hey there!").await,
            "a bare greeting should be confidently small talk"
        );
        assert!(
            gate.is_substantive("write a python function to parse this CSV")
                .await,
            "a concrete coding request should route"
        );
    }

    #[test]
    fn from_path_rejects_a_missing_file() {
        let err = ClassifierSalienceGate::from_path("/nonexistent/weights.json", LOW, HIGH)
            .expect_err("a missing weights file must be an error, not a silent default");
        assert!(err.contains("failed to read weights file"), "got: {err}");
    }
}
