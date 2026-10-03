//! POC benchmark: compare 6 MCP tool-search strategies.
//!
//! Strategies tested (see `docs/MCP_TOOL_SEARCH_DESIGN.md` §5 + dataset guide §4):
//!
//! | # | Architecture | Level 1 (Connectors) | Level 2 (Tools) |
//! |---|-------------|---------------------|----------------|
//! | 1 | Two-level   | BM25                | BM25           |
//! | 2 | Two-level   | BM25                | Semantic        |
//! | 3 | Two-level   | Semantic            | BM25           |
//! | 4 | Two-level   | Semantic            | Semantic        |
//! | 5 | Flat        | —                   | BM25           |
//! | 6 | Flat        | —                   | Semantic        |
//!
//! Usage:
//!   OPENAI_API_KEY=sk-… cargo run -p nasiko-mcp-gateway --bin tool-search-bench -- \
//!       --catalog data/tool-catalog.json --dataset data/dataset.json
//!
//!   # Quick smoke test (first 50 queries):
//!   … --dry-run

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use serde::{Deserialize, Serialize};

// ── CLI ────────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "tool-search-bench")]
struct Cli {
    /// Path to tool-catalog.json
    #[arg(long, default_value = "data/tool-catalog.json")]
    catalog: PathBuf,

    /// Path to dataset.json
    #[arg(long, default_value = "data/dataset.json")]
    dataset: PathBuf,

    /// Top-k connectors for Level 1
    #[arg(long, default_value_t = 3)]
    top_k_connectors: usize,

    /// Top-k tools for Level 2 / flat (must be >= 25 for recall@25)
    #[arg(long, default_value_t = 25)]
    top_k_tools: usize,

    /// Run on only the first N queries (smoke test)
    #[arg(long)]
    dry_run: Option<usize>,

    /// Embedding model (OpenAI)
    #[arg(long, default_value = "text-embedding-3-small")]
    embed_model: String,

    /// Write results JSON to this path
    #[arg(long, default_value = "tool-search-bench-results.json")]
    output: PathBuf,
}

// ── Data types ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, Clone)]
struct CatalogEntry {
    connector_name: String,
    connector_description: String,
    tool_name: String,
    tool_description: String,
}

#[derive(Deserialize, Clone)]
struct QueryEntry {
    #[allow(dead_code)]
    id: String,
    query: String,
    category: String,
    expected_connectors: Vec<String>,
    expected_tools: Vec<String>,
}

/// A connector's searchable text (deduplicated from the catalog).
#[derive(Clone)]
struct Connector {
    name: String,
    description: String,
}

/// A tool's searchable text.
#[derive(Clone)]
struct Tool {
    #[allow(dead_code)]
    connector_name: String,
    tool_name: String,
    description: String,
}

// ── BM25 ───────────────────────────────────────────────────────────────────────

struct Bm25Index {
    /// Each doc: (id, token list).  `id` is connector_name or tool_name.
    docs: Vec<(String, Vec<String>)>,
    /// doc freq per token
    df: HashMap<String, usize>,
    /// average document length
    avgdl: f64,
    k1: f64,
    b: f64,
}

impl Bm25Index {
    fn build(items: &[(String, String)], k1: f64, b: f64) -> Self {
        let docs: Vec<(String, Vec<String>)> = items
            .iter()
            .map(|(id, text)| (id.clone(), tokenize(text)))
            .collect();

        let mut df: HashMap<String, usize> = HashMap::new();
        for (_, tokens) in &docs {
            let unique: HashSet<&str> = tokens.iter().map(|s| s.as_str()).collect();
            for t in unique {
                *df.entry(t.to_string()).or_default() += 1;
            }
        }

        let total_len: usize = docs.iter().map(|(_, t)| t.len()).sum();
        let avgdl = if docs.is_empty() {
            1.0
        } else {
            total_len as f64 / docs.len() as f64
        };

        Self {
            docs,
            df,
            avgdl,
            k1,
            b,
        }
    }

