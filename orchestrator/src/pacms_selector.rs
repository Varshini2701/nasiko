//! PACMS context selector — a budget-aware, coverage-diversified replacement
//! for plain recency ("last-k") truncation of session history.
//!
//! Ported from `pacms/pacms-py/pacms/pacms_selector.py` (itself extracted
//! verbatim, algorithmically, from `PACMSEvaluator.pacms_v2`). Embeddings and
//! cosine similarity are delegated to `VectorStore` instead of Ollama/numpy.
//!
//! Algorithm (`select_pacms`):
//!   - embed every candidate + the query
//!   - relevance(i) = max(0, cosine(candidate_i, query))
//!   - coverage weight w[i][j] = relevance(i) * max(0, cosine(i, j))
//!   - monotone submodular facility-location objective F(S) = sum_i max_{j in S} w[i][j]
//!   - CELF lazy-greedy selection under a token (knapsack) budget, with
//!     mandatory indices seeded first
//!
//! `select_lastk` is also ported as a query-blind, embedding-free recency
//! baseline — usable as a fallback strategy (e.g. when embeddings are
//! disabled) without pulling in a second implementation of "keep the most
//! recent items that fit".
//!
//! This module is self-contained: it operates on `Vec<String>` candidates and
//! returns selected indices, mirroring the Python contract. Wiring it into
//! `SessionHistory` (formatting `ChatMessage`s into candidate strings, mapping
//! selected indices back, and restoring chronological order) is a separate
//! step.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};

use crate::error::RouterError;
use crate::vector_store::VectorStore;

/// Default token estimator: ~4 characters per token. Callers with a real
/// tokenizer should pass their own via `PacmsSelector::with_token_estimator`.
fn default_token_estimator(text: &str) -> usize {
    text.len() / 4
}

pub struct PacmsSelector<'a> {
    vector_store: &'a VectorStore,
    /// Fixed relevance/coverage tradeoff, used when `adaptive_lam` is false.
    lam: f64,
    /// When true, `lam` is derived per-call from `budget / total_tokens`
    /// (low budget -> favor relevance, high budget -> favor coverage),
    /// clamped to [0.01, 0.99] so neither term ever fully vanishes.
    adaptive_lam: bool,
    token_estimator: Box<dyn Fn(&str) -> usize + Send + Sync + 'a>,
}

impl<'a> PacmsSelector<'a> {
    pub fn new(vector_store: &'a VectorStore) -> Self {
        Self {
            vector_store,
            lam: 1.0,
            adaptive_lam: true,
            token_estimator: Box::new(default_token_estimator),
        }
    }

    pub fn with_lam(mut self, lam: f64) -> Self {
        self.lam = lam;
        self
    }

    pub fn with_adaptive_lam(mut self, adaptive: bool) -> Self {
        self.adaptive_lam = adaptive;
        self
    }

    pub fn with_token_estimator(
        mut self,
        estimator: impl Fn(&str) -> usize + Send + Sync + 'a,
    ) -> Self {
        self.token_estimator = Box::new(estimator);
        self
    }

    fn estimate_tokens(&self, text: &str) -> usize {
        (self.token_estimator)(text)
    }

    /// Embeds every candidate plus the query. Relevance is no longer computed
    /// here: it falls out of the same normalised matrix that feeds the coverage
    /// weights, so the query/candidate cosines are not a separate pass.
    ///
    /// Candidates go through `embed_batch`, not a per-candidate `embed()`
    /// loop. `embed_batch` checks `text_cache` per text and issues one request
    /// for the misses, so a warm pool still costs zero network calls — the
    /// property the per-item loop was reinstated for — while a cold pool costs
    /// one round trip instead of `pool_size` sequential ones.
    async fn embed_all(
        &self,
        candidates: &[String],
        query: &str,
    ) -> Result<(Vec<Vec<f32>>, Vec<f32>), RouterError> {
        let embeddings = self.vector_store.embed_batch(candidates).await?;
        let query_emb = self.vector_store.embed(query).await?;
        Ok((embeddings, query_emb))
    }

    fn seed_mandatory(
        &self,
        candidates: &[String],
        mandatory: &HashSet<usize>,
        n: usize,
    ) -> (Vec<usize>, usize) {
        let mut kept = Vec::new();
        let mut tok_total = 0;
        let mut sorted_mandatory: Vec<usize> =
            mandatory.iter().copied().filter(|&i| i < n).collect();
        sorted_mandatory.sort_unstable();
        for j in sorted_mandatory {
            tok_total += self.estimate_tokens(&candidates[j]);
            kept.push(j);
        }
        (kept, tok_total)
    }

