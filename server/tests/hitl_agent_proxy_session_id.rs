//! Regression: an `agent_proxy`-origin pause must carry the `chat_session_id` the proxy just
//! created for it.
//!
//! `persist_direct_chat_pause` computes and FK-validates that id for every origin, then applied it
//! on only the `direct_chat` arm — the `AgentProxy` arm dropped it, even though `agent_proxy.rs`
//! genuinely passes `Some(&session_id)`. Two things broke silently: the row missed
//! `list_for_chat_session`, so a proxy pause never appeared on session load; and `deliver`'s
//! `session_traces` insert is guarded on this being `Some`, so every MCP tool-approval retry after
//! the resume resolved to a fresh trace id and re-asked the human to approve the same tool.
//!
//! Drives the real `/api/agents/{id}/` proxy against a stub agent that answers with a
//! non-streaming paused reply, rather than calling the persist helper directly — the arm that
//! dropped the value is only reachable through that path.
//!
//! `just infra` then
//! `cargo test -p nasiko-server --test hitl_agent_proxy_session_id -- --test-threads=1`

mod common;

use std::time::Duration;

use axum::{Router, routing::post};
use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

/// A2A contextId supplied explicitly, so `ensure_chat_session` upserts a `chat_sessions` row under
/// exactly this id rather than generating a `ses_*` one — the assertion below can then compare a
/// known value instead of merely "not null".
const CONTEXT_ID: &str = "ses_agent_proxy_pause_session_id";

async fn init_admin(server: &common::TestServer) -> (String, Uuid) {
    let v: Value = server
        .client
        .post(server.url("/api/auth/initialize-admin"))
        .json(&json!({"username": "admin", "email": "admin@test.local"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = v["user_id"].as_str().unwrap().to_string();
    (id.clone(), Uuid::parse_str(&id).unwrap())
}

/// Answers every A2A call with a non-streaming `input-required` reply. `application/json`, not
/// `text/event-stream`, so the proxy takes the non-streaming branch that persists the pause.
async fn start_paused_stub_agent() -> String {
    let app = Router::new().route(
        "/",
        post(|| async {
            axum::response::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": "1",
                        "result": {"statusUpdate": {"status": {
                            "state": "input-required",
                            "message": {"role": "agent", "parts": [{"text": "which repo?"}]}
                        }}}
                    })
                    .to_string(),
                ))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn seed_running_agent(server: &common::TestServer, owner_id: Uuid, url: &str) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO agents (name, owner_id, image, status, url, is_public) \
         VALUES ($1, $2, 'x:1.0.0', 'running', $3, false) RETURNING id",
    )
    .bind(format!("proxy-pause-agent-{}", Uuid::new_v4()))
    .bind(owner_id)
    .bind(url)
    .fetch_one(&server.db)
    .await
    .unwrap()
}

/// The pause row is written from a spawned task, so poll rather than assume it has landed.
async fn wait_for_pause_row(
    server: &common::TestServer,
    agent_id: Uuid,
) -> (String, Option<String>) {
    for _ in 0..60 {
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT origin, chat_session_id FROM hitl_requests WHERE agent_id = $1")
                .bind(agent_id)
                .fetch_optional(&server.db)
                .await
                .unwrap();
        if let Some(row) = row {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no hitl_requests row was ever created for agent {agent_id}");
}

#[tokio::test]
#[serial]
async fn an_agent_proxy_pause_carries_its_chat_session_id() {
    let server = common::TestServer::start().await;
    let (admin_id, admin_uuid) = init_admin(&server).await;

    let stub_url = start_paused_stub_agent().await;
    let agent_id = seed_running_agent(&server, admin_uuid, &stub_url).await;

    let res = common::as_superuser(
        server
            .client
            // No trailing slash: the chat call is the dedicated `POST /agents/{id}` mount
            // (`lib.rs`), not the `/{*rest}` wildcard next to it.
            .post(server.url(&format!("/api/agents/{agent_id}"))),
        &admin_id,
        "admin",
    )
    .json(&json!({
        "jsonrpc": "2.0",
        "id": "1",
        "method": "message/send",
        "params": {"message": {
            "messageId": Uuid::new_v4().to_string(),
            "contextId": CONTEXT_ID,
            "role": "ROLE_USER",
            "parts": [{"text": "list my repos"}]
        }}
    }))
    .send()
    .await
    .unwrap();
    assert!(
        res.status().is_success(),
        "the proxied call itself must succeed: {}",
        res.status()
    );
    let _ = res.bytes().await;

    let (origin, chat_session_id) = wait_for_pause_row(&server, agent_id).await;
    assert_eq!(
        origin, "agent_proxy",
        "precondition: this path must produce an agent_proxy-origin row"
    );
    assert_eq!(
        chat_session_id.as_deref(),
        Some(CONTEXT_ID),
        "an agent_proxy pause must carry the chat session the proxy created for it — without it \
         the row is invisible to list_for_chat_session and the resume skips its session_traces \
         insert, re-asking the human to approve the same tool forever"
    );

    // The id must name a session that actually exists: `persist_direct_chat_pause` FK-validates it
    // before binding, so a bogus value would be silently dropped to NULL rather than rejected.
    let session_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM chat_sessions WHERE session_id = $1 AND user_id = $2)",
    )
    .bind(CONTEXT_ID)
    .bind(admin_uuid)
    .fetch_one(&server.db)
    .await
    .unwrap();
    assert!(
        session_exists,
        "the recorded chat_session_id must reference a real session row"
    );

    server.cleanup().await;
}