    fn search(&self, query: &str, limit: usize) -> Vec<(String, f64)> {
        let qtokens = tokenize(query);
        let n = self.docs.len() as f64;

        let mut scores: Vec<(String, f64)> = self
            .docs
            .iter()
            .map(|(id, doc_tokens)| {
                let dl = doc_tokens.len() as f64;
                let mut score = 0.0f64;
                for qt in &qtokens {
                    let tf = doc_tokens.iter().filter(|t| t == &qt).count() as f64;
                    let df = self.df.get(qt.as_str()).copied().unwrap_or(0) as f64;
                    if df == 0.0 {
                        continue;
                    }
                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                    let tf_norm = (tf * (self.k1 + 1.0))
                        / (tf + self.k1 * (1.0 - self.b + self.b * dl / self.avgdl));
                    score += idf * tf_norm;
                }
                (id.clone(), score)
            })
            .filter(|(_, s)| *s > 0.0)
            .collect();

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores.truncate(limit);
        scores
    }
}

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty() && s.len() > 1)
        .map(|s| s.to_string())
        .collect()
}

// ── Semantic (embedding) index ─────────────────────────────────────────────────

struct SemanticIndex {
    /// (id, embedding vector)
    items: Vec<(String, Vec<f32>)>,
}

impl SemanticIndex {
    fn new(ids: Vec<String>, embeddings: Vec<Vec<f32>>) -> Self {
        let items = ids.into_iter().zip(embeddings).collect();
        Self { items }
    }