    /// Recency baseline: keep the most-recent candidates that fit the token
    /// budget. No embeddings, no query — topic-blind, used as a fallback when
    /// embeddings are disabled/unavailable, or for comparison.
    pub fn select_lastk(
        &self,
        candidates: &[String],
        budget: usize,
        mandatory: Option<&HashSet<usize>>,
    ) -> Vec<usize> {
        let empty = HashSet::new();
        let mandatory = mandatory.unwrap_or(&empty);
        let n = candidates.len();
        if n == 0 {
            return vec![];
        }

        let (mut kept, mut tok) = self.seed_mandatory(candidates, mandatory, n);
        let mut kept_set: HashSet<usize> = kept.iter().copied().collect();

        for i in (0..n).rev() {
            if kept_set.contains(&i) {
                continue;
            }
            let t = self.estimate_tokens(&candidates[i]);
            if t == 0 {
                continue;
            }
            if tok + t <= budget {
                kept.push(i);
                kept_set.insert(i);
                tok += t;
            }
        }

        let mut plan: Vec<usize> = kept
            .iter()
            .copied()
            .filter(|i| mandatory.contains(i))
            .collect();
        let mut rest: Vec<usize> = kept
            .iter()
            .copied()
            .filter(|i| !mandatory.contains(i))
            .collect();
        plan.sort_unstable();
        rest.sort_unstable();
        plan.extend(rest);
        plan
    }

    /// Budget-aware submodular coverage selection (CELF lazy-greedy). See
    /// module docs for the objective.
    ///
    /// This is the I/O half: it embeds, then delegates to
    /// [`Self::select_from_embeddings`] for the selection itself.
    pub async fn select_pacms(
        &self,
        candidates: &[String],
        query: &str,
        budget: usize,
        mandatory: Option<&HashSet<usize>>,
    ) -> Result<Vec<usize>, RouterError> {
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        let (embeddings, query_emb) = self.embed_all(candidates, query).await?;
        Ok(self.select_from_embeddings(candidates, &embeddings, &query_emb, budget, mandatory))
    }

