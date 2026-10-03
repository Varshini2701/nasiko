//! Model identity: turning whatever a producer called a model into the keys a
//! price book can be searched by.
//!
//! The same model reaches us under several spellings — `claude-opus-4-6` from a
//! coding-agent transcript, `anthropic/claude-opus-4.6` from an OpenRouter price
//! book, `us.anthropic.claude-opus-4-v1:0` from Bedrock. Normalizing in one place
//! is what stops the DB lookup and the static table disagreeing about what a
//! model is called.

/// Who makes the model, inferred from its name rather than from who hosts it.
///
/// Cache pricing follows vendor-level conventions (Anthropic charges 1.25x input
/// to write and 0.10x to read; OpenAI does not charge for writes at all), and
/// those conventions travel with the model — a Claude served through Bedrock is
/// still priced like a Claude. Keying on the vendor rather than the provider
/// label is what makes [`crate::CacheRatios`] correct for re-hosted models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Vendor {
    Anthropic,
    OpenAi,
    Google,
    DeepSeek,
    Unknown,
}

/// A model name reduced to the forms a price book is searched by, cheapest
/// (most specific) first. See [`resolve_model`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelKey {
    /// Canonical provider label, or `None` when the caller did not supply one.
    pub provider: Option<String>,
    /// Exactly what the producer reported. Never used for lookup — kept so a
    /// stored row can be traced back to the call that produced it.
    pub model: String,
    /// Lowercased, vendor prefix and variant suffixes removed.
    pub canonical: String,
    /// `canonical` with a trailing minor version dropped, for the case where the
    /// price book carries the family but not the point release.
    pub family: String,
    pub vendor: Vendor,
}

/// Vendor-routing prefixes that some catalogs prepend. Order matters: the longer
/// `us.anthropic.` must be tried before `anthropic.`.
///
/// Stripping these is a **fallback**, never the first thing tried, because on
/// some upstreams the prefix is part of the model's real name rather than
/// routing noise: Bedrock genuinely calls its models `openai.gpt-6-astra` and
/// `anthropic.claude-opus-4-6-v1:0`, and its price book is keyed that way. See
/// [`crate::quote`], which probes the reported name before the stripped one.
const VENDOR_PREFIXES: &[&str] = &[
    "us.anthropic.",
    "eu.anthropic.",
    "us.meta.",
    "anthropic/",
    "anthropic.",
    "openai/",
    "openai.",
    "google/",
    "google.",
    "meta-llama/",
    "deepseek/",
    "zai-org/",
];

/// Variant markers that never change the price basis we can look up.
const VARIANT_SUFFIXES: &[&str] = &[":batch", ":free", ":beta", ".ft", "-latest"];

/// Reduce a reported `(provider, model)` to the keys a price book is searched by.
///
/// Pure and total: an unrecognized name still yields a `ModelKey`, with
/// `canonical == family` and [`Vendor::Unknown`], so callers never have to
/// handle an "unresolvable" case separately.
pub fn resolve_model(provider: Option<&str>, model: &str) -> ModelKey {
    let canonical = canonicalize(model);
    ModelKey {
        provider: provider.map(|p| p.trim().to_lowercase()),
        model: model.to_string(),
        family: reduce_to_family(&canonical),
        vendor: infer_vendor(&canonical),
        canonical,
    }
}

fn canonicalize(model: &str) -> String {
    let mut name = model.trim().to_lowercase();
    for prefix in VENDOR_PREFIXES {
        if let Some(rest) = name.strip_prefix(prefix) {
            name = rest.to_string();
            break;
        }
    }
    for suffix in VARIANT_SUFFIXES {
        if let Some(rest) = name.strip_suffix(suffix) {
            name = rest.to_string();
        }
    }
    let name = strip_revision_tag(name);
    strip_date_stamp(&name)
}

/// Drop a Bedrock-style revision, which arrives as `-v1:0` — a `:`-suffixed
/// generation followed by a `-v<n>` model revision.
fn strip_revision_tag(name: String) -> String {
    let name = match name.rsplit_once(':') {
        Some((head, tail)) if is_all_digits(tail) => head.to_string(),
        _ => name,
    };
    match name.rsplit_once("-v") {
        Some((head, tail)) if is_all_digits(tail) => head.to_string(),
        _ => name,
    }
}

/// Drop a trailing `-20241022` or `-2024-10-22` release stamp.
fn strip_date_stamp(name: &str) -> String {
    let parts: Vec<&str> = name.split('-').collect();
    // `-2024-10-22`: three trailing numeric parts of width 4, 2, 2.
    if parts.len() >= 4 {
        let [year, month, day] = [
            parts[parts.len() - 3],
            parts[parts.len() - 2],
            parts[parts.len() - 1],
        ];
        if is_digits(year, 4) && is_digits(month, 2) && is_digits(day, 2) {
            return parts[..parts.len() - 3].join("-");
        }
    }
    // `-20241022`: one trailing numeric part of width 8.
    if parts.len() >= 2 && is_digits(parts[parts.len() - 1], 8) {
        return parts[..parts.len() - 1].join("-");
    }
    name.to_string()
}

fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

fn is_digits(s: &str, width: usize) -> bool {
    s.len() == width && is_all_digits(s)
}