    fn search(&self, query_embedding: &[f32], limit: usize) -> Vec<(String, f64)> {
        let mut scores: Vec<(String, f64)> = self
            .items
            .iter()
            .map(|(id, emb)| (id.clone(), cosine_similarity(query_embedding, emb) as f64))
            .collect();

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scores.truncate(limit);
        scores
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

// ── Embedding client ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

async fn embed_batch(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    texts: &[String],
) -> anyhow::Result<Vec<Vec<f32>>> {
    // OpenAI allows up to 2048 inputs per batch; we chunk at 512 to stay safe.
    const CHUNK: usize = 512;
    let mut all = Vec::with_capacity(texts.len());

    for chunk in texts.chunks(CHUNK) {
        let body = serde_json::json!({
            "model": model,
            "input": chunk,
        });

        let resp = client
            .post("https://api.openai.com/v1/embeddings")
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Embedding API {status}: {text}");
        }

        let parsed: EmbeddingResponse = resp.json().await?;
        // Response may not preserve input order — sort by index.
        // OpenAI guarantees response order matches input order.
        for d in parsed.data {
            all.push(d.embedding);
        }
    }

    Ok(all)
}

// ── Search approaches ──────────────────────────────────────────────────────────

struct SearchResult {
    connectors: Vec<String>,
    tools: Vec<String>,
}

/// Two-level search: Level 1 (connector index) → Level 2 (tool index per connector).
fn two_level_search(
    query: &str,
    query_embedding: Option<&[f32]>,
    connector_index: &dyn Searcher,
    tool_indices: &HashMap<String, Box<dyn Searcher>>,
    top_k_connectors: usize,
    top_k_tools: usize,
) -> SearchResult {
    let matched_connectors = connector_index.search_text(query, query_embedding, top_k_connectors);
    let connector_names: Vec<String> = matched_connectors.into_iter().map(|(id, _)| id).collect();

    let mut tools = Vec::new();
    for conn in &connector_names {
        if let Some(idx) = tool_indices.get(conn) {
            let hits = idx.search_text(query, query_embedding, top_k_tools);
            // Min-max normalize scores to [0,1] so results from different
            // per-connector indexes (with different corpus sizes / avgdl) are
            // comparable.  Without this, BM25 scores from a small connector
            // are structurally higher than from a large one.
            let min = hits.iter().map(|(_, s)| *s).fold(f64::INFINITY, f64::min);
            let max = hits
                .iter()
                .map(|(_, s)| *s)
                .fold(f64::NEG_INFINITY, f64::max);
            let range = max - min;
            tools.extend(hits.into_iter().map(|(id, score)| {
                let norm = if range > 0.0 {
                    (score - min) / range
                } else if max > 0.0 {
                    1.0 // single result or all-equal scores
                } else {
                    0.0
                };
                (id, norm)
            }));
        }
    }
    // Re-sort globally by normalized scores and truncate.
    tools.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    tools.truncate(top_k_tools);

    SearchResult {
        connectors: connector_names,
        tools: tools.into_iter().map(|(id, _)| id).collect(),
    }
}

/// Flat search: one index over all tools.
fn flat_search(
    query: &str,
    query_embedding: Option<&[f32]>,
    tool_index: &dyn Searcher,
    top_k_tools: usize,
) -> SearchResult {
    let hits = tool_index.search_text(query, query_embedding, top_k_tools);
    SearchResult {
        connectors: vec![], // not applicable
        tools: hits.into_iter().map(|(id, _)| id).collect(),
    }
}

/// Unified trait so two-level/flat code is generic over BM25 vs semantic.
trait Searcher: Send + Sync {
    fn search_text(
        &self,
        query: &str,
        query_embedding: Option<&[f32]>,
        limit: usize,
    ) -> Vec<(String, f64)>;
}

struct Bm25Searcher(Bm25Index);

impl Searcher for Bm25Searcher {
    fn search_text(&self, query: &str, _: Option<&[f32]>, limit: usize) -> Vec<(String, f64)> {
        self.0.search(query, limit)
    }
}

struct SemanticSearcher(SemanticIndex);

impl Searcher for SemanticSearcher {
    fn search_text(
        &self,
        _: &str,
        query_embedding: Option<&[f32]>,
        limit: usize,
    ) -> Vec<(String, f64)> {
        match query_embedding {
            Some(emb) => self.0.search(emb, limit),
            None => vec![],
        }
    }
}

// ── Metrics ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Default)]
struct Metrics {
    connector_recall: f64,
    tool_recall_at_5: f64,
    tool_recall_at_10: f64,
    tool_recall_at_15: f64,
    tool_recall_at_20: f64,
    tool_recall_at_25: f64,
    tool_precision: f64,
    mrr: f64,
    hit_at_5: f64,
    hit_at_10: f64,
    latency_p50_us: f64,
    latency_p95_us: f64,
    count: usize,
}

#[derive(Clone)]
struct PerQuery {
    connector_recall: f64,
    tool_recall_at_5: f64,
    tool_recall_at_10: f64,
    tool_recall_at_15: f64,
    tool_recall_at_20: f64,
    tool_recall_at_25: f64,
    tool_precision: f64,
    reciprocal_rank: f64,
    hit_at_5: bool,
    hit_at_10: bool,
    latency_us: f64,
}

fn compute_per_query(result: &SearchResult, entry: &QueryEntry, latency_us: f64) -> PerQuery {
    let expected_conns: HashSet<&str> = entry
        .expected_connectors
        .iter()
        .map(|s| s.as_str())
        .collect();
    let expected_tools: HashSet<&str> = entry.expected_tools.iter().map(|s| s.as_str()).collect();

    let connector_recall = if expected_conns.is_empty() {
        1.0
    } else {
        let found = result
            .connectors
            .iter()
            .filter(|c| expected_conns.contains(c.as_str()))
            .count();
        found as f64 / expected_conns.len() as f64
    };

    let tool_recall = |k: usize| {
        if expected_tools.is_empty() {
            return 1.0;
        }
        let found = result.tools[..result.tools.len().min(k)]
            .iter()
            .filter(|t| expected_tools.contains(t.as_str()))
            .count();
        found as f64 / expected_tools.len() as f64
    };

    let tool_precision = if result.tools.is_empty() {
        0.0
    } else {
        let found = result
            .tools
            .iter()
            .filter(|t| expected_tools.contains(t.as_str()))
            .count();
        found as f64 / result.tools.len() as f64
    };

    let first_correct_rank = result
        .tools
        .iter()
        .position(|t| expected_tools.contains(t.as_str()));

    let reciprocal_rank = first_correct_rank
        .map(|r| 1.0 / (r as f64 + 1.0))
        .unwrap_or(0.0);

    let hit_at = |k: usize| {
        result.tools[..result.tools.len().min(k)]
            .iter()
            .any(|t| expected_tools.contains(t.as_str()))
    };

    PerQuery {
        connector_recall,
        tool_recall_at_5: tool_recall(5),
        tool_recall_at_10: tool_recall(10),
        tool_recall_at_15: tool_recall(15),
        tool_recall_at_20: tool_recall(20),
        tool_recall_at_25: tool_recall(25),
        tool_precision,
        reciprocal_rank,
        hit_at_5: hit_at(5),
        hit_at_10: hit_at(10),
        latency_us,
    }
}

fn aggregate(results: &[PerQuery]) -> Metrics {
    let n = results.len();
    if n == 0 {
        return Metrics::default();
    }
    let nf = n as f64;

    let mut latencies: Vec<f64> = results.iter().map(|r| r.latency_us).collect();
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());