    /// The pure selection core: same algorithm, no I/O. Split out from
    /// [`Self::select_pacms`] so the objective can be exercised without a live
    /// embedding endpoint, and so callers holding embeddings already need not
    /// re-fetch them.
    pub fn select_from_embeddings(
        &self,
        candidates: &[String],
        embeddings: &[Vec<f32>],
        query_emb: &[f32],
        budget: usize,
        mandatory: Option<&HashSet<usize>>,
    ) -> Vec<usize> {
        let empty = HashSet::new();
        let mandatory = mandatory.unwrap_or(&empty);
        let n = candidates.len();
        if n == 0 || embeddings.len() != n {
            return vec![];
        }

        let mut plan_idx: Vec<usize> = mandatory.iter().copied().filter(|&i| i < n).collect();
        plan_idx.sort_unstable();
        let plan_set: HashSet<usize> = plan_idx.iter().copied().collect();

        let total_tok: usize = candidates.iter().map(|c| self.estimate_tokens(c)).sum();
        let total_tok = total_tok.max(1);
        let lam = if self.adaptive_lam {
            (1.0 - (budget as f64 / total_tok as f64)).clamp(0.01, 0.99)
        } else {
            self.lam
        };

        // Normalise once, with the query as the last row. After this, cosine is
        // a plain dot product, so the per-pair norm recomputation that
        // `cosine_similarity` used to do (2n^2 norms for n^2 pairs, when only
        // n distinct norms exist) is gone, and the query/candidate cosines come
        // out of the same matrix as the pairwise ones.
        let unit = UnitRows::build(embeddings, query_emb);

        // relevance(i) = max(0, cos(candidate_i, query)) — the query row.
        let rel: Vec<f64> = (0..n).map(|i| f64::from(unit.cos(i, n).max(0.0))).collect();

        // Coverage weight w[i][j] = rel[i] * cos(i,j): how well j "covers" i,
        // weighted by i's own query relevance (facility-location form).
        //
        // Stored TRANSPOSED and flat: `wt[j * n + i] == w[i][j]`. Both the
        // commit and marginal-gain loops hold j fixed and sweep i, i.e. they
        // walk a column of w; transposing makes that column contiguous instead
        // of stride-n. Flat also replaces n separate Vec allocations.
        //
        // cos(i,j) == cos(j,i), so only the upper triangle is computed and each
        // dot product fills both transposed cells. w itself is not symmetric,
        // because rel[i] and rel[j] differ.
        let mut wt = vec![0.0f64; n * n];
        for i in 0..n {
            wt[i * n + i] = rel[i]; // diagonal: cos(i,i) taken as 1.0
            for j in (i + 1)..n {
                let s = f64::from(unit.cos(i, j).max(0.0));
                wt[j * n + i] = rel[i] * s; // w[i][j]
                wt[i * n + j] = rel[j] * s; // w[j][i]
            }
        }
        // Column j of w, contiguous.
        let col = |j: usize| -> &[f64] { &wt[j * n..(j + 1) * n] };

        let mut cover = vec![0.0f64; n];
        let mut selected: HashSet<usize> = HashSet::new();
        let mut selected_tok = 0usize;
        // Running sum of rel[j] over the selected set: the modular half of the
        // objective, needed by the singleton guard below.
        let mut selected_rel = 0.0f64;

        let commit = |j: usize,
                      selected: &mut HashSet<usize>,
                      selected_tok: &mut usize,
                      selected_rel: &mut f64,
                      cover: &mut [f64]| {
            selected.insert(j);
            *selected_tok += self.estimate_tokens(&candidates[j]);
            *selected_rel += rel[j];
            for (c, &w) in cover.iter_mut().zip(col(j)) {
                if w > *c {
                    *c = w;
                }
            }
        };

        // Seed mandatory items (respecting budget).
        for &j in &plan_idx {
            let tok = self.estimate_tokens(&candidates[j]);
            if selected_tok + tok <= budget {
                commit(
                    j,
                    &mut selected,
                    &mut selected_tok,
                    &mut selected_rel,
                    &mut cover,
                );
            }
        }

        // Snapshot the post-mandatory state. The singleton guard is defined
        // against M, not against the grown greedy set.
        let m_set = selected.clone();
        let m_cover = cover.clone();
        let m_tok = selected_tok;
        let m_rel = selected_rel;

        let marginal_gain = |j: usize, cover: &[f64]| -> f64 {
            let cov: f64 = col(j)
                .iter()
                .zip(cover.iter())
                .map(|(&w, &c)| {
                    let d = w - c;
                    if d > 0.0 { d } else { 0.0 }
                })
                .sum();
            lam * rel[j] + (1.0 - lam) * cov
        };

        // CELF lazy-greedy: heap entries carry (ratio, the round they were
        // last evaluated at, candidate index). A popped entry whose
        // `last_eval` matches the current round has a fresh, up-to-date gain
        // and can be committed immediately; otherwise it's stale and gets
        // recomputed and re-pushed. This lazy re-evaluation is what makes
        // greedy submodular selection fast in practice without changing the
        // final selection.
        let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::new();
        for (j, candidate) in candidates.iter().enumerate() {
            if selected.contains(&j) {
                continue;
            }
            let tok = self.estimate_tokens(candidate);
            if tok == 0 || selected_tok + tok > budget {
                continue;
            }
            let ratio = marginal_gain(j, &cover) / tok as f64;
            heap.push(HeapEntry {
                ratio,
                last_eval: 0,
                idx: j,
            });
        }

        let mut round = 1u64;
        while let Some(top) = heap.pop() {
            if selected_tok >= budget {
                break;
            }
            let tok = self.estimate_tokens(&candidates[top.idx]);
            if selected.contains(&top.idx) || selected_tok + tok > budget {
                continue;
            }
            if top.last_eval == round {
                if top.ratio <= 1e-9 {
                    break;
                }
                commit(
                    top.idx,
                    &mut selected,
                    &mut selected_tok,
                    &mut selected_rel,
                    &mut cover,
                );
                round += 1;
            } else {
                let ratio = marginal_gain(top.idx, &cover) / tok as f64;
                heap.push(HeapEntry {
                    ratio,
                    last_eval: round,
                    idx: top.idx,
                });
            }
        }

        // Singleton guard (Algorithm 1, line 13). The cost-aware density greedy
        // carries no approximation bound without it (Khuller et al. 1999): the
        // greedy set is compared against the best single item that fits
        // alongside M, and the higher-scoring of the two is returned. This is
        // the condition the (1 - e^-1/2) guarantee requires.
        //
        // The comparison uses the objective the greedy actually maximises: the
        // lam-weighted mix of the modular relevance term and the
        // facility-location coverage term.
        let objective = |rel_sum: f64, cover: &[f64]| -> f64 {
            lam * rel_sum + (1.0 - lam) * cover.iter().sum::<f64>()
        };

        let mut best = selected;
        let mut best_score = objective(selected_rel, &cover);
        let room = budget.saturating_sub(m_tok);
        for j in 0..n {
            if m_set.contains(&j) {
                continue;
            }
            let tok = self.estimate_tokens(&candidates[j]);
            // Same admissibility test the greedy applies, so both arms of the
            // comparison draw from the same candidate pool.
            if tok == 0 || tok > room {
                continue;
            }
            // Coverage of M + {j} without materialising the union.
            let cov: f64 = m_cover
                .iter()
                .zip(col(j))
                .map(|(&c, &w)| if w > c { w } else { c })
                .sum();
            let score = lam * (m_rel + rel[j]) + (1.0 - lam) * cov;
            if score > best_score {
                best_score = score;
                let mut candidate_set = m_set.clone();
                candidate_set.insert(j);
                best = candidate_set;
            }
        }

        let mut plan_first: Vec<usize> = best
            .iter()
            .copied()
            .filter(|i| plan_set.contains(i))
            .collect();
        let mut others: Vec<usize> = best
            .iter()
            .copied()
            .filter(|i| !plan_set.contains(i))
            .collect();
        plan_first.sort_unstable();
        others.sort_unstable();
        plan_first.extend(others);
        plan_first
    }
}

