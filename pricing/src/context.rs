//! Optional evidence affecting a call's price without changing its four token totals.

#[derive(Debug, Clone, Copy, Default)]
pub struct PricingContext<'a> {
    pub cache_creation_5m: Option<u64>,
    pub cache_creation_1h: Option<u64>,
    pub speed: Option<&'a str>,
    pub service_tier: Option<&'a str>,
    pub inference_geo: Option<&'a str>,
    pub conflicting_observations: bool,
}

pub(crate) fn context_cost(
    usage: &crate::NormalizedUsage,
    quote: &crate::PriceQuote,
    key: &crate::ModelKey,
    context: PricingContext<'_>,
) -> crate::CostBreakdown {
    let mut result = crate::cost(usage, quote);
    let has_ttl = context.cache_creation_5m.is_some() || context.cache_creation_1h.is_some();
    let five = context.cache_creation_5m.unwrap_or(0);
    let hour = context.cache_creation_1h.unwrap_or(0);
    let known = five.checked_add(hour);
    let direct = key.provider.as_deref() == Some("anthropic");
    let anthropic_model = key.vendor == crate::Vendor::Anthropic;
    let round = |v: f64| (v * 1_000_000.0).round() / 1_000_000.0;
    if has_ttl && known.is_some_and(|n| n <= usage.cache_creation) && anthropic_model {
        let remainder = usage.cache_creation - known.unwrap_or(0);
        let hour_rate = quote
            .cache_creation_1h_per_1m
            .unwrap_or(quote.input_per_1m * 2.0);
        result.estimated |= hour > 0 && quote.cache_creation_1h_per_1m.is_none();
        result.cache_creation_usd = round(
            (five as f64 * quote.cache_creation_per_1m
                + hour as f64 * hour_rate
                + remainder as f64 * quote.cache_creation_per_1m)
                / 1_000_000.0,
        );
        result.estimated |= !direct || remainder > 0;
    } else if has_ttl || (anthropic_model && usage.cache_creation > 0) {
        result.estimated = true;
    }
    let mut multiplier = 1.0;
    match context.speed.filter(|s| !s.is_empty()) {
        None | Some("standard") => {}
        Some("fast")
            if direct && matches!(key.canonical.as_str(), "claude-opus-5" | "claude-opus-4-8") =>
        {
            multiplier *= 2.0
        }
        Some(_) => result.estimated = true,
    }
    match context.inference_geo.filter(|s| !s.is_empty()) {
        None | Some("global") => {}
        Some("us") if direct => multiplier *= 1.1,
        Some(_) => result.estimated = true,
    }
    if context
        .service_tier
        .is_some_and(|s| !matches!(s, "" | "standard"))
    {
        result.estimated = true;
    }
    result.input_usd = round(result.input_usd * multiplier);
    result.output_usd = round(result.output_usd * multiplier);
    result.cache_read_usd = round(result.cache_read_usd * multiplier);
    result.cache_creation_usd = round(result.cache_creation_usd * multiplier);
    result.total_usd = round(
        result.input_usd + result.output_usd + result.cache_read_usd + result.cache_creation_usd,
    );
    result.estimated |= context.conflicting_observations || key.provider.is_none();
    if usage.is_empty() {
        result.estimated = false;
    }
    result
}
