//! Cache rates inferred from the shape of the price book, for the 81% of models
//! that carry input and output prices but no cache prices.
//!
//! The two answers already in the codebase are both wrong, measurably:
//!
//! * charging cache at the **input rate** — 133 of the 137 priced models in the
//!   book price cache reads at 0.5x input or less, so this is 2x to 10x high in
//!   every observed case;
//! * charging **nothing** — 119 of 137 do charge for cache reads.
//!
//! Cache pricing instead follows vendor conventions that are tight enough to
//! infer from. Anthropic is exact: across every Anthropic row in the book the
//! read ratio is 0.10 and the write ratio 1.25, with `min == max`.

use std::collections::HashMap;

use crate::model::Vendor;

/// A cache read costing more than fresh input is real for a few audio and
/// realtime models, but it is never a safe *inference*: when guessing, guess low.
const MAX_INFERRED_READ_RATIO: f64 = 1.0;

/// Below this many priced rows a ratio says more about one model than about the
/// vendor, so the global default is used instead. DeepSeek and Bedrock each have
/// a single row in the live book.
const MIN_SAMPLES_PER_VENDOR: usize = 3;

/// Cache price as a multiple of the input price: `(read, creation)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheRatio {
    pub read: f64,
    pub creation: f64,
}

/// Per-vendor cache ratios, with a fallback for vendors we have no evidence for.
#[derive(Debug, Clone)]
pub struct CacheRatios {
    by_vendor: HashMap<Vendor, CacheRatio>,
    global: CacheRatio,
}

impl Default for CacheRatios {
    fn default() -> Self {
        Self::measured()
    }
}

impl CacheRatios {
    /// Ratios measured from the live price book on 2026-09-24.
    ///
    /// Anthropic's are exact (`min == max` on every row). OpenAI's are the median
    /// of 116 rows, of which 102 price cache writes at exactly zero — OpenAI does
    /// not charge for writing to cache, so inferring the input rate there invents
    /// a cost that does not exist. The global fallback is the book-wide median.
    pub fn measured() -> Self {
        let by_vendor = [
            (
                Vendor::Anthropic,
                CacheRatio {
                    read: 0.10,
                    creation: 1.25,
                },
            ),
            (
                Vendor::OpenAi,
                CacheRatio {
                    read: 0.10,
                    creation: 0.00,
                },
            ),
            (
                Vendor::DeepSeek,
                CacheRatio {
                    read: 0.10,
                    creation: 0.10,
                },
            ),
        ]
        .into_iter()
        .collect();
        Self {
            by_vendor,
            global: CacheRatio {
                read: 0.10,
                creation: 0.00,
            },
        }
    }

    /// Derive ratios from the rows of a price book that *do* carry cache prices,
    /// so the inference sharpens as the pricing sync widens its coverage rather
    /// than rotting in a constant table.
    ///
    /// Medians, not means: the handful of realtime models that price cache reads
    /// above input would drag a mean badly (the observed OpenAI mean is 0.35
    /// against a median of 0.10). A vendor with fewer than
    /// [`MIN_SAMPLES_PER_VENDOR`] rows keeps the measured default.
    pub fn from_samples(samples: impl IntoIterator<Item = CacheRatioSample>) -> Self {
        let mut observed: HashMap<Vendor, (Vec<f64>, Vec<f64>)> = HashMap::new();
        for sample in samples {
            let Some(ratio) = sample.as_ratio() else {
                continue;
            };
            let entry = observed.entry(sample.vendor).or_default();
            entry.0.push(ratio.read);
            entry.1.push(ratio.creation);
        }

        let mut ratios = Self::measured();
        for (vendor, (mut reads, mut creations)) in observed {
            if reads.len() < MIN_SAMPLES_PER_VENDOR {
                continue;
            }
            ratios.by_vendor.insert(
                vendor,
                CacheRatio {
                    read: median(&mut reads),
                    creation: median(&mut creations),
                },
            );
        }
        ratios
    }

    /// Cache rates for a model, given the input rate its price row carries.
    ///
    /// The read rate is clamped to the input rate: an inferred cache read that
    /// costs more than fresh input would be a guess in the expensive direction.
    pub fn rates_for(&self, vendor: Vendor, input_per_1m: f64) -> (f64, f64) {
        let ratio = self.by_vendor.get(&vendor).copied().unwrap_or(self.global);
        let read = (input_per_1m * ratio.read).min(input_per_1m * MAX_INFERRED_READ_RATIO);
        (read, input_per_1m * ratio.creation)
    }
}

/// One priced row, as evidence for [`CacheRatios::from_samples`].
#[derive(Debug, Clone, Copy)]
pub struct CacheRatioSample {
    pub vendor: Vendor,
    pub input_per_1m: f64,
    pub cache_read_per_1m: f64,
    pub cache_creation_per_1m: f64,
}

