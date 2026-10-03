//! Token normalization: reducing whatever a producer reported to four token
//! classes whose meanings are fixed.
//!
//! The invariant every caller depends on: [`NormalizedUsage::input`] is the
//! *fresh* prompt, with cached tokens excluded. Costing charges `input` at the
//! full rate and the cache classes at their own rates and sums them, so an
//! `input` that still contains the cached tokens bills them twice.

/// Token counts exactly as a producer reported them, before normalization.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawUsage {
    /// Whatever the producer calls the prompt count. May or may not include the
    /// cached tokens — that is what [`PromptConvention`] settles.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    /// The producer's own total, when it reports one. Settles an ambiguous
    /// convention outright.
    pub total: Option<u64>,
}

/// What a producer's prompt count means, as declared by the producer.
///
/// Providers genuinely disagree, and guessing is only correct until it isn't:
///
/// * OpenAI and the GenAI semconv report the **whole** prompt with the cached
///   tokens as a subset of it.
/// * Anthropic reports the prompt **excluding** cache reads, as a disjoint count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptConvention {
    /// `input` already excludes cached tokens — Anthropic, coding-agent feeds.
    Exclusive,
    /// `input` includes cached tokens — OpenAI, Gemini.
    Inclusive,
    /// Unknown, as for a third-party OTel span. Falls back to [`infer_fresh_prompt`].
    Infer,
}

/// Four token classes with fixed meanings. `input` is always cache-exclusive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NormalizedUsage {
    /// Prompt tokens billed at the full input rate. Cached tokens are **not** here.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

impl NormalizedUsage {
    /// Every prompt token, cached or not — what an OpenAI response calls
    /// `prompt_tokens` and what a reader means by "input tokens".
    pub fn total_prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_creation
    }

    /// Prompt plus completion. The headline figure for a turn.
    pub fn total(&self) -> u64 {
        self.total_prompt() + self.output
    }

    pub fn is_empty(&self) -> bool {
        self.input == 0 && self.output == 0 && self.cache_read == 0 && self.cache_creation == 0
    }
}

/// Normalize a producer's counts into the four classes.
pub fn normalize_usage(raw: RawUsage, convention: PromptConvention) -> NormalizedUsage {
    let cached = raw.cache_read.saturating_add(raw.cache_creation);
    let input = match convention {
        PromptConvention::Exclusive => raw.input,
        PromptConvention::Inclusive => raw.input.saturating_sub(cached),
        PromptConvention::Infer => infer_fresh_prompt(raw.input, raw.output, cached, raw.total),
    };
    NormalizedUsage {
        input,
        output: raw.output,
        cache_read: raw.cache_read,
        cache_creation: raw.cache_creation,
    }
}

/// Best-effort fresh-prompt count when the convention is unknown.
///
/// Used only for spans we did not emit. Where the evidence is ambiguous this
/// errs toward the inclusive reading, which under-charges at worst — guessing
/// the other way double-charges every cached token.
pub fn infer_fresh_prompt(input: u64, output: u64, cached: u64, total: Option<u64>) -> u64 {
    if cached == 0 {
        return input;
    }
    // A reported total settles it outright when it adds up one way and not the other.
    if let Some(total) = total {
        if total == input.saturating_add(output) {
            return input.saturating_sub(cached); // inclusive
        }
        if total == input.saturating_add(cached).saturating_add(output) {
            return input; // exclusive — already the fresh count
        }
    }
    // No usable total. An `input` smaller than the cached subset cannot contain
    // it, so that reading must be disjoint; otherwise take the semconv reading.
    if input >= cached {
        input - cached
    } else {
        input
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(input: u64, output: u64, cache_read: u64, cache_creation: u64) -> RawUsage {
        RawUsage {
            input,
            output,
            cache_read,
            cache_creation,
            total: None,
        }
    }

    #[test]
    fn an_exclusive_producer_is_taken_at_its_word() {
        // Real Claude Code turn: 848 fresh against 187M cached. Subtracting here
        // would zero a real count.
        let u = normalize_usage(
            raw(848, 160_410, 187_089_147, 2_965_780),
            PromptConvention::Exclusive,
        );
        assert_eq!(u.input, 848);
        assert_eq!(u.total_prompt(), 190_055_775);
    }

    #[test]
    fn an_inclusive_producer_has_the_cached_subset_removed() {
        // OpenAI: prompt_tokens 4732 of which 3968 were cache reads.
        let u = normalize_usage(raw(4732, 100, 3968, 0), PromptConvention::Inclusive);
        assert_eq!(u.input, 764);
        // The whole prompt is preserved — only its split changed.
        assert_eq!(u.total_prompt(), 4732);
    }

    #[test]
    fn an_inclusive_producer_reporting_less_than_it_cached_saturates() {
        let u = normalize_usage(raw(10, 5, 4000, 0), PromptConvention::Inclusive);
        assert_eq!(u.input, 0);
    }

    #[test]
    fn inference_uses_a_total_that_adds_up_inclusively() {
        let mut r = raw(100, 20, 40, 0);
        r.total = Some(120); // input + output => input already contains the cache
        assert_eq!(normalize_usage(r, PromptConvention::Infer).input, 60);
    }

    #[test]
    fn inference_uses_a_total_that_adds_up_exclusively() {
        let mut r = raw(100, 20, 40, 0);
        r.total = Some(160); // input + cached + output => input is already fresh
        assert_eq!(normalize_usage(r, PromptConvention::Infer).input, 100);
    }

    #[test]
    fn inference_reads_a_prompt_smaller_than_its_cache_as_disjoint() {
        // The Anthropic shape: a handful of fresh tokens against a huge cache.
        // Measured across 672 real cached calls, this branch was correct every time.
        assert_eq!(
            normalize_usage(raw(4, 100, 20_000, 1_000), PromptConvention::Infer).input,
            4
        );
    }

    #[test]
    fn inference_falls_back_to_the_semconv_reading() {
        assert_eq!(
            normalize_usage(raw(100, 10, 40, 0), PromptConvention::Infer).input,
            60
        );
    }

    #[test]
    fn a_call_with_no_cache_is_unchanged_under_every_convention() {
        for convention in [
            PromptConvention::Exclusive,
            PromptConvention::Inclusive,
            PromptConvention::Infer,
        ] {
            assert_eq!(normalize_usage(raw(500, 100, 0, 0), convention).input, 500);
        }
    }

    #[test]
    fn a_turn_served_entirely_from_cache_is_not_empty() {
        let u = normalize_usage(raw(0, 0, 5_000, 0), PromptConvention::Exclusive);
        assert!(!u.is_empty());
        assert_eq!(u.total_prompt(), 5_000);
    }
}
