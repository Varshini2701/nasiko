//! Regression test for the direct agent-proxy path's `chat_messages` persistence
//! (`oss/server/src/agent_proxy.rs`): `nasiko chat`'s protocol-negotiation retry (method/role
//! mismatches some agent SDKs reject) resends one logical message as a second, separate HTTP
//! request under the same `contextId`, milliseconds later. Without a dedup guard, that showed up
//! as the same user message persisted twice — visible as a duplicate bubble in both the CLI and
//! the web UI's session history.
//!
//! Requires infra (Postgres :5432, Redis, S3):
//!   cargo test -p nasiko-server --test agent_proxy_message_dedup -- --test-threads=1

mod common;

use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

async fn init_admin(server: &common::TestServer) -> Uuid {
    let v = server
        .client
        .post(server.url("/api/auth/initialize-admin"))
        .json(&json!({"username": "admin", "email": "admin@test.local"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    Uuid::parse_str(v["user_id"].as_str().unwrap()).unwrap()
}

async fn seed_running_agent(
    server: &common::TestServer,
    owner_id: Uuid,
    name: &str,
    url: &str,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO agents (name, owner_id, image, status, url, is_public) \
         VALUES ($1, $2, 'x:1.0.0', 'running', $3, false) RETURNING id",
    )
    .bind(name)
    .bind(owner_id)
    .bind(url)
    .fetch_one(&server.db)
    .await
    .unwrap()
}

/// Stub agent: answers any A2A call with a flat, non-streaming message result — enough for
/// `agent_proxy.rs`'s forwarding to succeed; this test only cares about what gets persisted on
/// the way in, not the agent's own reply.
async fn start_stub_agent() -> String {
    let a2a = post(|| async {
        Json(json!({
            "jsonrpc": "2.0", "id": "1",
            "result": {"kind": "message", "parts": [{"kind": "text", "text": "ok"}]}
        }))
    });
    let app = Router::new().route("/", a2a);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

async fn send_message(
    server: &common::TestServer,
    owner: Uuid,
    agent: Uuid,
    context_id: &str,
    text: &str,
) {
    let body = json!({
        "jsonrpc": "2.0",
        "method": "message/stream",
        "id": Uuid::new_v4().to_string(),
        "params": {
            "message": {
                "messageId": Uuid::new_v4().to_string(),
                "role": "ROLE_USER",
                "contextId": context_id,
                "parts": [{ "text": text }]
            }
        }
    });
    let res = common::as_superuser(
        server
            .client
            .post(server.url(&format!("/api/agents/{agent}"))),
        &owner.to_string(),
        "admin",
    )
    .json(&body)
    .send()
    .await
    .unwrap();
    assert!(
        res.status().is_success(),
        "proxy call should succeed against the stub: {}",
        res.status()
    );
}

async fn user_message_count(server: &common::TestServer, session_id: &str, content: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_messages WHERE session_id = $1 AND role = 'user' AND content = $2",
    )
    .bind(session_id)
    .bind(content)
    .fetch_one(&server.db)
    .await
    .unwrap()
}

#[tokio::test]
#[serial]
async fn a_retry_of_the_same_message_within_the_dedup_window_is_not_persisted_twice() {
    let server = common::TestServer::start().await;
    let owner = init_admin(&server).await;
    let stub_url = start_stub_agent().await;
    let agent = seed_running_agent(&server, owner, "dedup-agent", &stub_url).await;

    let context_id = format!("ses_{}", Uuid::new_v4().simple());
    let text = "create a github issue in nasiko-bishnu/test";

    // Two requests under the same contextId with identical text, back-to-back — exactly what
    // the CLI's protocol-negotiation retry produces for one logical message.
    send_message(&server, owner, agent, &context_id, text).await;
    send_message(&server, owner, agent, &context_id, text).await;

    assert_eq!(
        user_message_count(&server, &context_id, text).await,
        1,
        "a same-text retry within the dedup window must persist only once"
    );

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a_genuinely_different_message_in_the_same_session_is_still_persisted() {
    let server = common::TestServer::start().await;
    let owner = init_admin(&server).await;
    let stub_url = start_stub_agent().await;
    let agent = seed_running_agent(&server, owner, "dedup-agent-2", &stub_url).await;

    let context_id = format!("ses_{}", Uuid::new_v4().simple());

    send_message(&server, owner, agent, &context_id, "first message").await;
    send_message(
        &server,
        owner,
        agent,
        &context_id,
        "second, different message",
    )
    .await;

    assert_eq!(
        user_message_count(&server, &context_id, "first message").await,
        1
    );
    assert_eq!(
        user_message_count(&server, &context_id, "second, different message").await,
        1,
        "the dedup guard must never suppress genuinely distinct messages in the same session"
    );

    server.cleanup().await;
}
