use nasiko_orchestrator::AgentSelector;
use nasiko_orchestrator::models::{AgentCardSummary, SkillSummary};
use nasiko_orchestrator::providers::LLMProvider;
use uuid::Uuid;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn dummy_agent(name: &str, desc: &str, skills: Vec<SkillSummary>) -> AgentCardSummary {
    AgentCardSummary {
        id: Uuid::new_v4(),
        name: name.to_string(),
        description: desc.to_string(),
        skills,
        tags: vec!["test".to_string()],
    }
}

fn make_selector() -> AgentSelector {
    AgentSelector::new(
        LLMProvider::from_env(reqwest::Client::new()),
        "test-model".to_string(),
    )
}

// ── AgentSelector construction ────────────────────────────────────────────────

#[test]
fn agent_selector_new_does_not_panic() {
    let _ = make_selector();
}

#[test]
fn agent_selector_reports_model_name() {
    let selector = AgentSelector::new(
        LLMProvider::from_env(reqwest::Client::new()),
        "gpt-4o-mini".to_string(),
    );
    assert_eq!(selector.model_name(), "gpt-4o-mini");
}

// ── AgentCardSummary construction and serialization ───────────────────────────

#[test]
fn agent_card_summary_constructs_with_all_fields() {
    let id = Uuid::new_v4();
    let summary = AgentCardSummary {
        id,
        name: "code-agent".to_string(),
        description: "Writes code".to_string(),
        skills: vec![SkillSummary {
            name: "rust".to_string(),
            description: "Rust programming".to_string(),
            examples: Vec::new(),
        }],
        tags: vec!["engineering".to_string()],
    };
    assert_eq!(summary.id, id);
    assert_eq!(summary.skills.len(), 1);
}

#[test]
fn agent_card_summary_round_trips_through_json() {
    let summary = AgentCardSummary {
        id: Uuid::new_v4(),
        name: "agent".to_string(),
        description: "desc".to_string(),
        skills: vec![SkillSummary {
            name: "s1".to_string(),
            description: "d1".to_string(),
            examples: Vec::new(),
        }],
        tags: vec!["t1".to_string()],
    };
    let json = serde_json::to_string(&summary).unwrap();
    let restored: AgentCardSummary = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.name, summary.name);
    assert_eq!(restored.skills[0].name, "s1");
}

#[test]
fn agent_card_summary_with_empty_skills() {
    let summary = dummy_agent("bare-agent", "Does stuff", vec![]);
    assert!(summary.skills.is_empty());
    assert_eq!(summary.description, "Does stuff");
}

// ── SkillSummary ──────────────────────────────────────────────────────────────

#[test]
fn skill_summary_constructs() {
    let s = SkillSummary {
        name: "code-review".to_string(),
        description: "Reviews code for bugs".to_string(),
        examples: Vec::new(),
    };
    assert_eq!(s.name, "code-review");
    assert_eq!(s.description, "Reviews code for bugs");
}

#[test]
fn skill_summary_round_trips_through_json() {
    let original = SkillSummary {
        name: "summarize".to_string(),
        description: "Summarizes long documents".to_string(),
        examples: Vec::new(),
    };
    let json = serde_json::to_string(&original).unwrap();
    let restored: SkillSummary = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.name, original.name);
    assert_eq!(restored.description, original.description);
}

// ── select_agent: no agents → error ──────────────────────────────────────────

#[tokio::test]
async fn select_agent_with_empty_list_returns_error() {
    let selector = make_selector();
    let result = selector.select_agent("some query", &[], &[], None).await;
    assert!(
        result.is_err(),
        "select_agent should return Err when no agents provided"
    );
}

// ── select_agent: hallucinated agent_id ────────────────────────────────────────
// Regression for a mislabeled fallback: when the model names an agent_id that
// doesn't exist in the candidate list, select_agent substitutes the first real
// candidate but was returning `fallback_used = false` for it — indistinguishable
// in router_logs from a genuine, confident, non-fallback pick. The returned
// `bool` must be `true` for this path.

#[tokio::test]
async fn select_agent_flags_hallucinated_id_as_fallback_used() {
    let mut server = mockito::Server::new_async().await;
    let ghost_id = Uuid::new_v4();
    let real_agent = dummy_agent("real-agent", "Does real things", vec![]);

    let selection_json = serde_json::json!({
        "agent_id": ghost_id,
        "agent_name": "ghost-agent",
        "reasoning": "looks like a great fit",
        "confidence": 90,
    })
    .to_string();

    let body = serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": selection_json },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 },
    })
    .to_string();

    server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body)
        .create_async()
        .await;

    let provider = LLMProvider::new(reqwest::Client::new(), "sk-test".to_string(), server.url());
    let selector = AgentSelector::new(provider, "test-model".to_string());

    let (selection, _usage, hallucinated_fallback) = selector
        .select_agent("do the thing", &[], std::slice::from_ref(&real_agent), None)
        .await
        .expect("a hallucinated agent_id should resolve via fallback, not error");

    assert!(
        hallucinated_fallback,
        "a hallucinated agent_id must be flagged as a fallback"
    );
    assert_eq!(
        selection.agent_id, real_agent.id,
        "should substitute the first real candidate"
    );
    assert_ne!(selection.agent_id, ghost_id);
}

// ── select_agent: live LLM tests ──────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires live OpenAI-compatible LLM API"]
async fn select_agent_with_live_llm_returns_valid_selection() {
    let api_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY required");
    let base_url =
        std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com".into());

    let provider = LLMProvider::new(reqwest::Client::new(), api_key, base_url);
    let selector = AgentSelector::new(provider, "gpt-4o-mini".to_string());

    let agents = vec![
        AgentCardSummary {
            id: Uuid::new_v4(),
            name: "coding-agent".to_string(),
            description: "Writes and reviews Rust code".to_string(),
            skills: vec![SkillSummary {
                name: "rust".to_string(),
                description: "Rust programming".to_string(),
                examples: Vec::new(),
            }],
            tags: vec!["engineering".to_string()],
        },
        AgentCardSummary {
            id: Uuid::new_v4(),
            name: "finance-agent".to_string(),
            description: "Analyzes stock prices and crypto markets".to_string(),
            skills: vec![SkillSummary {
                name: "trading".to_string(),
                description: "Financial analysis".to_string(),
                examples: Vec::new(),
            }],
            tags: vec!["finance".to_string()],
        },
    ];

    let result = selector
        .select_agent("write a Rust function", &[], &agents, None)
        .await;
    assert!(result.is_ok(), "expected Ok, got {result:?}");
    let (selection, _usage, _hallucinated_fallback) = result.unwrap();
    assert!(!selection.reasoning.is_empty());
}

#[tokio::test]
#[ignore = "requires live DB"]
async fn fetch_active_agents_from_db() {
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/nasiko".to_string());
    let pool = sqlx::PgPool::connect(&db_url).await.unwrap();
    let result = AgentSelector::fetch_active_agents(&pool).await;
    assert!(result.is_ok());
}
