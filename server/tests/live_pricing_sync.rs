//! Opt-in local pricing-sync verification. Applies pending OSS migrations and
//! syncs the Anthropic reference book; never reprices stored usage/history.
use nasiko_llm_router::routing::pricing_sync::sync_provider_prices;
use nasiko_pricing::{PricingContext, PricingEngine, PromptConvention, RawUsage};

#[tokio::test]
#[ignore = "mutates local price book and applies migrations; requires explicit approval"]
async fn sync_anthropic_and_verify_db_first_hourly_rate() {
    let url = std::env::var("DATABASE_URL").expect("explicit local database URL");
    let parsed = reqwest::Url::parse(&url).unwrap();
    assert!(
        matches!(parsed.host_str(), Some("localhost" | "127.0.0.1")),
        "local verification only"
    );
    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
        .connect(&url)
        .await
        .unwrap();
    let mut migrator = sqlx::migrate!("../migrations");
    // The local EE server also applies enterprise migrations to this database.
    migrator.set_ignore_missing(true);
    migrator.run(&db).await.unwrap();
    let http = reqwest::Client::new();
    let inserted = sync_provider_prices(
        &db,
        &http,
        "anthropic",
        "https://api.anthropic.com",
        "https://configs.portkey.ai",
    )
    .await
    .unwrap();
    let repeat = sync_provider_prices(
        &db,
        &http,
        "anthropic",
        "https://api.anthropic.com",
        "https://configs.portkey.ai",
    )
    .await
    .unwrap();
    assert_eq!(repeat, 0, "same price book must not create new history");
    let engine = PricingEngine::new(db.clone());
    for model in ["claude-opus-5", "claude-opus-4-6"] {
        let result = engine
            .price_with_context(
                Some("anthropic"),
                model,
                RawUsage {
                    input: 1_000_000,
                    output: 1_000_000,
                    cache_read: 1_000_000,
                    cache_creation: 2_000_000,
                    total: None,
                },
                PromptConvention::Exclusive,
                chrono::Utc::now(),
                PricingContext {
                    cache_creation_5m: Some(1_000_000),
                    cache_creation_1h: Some(1_000_000),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(result.quote.source, nasiko_pricing::PriceSource::Exact);
        assert_eq!(result.quote.cache_creation_1h_per_1m, Some(10.0));
        assert_eq!(result.cost.cache_creation_usd, 16.25);
        assert_eq!(result.cost.total_usd, 46.75);
        assert!(!result.cost.estimated);
        println!(
            "{model}: DB rates input={} output={} read={} write5m={} write1h={:?}; mixed-TTL cost={}",
            result.quote.input_per_1m,
            result.quote.output_per_1m,
            result.quote.cache_read_per_1m,
            result.quote.cache_creation_per_1m,
            result.quote.cache_creation_1h_per_1m,
            result.cost.total_usd
        );
    }
    println!("sync rows inserted={inserted}; repeat={repeat}");
    db.close().await;
}