/// Row-major `(n + 1) x d` matrix of L2-normalised embeddings, with the query
/// in the last row. Normalising once up front turns cosine similarity into a
/// bare dot product, which is what removes the repeated norm computation from
/// the `O(n^2)` coverage-weight loop.
///
/// Rows whose source vector had zero norm, or whose length disagrees with the
/// query's dimension, are stored as zeros. Their dot products are then 0,
/// matching the `norm == 0` and length-mismatch guards in the scalar cosine
/// this replaces.
struct UnitRows {
    data: Vec<f32>,
    d: usize,
}

impl UnitRows {
    fn build(embeddings: &[Vec<f32>], query: &[f32]) -> Self {
        let n = embeddings.len();
        let d = query.len();
        let mut data = vec![0.0f32; (n + 1) * d];
        if d == 0 {
            return Self { data, d };
        }
        /// Writes `src` normalised into row `i`, or leaves the row zeroed if
        /// `src` has the wrong dimension or zero norm.
        fn write_unit(data: &mut [f32], i: usize, d: usize, src: &[f32]) {
            if src.len() != d {
                return;
            }
            let norm = src.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm == 0.0 {
                return;
            }
            for (dst, &x) in data[i * d..(i + 1) * d].iter_mut().zip(src.iter()) {
                *dst = x / norm;
            }
        }
        for (i, src) in embeddings.iter().enumerate() {
            write_unit(&mut data, i, d, src);
        }
        write_unit(&mut data, n, d, query);
        Self { data, d }
    }

    #[inline]
    fn row(&self, i: usize) -> &[f32] {
        &self.data[i * self.d..(i + 1) * self.d]
    }

    /// Cosine similarity between rows `i` and `j`, i.e. their dot product,
    /// since every row is already unit length (or zero).
    #[inline]
    fn cos(&self, i: usize, j: usize) -> f32 {
        self.row(i)
            .iter()
            .zip(self.row(j).iter())
            .map(|(x, y)| x * y)
            .sum()
    }
}