/// Drop a trailing minor version, but only when a major version remains behind it.
///
/// `claude-opus-4-6` and `claude-opus-4.6` both reduce to `claude-opus-4`, which
/// is the name the price book actually carries — books track families, not every
/// point release. The guard is what keeps this safe: the minor is dropped only
/// when the text before it *ends in a digit*, i.e. there is a major version left
/// to stand on. So `claude-sonnet-4` stays whole (dropping `4` would leave a
/// meaningless `claude-sonnet`) and `claude-3-5-sonnet` stays whole (its last
/// part is a tier, not a version). Both are real rows in `model_pricing`.
fn reduce_to_family(canonical: &str) -> String {
    let Some(boundary) = canonical.rfind(['-', '.']) else {
        return canonical.to_string();
    };
    let (head, minor) = canonical.split_at(boundary);
    let minor = &minor[1..];
    let major_precedes = head.ends_with(|c: char| c.is_ascii_digit());
    if is_all_digits(minor) && major_precedes {
        return head.to_string();
    }
    canonical.to_string()
}

fn infer_vendor(canonical: &str) -> Vendor {
    const MARKERS: &[(&str, Vendor)] = &[
        ("claude", Vendor::Anthropic),
        ("gpt", Vendor::OpenAi),
        ("o1", Vendor::OpenAi),
        ("o3", Vendor::OpenAi),
        ("gemini", Vendor::Google),
        ("deepseek", Vendor::DeepSeek),
    ];
    MARKERS
        .iter()
        .find(|(marker, _)| canonical.contains(marker))
        .map_or(Vendor::Unknown, |(_, vendor)| *vendor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(model: &str) -> ModelKey {
        resolve_model(None, model)
    }

    #[test]
    fn a_provider_label_is_normalized_but_never_rewritten() {
        // The pricing sync writes rows under the router's own provider label, so
        // the label a caller reports is the one the book is keyed by. Mapping it
        // to some "canonical" spelling walks past every synced row: a Bedrock
        // provider labelled `aws-bedrock` has 214 rows under exactly that label.
        assert_eq!(
            resolve_model(Some("  AWS-Bedrock "), "x")
                .provider
                .as_deref(),
            Some("aws-bedrock")
        );
        assert_eq!(
            resolve_model(Some("nebius-token-factory"), "x")
                .provider
                .as_deref(),
            Some("nebius-token-factory")
        );
    }

    #[test]
    fn vendor_prefixes_are_stripped() {
        assert_eq!(
            key("anthropic/claude-opus-4.6").canonical,
            "claude-opus-4.6"
        );
        assert_eq!(
            key("us.anthropic.claude-opus-4-v1:0").canonical,
            "claude-opus-4"
        );
        assert_eq!(key("openai.gpt-6-astra").canonical, "gpt-6-astra");
        assert_eq!(key("zai-org/GLM-5.3").canonical, "glm-5.3");
    }

    #[test]
    fn variant_suffixes_are_stripped() {
        assert_eq!(
            key("anthropic/claude-opus-4.6:batch").canonical,
            "claude-opus-4.6"
        );
        assert_eq!(key("gpt-4o.ft").canonical, "gpt-4o");
        assert_eq!(key("chatgpt-4o-latest").canonical, "chatgpt-4o");
    }

    #[test]
    fn release_stamps_are_stripped() {
        assert_eq!(
            key("claude-3-5-sonnet-20241022").canonical,
            "claude-3-5-sonnet"
        );
        assert_eq!(key("gpt-4.1-2025-04-14").canonical, "gpt-4.1");
    }

    #[test]
    fn family_drops_a_minor_version_but_keeps_a_major() {
        // The case that has no price row: claude-opus-4-6 must reach claude-opus-4.
        assert_eq!(key("claude-opus-4-6").family, "claude-opus-4");
        assert_eq!(key("claude-opus-4.6").family, "claude-opus-4");
        assert_eq!(key("claude-sonnet-4-5").family, "claude-sonnet-4");
    }

    #[test]
    fn family_leaves_names_that_have_no_minor_version_alone() {
        // Reducing these would match the wrong row, or nothing at all.
        assert_eq!(key("claude-sonnet-4").family, "claude-sonnet-4");
        assert_eq!(key("claude-3-5-sonnet").family, "claude-3-5-sonnet");
        assert_eq!(key("gpt-4o-mini").family, "gpt-4o-mini");
        assert_eq!(key("gpt-6-astra").family, "gpt-6-astra");
    }

    #[test]
    fn vendor_is_inferred_from_the_name_not_the_host() {
        // A Claude served through Bedrock is still priced like a Claude.
        assert_eq!(
            resolve_model(Some("aws-bedrock"), "us.anthropic.claude-opus-4-v1:0").vendor,
            Vendor::Anthropic
        );
        assert_eq!(key("claude-opus-4-6").vendor, Vendor::Anthropic);
        assert_eq!(key("gpt-4o-mini").vendor, Vendor::OpenAi);
        assert_eq!(key("deepseek-chat").vendor, Vendor::DeepSeek);
        assert_eq!(key("glm-5.3").vendor, Vendor::Unknown);
    }

    #[test]
    fn the_reported_name_is_preserved_verbatim() {
        let resolved = key("Anthropic/Claude-Opus-4.6:batch");
        assert_eq!(resolved.model, "Anthropic/Claude-Opus-4.6:batch");
        assert_eq!(resolved.canonical, "claude-opus-4.6");
    }

    #[test]
    fn an_unrecognized_name_still_resolves() {
        let resolved = key("some-internal-model");
        assert_eq!(resolved.canonical, "some-internal-model");
        assert_eq!(resolved.family, "some-internal-model");
        assert_eq!(resolved.vendor, Vendor::Unknown);
    }
}