impl CacheRatioSample {
    fn as_ratio(&self) -> Option<CacheRatio> {
        (self.input_per_1m > 0.0).then_some(CacheRatio {
            read: self.cache_read_per_1m / self.input_per_1m,
            creation: self.cache_creation_per_1m / self.input_per_1m,
        })
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(vendor: Vendor, input: f64, read: f64, creation: f64) -> CacheRatioSample {
        CacheRatioSample {
            vendor,
            input_per_1m: input,
            cache_read_per_1m: read,
            cache_creation_per_1m: creation,
        }
    }

    #[test]
    fn anthropic_inference_reproduces_the_published_rate_exactly() {
        // claude-opus-5: input $15.00, true cache read $1.50, true write $18.75.
        let (read, creation) = CacheRatios::measured().rates_for(Vendor::Anthropic, 15.00);
        assert!((read - 1.50).abs() < 1e-9, "read was {read}");
        assert!((creation - 18.75).abs() < 1e-9, "creation was {creation}");
    }

    #[test]
    fn inference_never_charges_cache_at_the_input_rate() {
        // The live Rust bug: `unwrap_or(in_p)`. On the 39.5M cache-read tokens in
        // trace_usage that was $593 against a true $59.
        for vendor in [
            Vendor::Anthropic,
            Vendor::OpenAi,
            Vendor::DeepSeek,
            Vendor::Google,
            Vendor::Unknown,
        ] {
            let (read, _) = CacheRatios::measured().rates_for(vendor, 15.00);
            assert!(
                read < 15.00,
                "{vendor:?} priced cache reads at the input rate"
            );
        }
    }

    #[test]
    fn inference_never_charges_cache_reads_at_zero() {
        // The live SQL bug: cache cost skipped entirely.
        for vendor in [Vendor::Anthropic, Vendor::OpenAi, Vendor::Unknown] {
            let (read, _) = CacheRatios::measured().rates_for(vendor, 15.00);
            assert!(read > 0.0, "{vendor:?} priced cache reads at zero");
        }
    }

    #[test]
    fn openai_cache_writes_are_free_because_openai_does_not_charge_for_them() {
        let (_, creation) = CacheRatios::measured().rates_for(Vendor::OpenAi, 2.50);
        assert_eq!(creation, 0.0);
    }

    #[test]
    fn an_unknown_vendor_falls_back_to_the_book_wide_median() {
        let (read, creation) = CacheRatios::measured().rates_for(Vendor::Unknown, 10.00);
        assert!((read - 1.00).abs() < 1e-9);
        assert_eq!(creation, 0.0);
    }

    #[test]
    fn derived_ratios_use_the_median_so_outliers_do_not_drag_them() {
        // Four ordinary rows at 0.10 plus one realtime model at 4.0: the mean
        // would be 0.88, the median stays 0.10.
        let samples = vec![
            sample(Vendor::OpenAi, 10.0, 1.0, 0.0),
            sample(Vendor::OpenAi, 10.0, 1.0, 0.0),
            sample(Vendor::OpenAi, 10.0, 1.0, 0.0),
            sample(Vendor::OpenAi, 10.0, 1.0, 0.0),
            sample(Vendor::OpenAi, 10.0, 40.0, 0.0),
        ];
        let (read, _) = CacheRatios::from_samples(samples).rates_for(Vendor::OpenAi, 10.0);
        assert!((read - 1.0).abs() < 1e-9, "read was {read}");
    }

    #[test]
    fn a_vendor_with_too_few_rows_keeps_the_measured_default() {
        // One row must not redefine a vendor: the live book has exactly one
        // DeepSeek and one Bedrock row.
        let samples = vec![sample(Vendor::Anthropic, 10.0, 9.0, 9.0)];
        let (read, _) = CacheRatios::from_samples(samples).rates_for(Vendor::Anthropic, 10.0);
        assert!((read - 1.0).abs() < 1e-9, "one row overrode the default");
    }

    #[test]
    fn an_inferred_read_rate_is_clamped_to_the_input_rate() {
        let samples = vec![
            sample(Vendor::Google, 10.0, 50.0, 0.0),
            sample(Vendor::Google, 10.0, 50.0, 0.0),
            sample(Vendor::Google, 10.0, 50.0, 0.0),
        ];
        let (read, _) = CacheRatios::from_samples(samples).rates_for(Vendor::Google, 10.0);
        assert!((read - 10.0).abs() < 1e-9, "clamp did not hold: {read}");
    }

    #[test]
    fn rows_without_an_input_price_are_ignored_rather_than_dividing_by_zero() {
        let samples = vec![
            sample(Vendor::Google, 0.0, 1.0, 1.0),
            sample(Vendor::Google, 0.0, 1.0, 1.0),
            sample(Vendor::Google, 0.0, 1.0, 1.0),
        ];
        let (read, _) = CacheRatios::from_samples(samples).rates_for(Vendor::Google, 10.0);
        assert!(read.is_finite());
        assert!((read - 1.0).abs() < 1e-9);
    }
}