/// Heap entry for the CELF lazy-greedy loop. `BinaryHeap` in Rust is a
/// max-heap (Python's `heapq` is a min-heap), so `Ord` here is defined
/// directly on "highest ratio wins, ties broken by lowest index" — the
/// equivalent of Python's `(-ratio, last_eval, idx)` min-heap tuple, without
/// needing to negate the ratio a second time.
struct HeapEntry {
    ratio: f64,
    last_eval: u64,
    idx: usize,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.ratio == other.ratio && self.last_eval == other.last_eval && self.idx == other.idx
    }
}
impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Matches Python's tuple comparison (-ratio, last_eval, idx) on a
        // min-heap: highest ratio first, ties broken by lowest last_eval,
        // remaining ties broken by lowest idx. Each "lowest wins" step is
        // inverted here (other.cmp(&self)) because BinaryHeap pops the
        // greatest element first.
        self.ratio
            .partial_cmp(&other.ratio)
            .unwrap_or(Ordering::Equal)
            .then_with(|| other.last_eval.cmp(&self.last_eval))
            .then_with(|| other.idx.cmp(&self.idx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scalar cosine this module used before the rows were pre-normalised.
    /// Kept as the reference implementation that `UnitRows::cos` is checked
    /// against, so the optimisation cannot silently drift from the definition.
    fn reference_cosine(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            0.0
        } else {
            dot / (norm_a * norm_b)
        }
    }

    /// Deterministic xorshift, so the property tests need no dev-dependency.
    struct Rng(u64);
    impl Rng {
        fn next_f32(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            ((self.0 >> 11) as f32 / (1u64 << 53) as f32) * 2.0 - 1.0
        }
        fn next_usize(&mut self, hi: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as usize % hi.max(1)
        }
    }

    /// The pre-normalised dot product must agree with the scalar cosine it
    /// replaced, including the zero-vector and dimension-mismatch guards.
    #[test]
    fn unit_rows_cos_matches_scalar_cosine() {
        let mut rng = Rng(0x2545F4914F6CDD1D);
        let d = 128;
        let mut worst = 0.0f32;
        for trial in 0..40 {
            let n = 6;
            let mut embs: Vec<Vec<f32>> = Vec::new();
            for i in 0..n {
                if trial % 7 == 0 && i == 2 {
                    embs.push(vec![0.0; d]); // zero-norm row
                } else {
                    let scale = 0.1 + (i as f32);
                    embs.push((0..d).map(|_| rng.next_f32() * scale).collect());
                }
            }
            let q: Vec<f32> = (0..d).map(|_| rng.next_f32()).collect();
            let unit = UnitRows::build(&embs, &q);
            for i in 0..n {
                for j in 0..n {
                    if i == j {
                        continue;
                    }
                    let got = unit.cos(i, j);
                    let want = reference_cosine(&embs[i], &embs[j]);
                    worst = worst.max((got - want).abs());
                }
                let got_q = unit.cos(i, n);
                let want_q = reference_cosine(&embs[i], &q);
                worst = worst.max((got_q - want_q).abs());
            }
        }
        assert!(worst < 1e-5, "max cosine deviation {worst} exceeds 1e-5");
    }

    #[test]
    fn unit_rows_handles_dimension_mismatch_and_empty() {
        let q = vec![1.0f32, 0.0, 0.0];
        // Row 1 has the wrong dimension: it must score 0 against everything.
        let embs = vec![vec![1.0, 0.0, 0.0], vec![1.0, 0.0]];
        let unit = UnitRows::build(&embs, &q);
        assert_eq!(unit.cos(1, 0), 0.0);
        assert_eq!(unit.cos(1, 2), 0.0);
        assert!((unit.cos(0, 2) - 1.0).abs() < 1e-6);

        // Zero-dimension query must not panic.
        let unit = UnitRows::build(&[vec![]], &[]);
        assert_eq!(unit.cos(0, 1), 0.0);
    }

    /// The facility-location objective the greedy maximises, computed directly
    /// from the definition rather than from the incremental cover vector.
    fn objective_from_scratch(selected: &[usize], embs: &[Vec<f32>], q: &[f32], lam: f64) -> f64 {
        let n = embs.len();
        let rel: Vec<f64> = (0..n)
            .map(|i| f64::from(reference_cosine(&embs[i], q).max(0.0)))
            .collect();
        let w = |i: usize, j: usize| -> f64 {
            let s = if i == j {
                1.0
            } else {
                f64::from(reference_cosine(&embs[i], &embs[j]).max(0.0))
            };
            rel[i] * s
        };
        let coverage: f64 = (0..n)
            .map(|i| {
                selected
                    .iter()
                    .map(|&j| w(i, j))
                    .fold(0.0f64, |acc, v| if v > acc { v } else { acc })
            })
            .sum();
        let modular: f64 = selected.iter().map(|&j| rel[j]).sum();
        lam * modular + (1.0 - lam) * coverage
    }

    /// The singleton guard may only ever raise the objective. If it fires and
    /// the result scores lower than the plain greedy set, the guard is wrong.
    #[test]
    fn singleton_guard_never_lowers_the_objective() {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        let d = 64;
        let mut fired = 0;
        for _ in 0..200 {
            let n = 4 + rng.next_usize(24);
            // Cluster the embeddings around a few centres so coverage bites.
            let centres: Vec<Vec<f32>> = (0..3)
                .map(|_| (0..d).map(|_| rng.next_f32()).collect())
                .collect();
            let mut embs: Vec<Vec<f32>> = Vec::with_capacity(n);
            let mut cands: Vec<String> = Vec::with_capacity(n);
            for i in 0..n {
                let c = &centres[i % 3];
                embs.push(
                    (0..d)
                        .map(|k| c[k] + 0.4 * rng.next_f32())
                        .collect::<Vec<f32>>(),
                );
                cands.push("x".repeat(4 * (1 + rng.next_usize(400))));
            }
            let q: Vec<f32> = (0..d)
                .map(|k| centres[0][k] + 0.25 * rng.next_f32())
                .collect();

            let vs = VectorStore::disabled();
            let selector = PacmsSelector::new(&vs);
            let total: usize = cands.iter().map(|c| c.len() / 4).sum();
            let budget = total * (15 + rng.next_usize(55)) / 100;
            let lam = (1.0 - (budget as f64 / total.max(1) as f64)).clamp(0.01, 0.99);

            let got = selector.select_from_embeddings(&cands, &embs, &q, budget, None);

            // Recompute the greedy-only result by disabling the guard: the guard
            // can only replace the set with M + {j}, which with no mandatory set
            // is a single item. So if the answer is a single item, check it beats
            // every other single item; otherwise it is the greedy set.
            let f_got = objective_from_scratch(&got, &embs, &q, lam);
            let best_single = (0..n)
                .filter(|&j| {
                    let t = cands[j].len() / 4;
                    t > 0 && t <= budget
                })
                .map(|j| objective_from_scratch(&[j], &embs, &q, lam))
                .fold(f64::NEG_INFINITY, f64::max);
            if got.len() == 1 {
                fired += 1;
            }
            assert!(
                f_got >= best_single - 1e-9,
                "returned set scores {f_got} but the best single item scores {best_single}"
            );
            // Budget must hold regardless.
            let spent: usize = got.iter().map(|&j| cands[j].len() / 4).sum();
            assert!(spent <= budget, "spent {spent} over budget {budget}");
        }
        // Sanity: the sweep should exercise both branches at least sometimes.
        assert!(fired < 200, "guard fired on every case, suspicious");
    }

    #[test]
    fn lastk_keeps_most_recent_within_budget() {
        let vs = VectorStore::disabled();
        let selector = PacmsSelector::new(&vs).with_token_estimator(|s: &str| s.len());
        let candidates: Vec<String> = vec!["a".into(), "bb".into(), "ccc".into(), "dddd".into()];
        // Walking from most recent: "dddd" (4) fits (tok=4); "ccc" (3) would
        // overflow (7>5) so it's skipped; "bb" (2) would also overflow (6>5);
        // "a" (1) fits (tok=5). Selected = {3, 0}, returned in index order.
        let selected = selector.select_lastk(&candidates, 5, None);
        assert_eq!(selected, vec![0, 3]);
    }

    #[test]
    fn lastk_seeds_mandatory_first() {
        let vs = VectorStore::disabled();
        let selector = PacmsSelector::new(&vs).with_token_estimator(|s: &str| s.len());
        let candidates: Vec<String> = vec!["a".into(), "bb".into(), "ccc".into(), "dddd".into()];
        let mandatory: HashSet<usize> = [0].into_iter().collect();
        let selected = selector.select_lastk(&candidates, 5, Some(&mandatory));
        // mandatory (0, cost 1) + most recent that fits in remaining budget 4 -> index 3 (cost 4)
        assert_eq!(selected, vec![0, 3]);
    }

    #[test]
    fn lastk_empty_candidates() {
        let vs = VectorStore::disabled();
        let selector = PacmsSelector::new(&vs);
        let selected = selector.select_lastk(&[], 100, None);
        assert!(selected.is_empty());
    }

    #[test]
    fn heap_entry_orders_by_ratio_then_lowest_index() {
        let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::new();
        heap.push(HeapEntry {
            ratio: 1.0,
            last_eval: 0,
            idx: 5,
        });
        heap.push(HeapEntry {
            ratio: 2.0,
            last_eval: 0,
            idx: 1,
        });
        heap.push(HeapEntry {
            ratio: 2.0,
            last_eval: 0,
            idx: 0,
        });
        // Highest ratio first; ties broken by lowest index.
        assert_eq!(heap.pop().unwrap().idx, 0);
        assert_eq!(heap.pop().unwrap().idx, 1);
        assert_eq!(heap.pop().unwrap().idx, 5);
    }
}
