//! `titling::truncate_title` (pure) and `titling::title_from_prompt`, the latter
//! exercised indirectly through `POST /api/chat/sessions`'s `first_prompt` — a
//! just-extracted shared module, previously private to `chat::routes`.
//!
//! `title_from_prompt` builds its LLM provider via `LLMProvider::from_env`, which
//! reads `OPENAI_API_KEY`/`OPENAI_BASE_URL` straight from the process environment
//! (not `state.config`) — so pointing it at a mock server means mutating global
//! env vars. Every test that does this is `#[serial]`; cargo runs each integration
//! test file as its own process, so this only has to coordinate with the other
//! tests in *this* file.
//!
//! Requires infra (Postgres :5432, Redis, S3):
//!   cargo test -p nasiko-server --test titling -- --test-threads=1

mod common;

use nasiko_server::titling::{MAX_TITLE_CHARS, truncate_title};
use serde_json::{Value, json};
use serial_test::serial;

// ─── truncate_title: pure, no infra needed ──────────────────────────────────

#[test]
fn short_ascii_is_untouched() {
    assert_eq!(truncate_title("hello world"), "hello world");
}

#[test]
fn empty_and_whitespace_only_become_empty() {
    assert_eq!(truncate_title(""), "");
    assert_eq!(truncate_title("   "), "");
    assert_eq!(truncate_title("\t\n  \t"), "");
}

#[test]
fn exactly_at_the_cap_is_untouched() {
    let s: String = "a".repeat(MAX_TITLE_CHARS);
    assert_eq!(truncate_title(&s).chars().count(), MAX_TITLE_CHARS);
    assert_eq!(truncate_title(&s), s);
}

#[test]
fn one_over_the_cap_truncates_by_exactly_one_char() {
    let s: String = "a".repeat(MAX_TITLE_CHARS + 1);
    let out = truncate_title(&s);
    assert_eq!(out.chars().count(), MAX_TITLE_CHARS);
}

#[test]
fn truncation_trims_trailing_whitespace_left_at_the_cut() {
    // 79 'a's + a space + more 'b's: the cut at char 80 lands right after the
    // space, and trim_end must eat it rather than leaving a dangling space.
    let s = format!("{} {}", "a".repeat(79), "b".repeat(20));
    let out = truncate_title(&s);
    assert!(
        !out.ends_with(' '),
        "must not leave a trailing space at the cut: {out:?}"
    );
    assert_eq!(out.chars().count(), 79);
}

#[test]
fn multibyte_codepoints_never_panic_and_stay_on_a_char_boundary() {
    // Emoji (4-byte UTF-8) repeated past the cap. A byte-index truncation would
    // panic mid-codepoint; char_indices is what titling.rs actually uses.
    let s: String = "🎉".repeat(100);
    let out = truncate_title(&s);
    assert_eq!(out.chars().count(), MAX_TITLE_CHARS);
    assert!(out.chars().all(|c| c == '🎉'));
}

#[test]
fn cjk_multibyte_truncates_by_character_count_not_byte_count() {
    let s: String = "漢".repeat(200);
    let out = truncate_title(&s);
    assert_eq!(out.chars().count(), MAX_TITLE_CHARS);
    assert_eq!(out.len(), MAX_TITLE_CHARS * "漢".len());
}

#[test]
fn leading_and_trailing_whitespace_is_trimmed_before_the_cap_is_applied() {
    let padded = format!("  {}  ", "x".repeat(90));
    let out = truncate_title(&padded);
    assert_eq!(out.chars().count(), MAX_TITLE_CHARS);
    assert!(!out.starts_with(' '));
}

// ─── title_from_prompt via POST /api/chat/sessions{first_prompt} ────────────

/// Point the process-global OpenAI client at `base_url`. `#[serial]`d callers only.
fn point_llm_provider_at(base_url: &str) {
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "test-key");
        std::env::set_var("OPENAI_BASE_URL", base_url);
    }
}

fn chat_completion_body(content: &str) -> serde_json::Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 0,
        "model": "gpt-4o-mini",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
    })
}