    Metrics {
        connector_recall: results.iter().map(|r| r.connector_recall).sum::<f64>() / nf,
        tool_recall_at_5: results.iter().map(|r| r.tool_recall_at_5).sum::<f64>() / nf,
        tool_recall_at_10: results.iter().map(|r| r.tool_recall_at_10).sum::<f64>() / nf,
        tool_recall_at_15: results.iter().map(|r| r.tool_recall_at_15).sum::<f64>() / nf,
        tool_recall_at_20: results.iter().map(|r| r.tool_recall_at_20).sum::<f64>() / nf,
        tool_recall_at_25: results.iter().map(|r| r.tool_recall_at_25).sum::<f64>() / nf,
        tool_precision: results.iter().map(|r| r.tool_precision).sum::<f64>() / nf,
        mrr: results.iter().map(|r| r.reciprocal_rank).sum::<f64>() / nf,
        hit_at_5: results.iter().filter(|r| r.hit_at_5).count() as f64 / nf,
        hit_at_10: results.iter().filter(|r| r.hit_at_10).count() as f64 / nf,
        latency_p50_us: percentile(&latencies, 50.0),
        latency_p95_us: percentile(&latencies, 95.0),
        count: n,
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (p / 100.0 * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ── Main ───────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ApproachResult {
    name: String,
    overall: Metrics,
    by_category: HashMap<String, Metrics>,
}

#[derive(Serialize)]
struct BenchOutput {
    config: BenchConfig,
    approaches: Vec<ApproachResult>,
}

#[derive(Serialize)]
struct BenchConfig {
    catalog_tools: usize,
    catalog_connectors: usize,
    dataset_queries: usize,
    top_k_connectors: usize,
    top_k_tools: usize,
    embed_model: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // ── Load data ──────────────────────────────────────────────────────────
    let catalog: Vec<CatalogEntry> = serde_json::from_str(&std::fs::read_to_string(&cli.catalog)?)?;
    let mut queries: Vec<QueryEntry> =
        serde_json::from_str(&std::fs::read_to_string(&cli.dataset)?)?;

    if let Some(n) = cli.dry_run {
        queries.truncate(n);
        eprintln!("[dry-run] using first {n} queries");
    }

    // Deduplicate connectors.
    let mut connector_map: HashMap<String, Connector> = HashMap::new();
    for e in &catalog {
        connector_map
            .entry(e.connector_name.clone())
            .or_insert_with(|| Connector {
                name: e.connector_name.clone(),
                description: e.connector_description.clone(),
            });
    }
    let connectors: Vec<Connector> = {
        let mut v: Vec<_> = connector_map.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    };

    // Tools grouped by connector.
    let mut tools_by_connector: HashMap<String, Vec<Tool>> = HashMap::new();
    let mut all_tools: Vec<Tool> = Vec::new();
    for e in &catalog {
        let tool = Tool {
            connector_name: e.connector_name.clone(),
            tool_name: e.tool_name.clone(),
            description: e.tool_description.clone(),
        };
        tools_by_connector
            .entry(e.connector_name.clone())
            .or_default()
            .push(tool.clone());
        all_tools.push(tool);
    }

    eprintln!(
        "Loaded {} tools across {} connectors, {} queries",
        all_tools.len(),
        connectors.len(),
        queries.len()
    );

    // ── Build BM25 indexes ─────────────────────────────────────────────────
    eprintln!("Building BM25 indexes…");

    let connector_bm25_items: Vec<(String, String)> = connectors
        .iter()
        .map(|c| (c.name.clone(), format!("{} {}", c.name, c.description)))
        .collect();
    let connector_bm25 = Bm25Searcher(Bm25Index::build(&connector_bm25_items, 1.5, 0.75));

    let mut tool_bm25_by_connector: HashMap<String, Box<dyn Searcher>> = HashMap::new();
    for (conn_name, tools) in &tools_by_connector {
        let items: Vec<(String, String)> = tools
            .iter()
            .map(|t| {
                let text = format!("{} {}", t.tool_name.replace('_', " "), t.description);
                (t.tool_name.clone(), text)
            })
            .collect();
        tool_bm25_by_connector.insert(
            conn_name.clone(),
            Box::new(Bm25Searcher(Bm25Index::build(&items, 1.5, 0.75))),
        );
    }

    let all_tools_bm25_items: Vec<(String, String)> = all_tools
        .iter()
        .map(|t| {
            let text = format!("{} {}", t.tool_name.replace('_', " "), t.description);
            (t.tool_name.clone(), text)
        })
        .collect();
    let flat_bm25 = Bm25Searcher(Bm25Index::build(&all_tools_bm25_items, 1.5, 0.75));

    // ── Build semantic indexes ─────────────────────────────────────────────
    let api_key =
        std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY must be set for semantic search");

    let http = reqwest::Client::new();

    eprintln!("Embedding {} connectors…", connectors.len());
    let connector_texts: Vec<String> = connectors
        .iter()
        .map(|c| format!("{}: {}", c.name, c.description))
        .collect();
    let connector_ids: Vec<String> = connectors.iter().map(|c| c.name.clone()).collect();
    let connector_embeddings =
        embed_batch(&http, &api_key, &cli.embed_model, &connector_texts).await?;
    let connector_semantic =
        SemanticSearcher(SemanticIndex::new(connector_ids, connector_embeddings));

    eprintln!("Embedding {} tools…", all_tools.len());
    let tool_texts: Vec<String> = all_tools
        .iter()
        .map(|t| format!("{}: {}", t.tool_name.replace('_', " "), t.description))
        .collect();
    let tool_ids: Vec<String> = all_tools.iter().map(|t| t.tool_name.clone()).collect();
    let tool_embeddings = embed_batch(&http, &api_key, &cli.embed_model, &tool_texts).await?;

    // Per-connector semantic indexes.
    let mut tool_semantic_by_connector: HashMap<String, Box<dyn Searcher>> = HashMap::new();
    {
        // Map tool_name → embedding for lookup.
        let emb_map: HashMap<&str, &Vec<f32>> = tool_ids
            .iter()
            .zip(tool_embeddings.iter())
            .map(|(id, emb)| (id.as_str(), emb))
            .collect();

        for (conn_name, tools) in &tools_by_connector {
            let ids: Vec<String> = tools.iter().map(|t| t.tool_name.clone()).collect();
            let embeddings: Vec<Vec<f32>> =
                ids.iter().map(|id| emb_map[id.as_str()].clone()).collect();
            tool_semantic_by_connector.insert(
                conn_name.clone(),
                Box::new(SemanticSearcher(SemanticIndex::new(ids, embeddings))),
            );
        }
    }

    let flat_semantic = SemanticSearcher(SemanticIndex::new(tool_ids, tool_embeddings));

    // ── Pre-embed all queries ──────────────────────────────────────────────
    eprintln!("Embedding {} queries…", queries.len());
    let query_texts: Vec<String> = queries.iter().map(|q| q.query.clone()).collect();
    let query_embeddings = embed_batch(&http, &api_key, &cli.embed_model, &query_texts).await?;

    // ── Run 6 approaches ───────────────────────────────────────────────────

    let approach_names = [
        "two-level: BM25 → BM25",
        "two-level: BM25 → Semantic",
        "two-level: Semantic → BM25",
        "two-level: Semantic → Semantic",
        "flat: BM25",
        "flat: Semantic",
    ];

    let mut all_results: Vec<ApproachResult> = Vec::new();

    for (idx, name) in approach_names.iter().enumerate() {
        eprintln!("\nRunning approach {}: {name}", idx + 1);
        let mut per_query_results: Vec<(String, PerQuery)> = Vec::new(); // (category, metrics)

        for (qi, entry) in queries.iter().enumerate() {
            let qemb = &query_embeddings[qi];
            let qemb_slice: Option<&[f32]> = Some(qemb.as_slice());

            let start = Instant::now();
            let result = match idx {
                0 => two_level_search(
                    &entry.query,
                    qemb_slice,
                    &connector_bm25,
                    &tool_bm25_by_connector,
                    cli.top_k_connectors,
                    cli.top_k_tools,
                ),
                1 => two_level_search(
                    &entry.query,
                    qemb_slice,
                    &connector_bm25,
                    &tool_semantic_by_connector,
                    cli.top_k_connectors,
                    cli.top_k_tools,
                ),
                2 => two_level_search(
                    &entry.query,
                    qemb_slice,
                    &connector_semantic,
                    &tool_bm25_by_connector,
                    cli.top_k_connectors,
                    cli.top_k_tools,
                ),
                3 => two_level_search(
                    &entry.query,
                    qemb_slice,
                    &connector_semantic,
                    &tool_semantic_by_connector,
                    cli.top_k_connectors,
                    cli.top_k_tools,
                ),
                4 => flat_search(&entry.query, qemb_slice, &flat_bm25, cli.top_k_tools),
                5 => flat_search(&entry.query, qemb_slice, &flat_semantic, cli.top_k_tools),
                _ => unreachable!(),
            };
            let elapsed_us = start.elapsed().as_micros() as f64;

            let pq = compute_per_query(&result, entry, elapsed_us);
            per_query_results.push((entry.category.clone(), pq));
        }

        // Aggregate overall.
        let all_pq: Vec<PerQuery> = per_query_results.iter().map(|(_, pq)| pq.clone()).collect();
        let overall = aggregate(&all_pq);

        // Aggregate by category.
        let mut by_cat: HashMap<String, Vec<PerQuery>> = HashMap::new();
        for (cat, pq) in per_query_results {
            by_cat.entry(cat).or_default().push(pq);
        }
        let by_category: HashMap<String, Metrics> = by_cat
            .iter()
            .map(|(cat, pqs)| (cat.clone(), aggregate(pqs)))
            .collect();

        // Print summary line.
        eprintln!(
            "  recall@15={:.3}  mrr={:.3}  hit@5={:.3}  p50={:.0}µs",
            overall.tool_recall_at_15, overall.mrr, overall.hit_at_5, overall.latency_p50_us
        );

        all_results.push(ApproachResult {
            name: name.to_string(),
            overall,
            by_category,
        });
    }

    // ── Print comparison table ─────────────────────────────────────────────
    println!();
    println!(
        "{:<30} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>8} {:>8} {:>10} {:>10}",
        "Approach",
        "Recall@5",
        "Recall@10",
        "Recall@15",
        "Recall@20",
        "Recall@25",
        "Precision",
        "MRR",
        "Hit@5",
        "p50(µs)",
        "p95(µs)"
    );
    println!("{}", "-".repeat(148));
    for r in &all_results {
        let m = &r.overall;
        println!(
            "{:<30} {:>10.4} {:>10.4} {:>10.4} {:>10.4} {:>10.4} {:>10.4} {:>8.4} {:>8.4} {:>10.0} {:>10.0}",
            r.name,
            m.tool_recall_at_5,
            m.tool_recall_at_10,
            m.tool_recall_at_15,
            m.tool_recall_at_20,
            m.tool_recall_at_25,
            m.tool_precision,
            m.mrr,
            m.hit_at_5,
            m.latency_p50_us,
            m.latency_p95_us,
        );
    }

    // Per-category breakdown.
    let categories = [
        "exact_match",
        "synonym",
        "multi_tool",
        "multi_toolkit",
        "vague",
    ];
    for cat in &categories {
        println!("\n── {cat} ──");
        println!(
            "{:<30} {:>10} {:>10} {:>10} {:>10} {:>8} {:>8}",
            "Approach", "Recall@5", "Recall@15", "Recall@20", "Recall@25", "MRR", "Hit@5"
        );
        println!("{}", "-".repeat(106));
        for r in &all_results {
            if let Some(m) = r.by_category.get(*cat) {
                println!(
                    "{:<30} {:>10.4} {:>10.4} {:>10.4} {:>10.4} {:>8.4} {:>8.4}",
                    r.name,
                    m.tool_recall_at_5,
                    m.tool_recall_at_15,
                    m.tool_recall_at_20,
                    m.tool_recall_at_25,
                    m.mrr,
                    m.hit_at_5,
                );
            }
        }
    }

    // Connector recall for two-level approaches.
    println!("\n── Connector Recall (two-level only) ──");
    println!("{:<30} {:>12}", "Approach", "ConnRecall");
    println!("{}", "-".repeat(44));
    for r in &all_results[..4] {
        println!("{:<30} {:>12.4}", r.name, r.overall.connector_recall);
    }

    // ── Save JSON ──────────────────────────────────────────────────────────
    let output = BenchOutput {
        config: BenchConfig {
            catalog_tools: catalog.len(),
            catalog_connectors: connectors.len(),
            dataset_queries: queries.len(),
            top_k_connectors: cli.top_k_connectors,
            top_k_tools: cli.top_k_tools,
            embed_model: cli.embed_model,
        },
        approaches: all_results,
    };
    std::fs::write(&cli.output, serde_json::to_string_pretty(&output)?)?;
    eprintln!("\nResults written to {}", cli.output.display());

    Ok(())
}