async fn init_admin(server: &common::TestServer) -> Value {
    server
        .client
        .post(server.url("/api/auth/initialize-admin"))
        .json(&json!({"username": "admin", "email": "admin@test.local"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()
}

async fn create_session_with_prompt(
    server: &common::TestServer,
    uid: &str,
    first_prompt: &str,
) -> Value {
    let body = common::as_superuser(
        server.client.post(server.url("/api/chat/sessions")),
        uid,
        "admin",
    )
    .json(&json!({ "first_prompt": first_prompt }))
    .send()
    .await
    .unwrap()
    .json::<Value>()
    .await
    .unwrap();
    body["data"].clone()
}

#[tokio::test]
#[serial]
async fn no_first_prompt_still_defaults_to_new_chat_not_the_shared_fn() {
    // Regression guard for the extraction itself: the "New chat" default must
    // still live in routes.rs (title_from_prompt has no empty-prompt special
    // case), so omitting first_prompt entirely must not touch the LLM at all.
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();
    // Deliberately no OPENAI_BASE_URL override — if this path called the LLM,
    // it would hit the real api.openai.com with a bogus key and fail/hang.
    let created = common::as_superuser(
        server.client.post(server.url("/api/chat/sessions")),
        uid,
        "admin",
    )
    .json(&json!({}))
    .send()
    .await
    .unwrap()
    .json::<Value>()
    .await
    .unwrap();
    assert_eq!(created["data"]["title"], "New chat");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn whitespace_only_first_prompt_also_defaults_to_new_chat() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();
    let created = create_session_with_prompt(&server, uid, "   \t  ").await;
    assert_eq!(created["title"], "New chat");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_clean_llm_title_is_used_verbatim() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let mut mock = mockito::Server::new_async().await;
    let _m = mock
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(chat_completion_body("Deploy Failure Debugging").to_string())
        .create_async()
        .await;
    point_llm_provider_at(&mock.url());

    let created = create_session_with_prompt(&server, uid, "help me debug my deploy").await;
    assert_eq!(created["title"], "Deploy Failure Debugging");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_single_layer_of_surrounding_quotes_is_stripped() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let mut mock = mockito::Server::new_async().await;
    let _m = mock
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(chat_completion_body("\"Translation Assistance\"").to_string())
        .create_async()
        .await;
    point_llm_provider_at(&mock.url());

    let created = create_session_with_prompt(&server, uid, "translate this to spanish").await;
    assert_eq!(created["title"], "Translation Assistance");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_stray_unbalanced_quote_is_left_alone() {
    // strip_prefix('"').and_then(strip_suffix('"')) requires BOTH ends — a
    // model that only opens a quote must not have its lone quote eaten.
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let mut mock = mockito::Server::new_async().await;
    let _m = mock
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(chat_completion_body("\"Unbalanced Quote").to_string())
        .create_async()
        .await;
    point_llm_provider_at(&mock.url());

    let created = create_session_with_prompt(&server, uid, "anything").await;
    assert_eq!(created["title"], "\"Unbalanced Quote");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn an_empty_llm_response_falls_back_to_the_truncated_prompt() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let mut mock = mockito::Server::new_async().await;
    let _m = mock
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(chat_completion_body("").to_string())
        .create_async()
        .await;
    point_llm_provider_at(&mock.url());

    let created = create_session_with_prompt(&server, uid, "help me write a dockerfile").await;
    assert_eq!(created["title"], "help me write a dockerfile");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_whitespace_only_llm_response_falls_back_to_the_truncated_prompt() {
    // sanitize_title trims raw, then re-trims after the quote-strip, then hands
    // an empty string to truncate_title — the outer `!title.is_empty()` guard
    // in title_from_prompt is what must catch this rather than returning " ".
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let mut mock = mockito::Server::new_async().await;
    let _m = mock
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(chat_completion_body("   \n\t  ").to_string())
        .create_async()
        .await;
    point_llm_provider_at(&mock.url());

    let created = create_session_with_prompt(&server, uid, "explain container networking").await;
    assert_eq!(created["title"], "explain container networking");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_provider_5xx_falls_back_to_the_truncated_prompt_not_an_error() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let mut mock = mockito::Server::new_async().await;
    let _m = mock
        .mock("POST", "/v1/chat/completions")
        .with_status(500)
        .with_body("internal server error")
        .create_async()
        .await;
    point_llm_provider_at(&mock.url());

    let created = create_session_with_prompt(&server, uid, "generate a new agent").await;
    assert_eq!(created["title"], "generate a new agent");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_malformed_json_body_falls_back_to_the_truncated_prompt() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let mut mock = mockito::Server::new_async().await;
    let _m = mock
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("{not json")
        .create_async()
        .await;
    point_llm_provider_at(&mock.url());

    let created = create_session_with_prompt(&server, uid, "what needs my attention").await;
    assert_eq!(created["title"], "what needs my attention");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn an_unreachable_provider_falls_back_to_the_truncated_prompt() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    // Nothing is listening on this port.
    point_llm_provider_at("http://127.0.0.1:1");

    let created = create_session_with_prompt(&server, uid, "create a new agent").await;
    assert_eq!(created["title"], "create a new agent");
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_long_unicode_prompt_falls_back_to_a_char_boundary_safe_truncation() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let mut mock = mockito::Server::new_async().await;
    let _m = mock
        .mock("POST", "/v1/chat/completions")
        .with_status(500)
        .create_async()
        .await;
    point_llm_provider_at(&mock.url());

    let prompt: String = "漢字".repeat(60); // 120 chars, well over the 80 cap
    let created = create_session_with_prompt(&server, uid, &prompt).await;
    assert_eq!(
        created["title"].as_str().unwrap().chars().count(),
        MAX_TITLE_CHARS
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn the_configured_capability_model_and_call_shape_are_sent() {
    let server =
        common::TestServer::start_with(|c| c.capability_generator_model = "my-test-model".into())
            .await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let mut mock = mockito::Server::new_async().await;
    let expect = mock
        .mock("POST", "/v1/chat/completions")
        .match_body(mockito::Matcher::PartialJson(json!({
            "model": "my-test-model",
            "temperature": 0.2,
            "max_tokens": 16,
            "stream": false,
        })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(chat_completion_body("Whatever").to_string())
        .create_async()
        .await;
    point_llm_provider_at(&mock.url());

    let created = create_session_with_prompt(&server, uid, "anything").await;
    assert_eq!(created["title"], "Whatever");
    expect.assert_async().await;
    server.cleanup().await;
}
