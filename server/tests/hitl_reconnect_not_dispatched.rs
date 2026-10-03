//! Regression coverage for reconnect's "nothing was ever dispatched for this id" hang: before this
//! fix, `reconnect_stream` (`oss/server/src/router/a2a_dispatch.rs`) authorized the row and called
//! `watch(row.id)` unconditionally — for a still-`pending` row, or an unmirrored `mcp_tool` row
//! (one whose `owner_user_id` never gets an alias into `continuation_events` at all, since neither
//! `deliver()` nor the dedicated `mcp_tool` dispatcher ever touches it for a standalone row), that
//! created a fresh, permanently-non-terminal buffer nothing would ever finish — the connection
//! hung forever and leaked a registry entry per attempt.
//!
//! `just infra` then `cargo test -p nasiko-server --test hitl_reconnect_not_dispatched -- --test-threads=1`

mod common;

use std::time::Duration;

use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

fn auth(rb: reqwest::RequestBuilder, user_id: Uuid) -> reqwest::RequestBuilder {
    let user_id_str = user_id.to_string();
    common::as_member(rb, &user_id_str, &format!("user_{}", &user_id_str[..8]))
}

async fn seed_user(server: &common::TestServer, user_id: Uuid) {
    sqlx::query(
        "INSERT INTO users (id, username, email) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(user_id)
    .bind(format!("user_{}", &user_id.to_string()[..8]))
    .bind(format!("user_{}@test.example", &user_id.to_string()[..8]))
    .execute(&server.db)
    .await
    .expect("seed_user");
}

async fn seed_running_agent(server: &common::TestServer, owner_id: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO agents (name, owner_id, url, status) VALUES ($1, $2, 'http://127.0.0.1:1', 'running') RETURNING id",
    )
    .bind(format!("not-dispatched-agent-{}", Uuid::new_v4()))
    .bind(owner_id)
    .fetch_one(&server.db)
    .await
    .expect("seed_running_agent")
}

async fn reconnect(server: &common::TestServer, user_id: Uuid, hitl_id: Uuid) -> reqwest::Response {
    let body = json!({
        "jsonrpc": "2.0", "id": Uuid::new_v4().to_string(), "method": "message/stream",
        "params": {
            "message": {"messageId": Uuid::new_v4().to_string(), "role": "ROLE_USER", "parts": []},
            "metadata": {"reconnect_after_hitl_id": hitl_id.to_string()},
        },
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        auth(
            server
                .client
                .post(server.url("/api/orchestrator/a2a"))
                .json(&body),
            user_id,
        )
        .send(),
    )
    .await
    .expect("reconnect request itself must not hang")
    .expect("reconnect request must complete")
}

#[tokio::test]
#[serial]
async fn reconnecting_on_a_still_pending_row_fails_cleanly_instead_of_hanging() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;
    let agent_id = seed_running_agent(&server, user_id).await;

    let hitl_id: Uuid = sqlx::query_scalar(
        "INSERT INTO hitl_requests
            (kind, origin, agent_id, owner_user_id, task_id, context_id, question)
         VALUES ('input_required', 'direct_chat', $1, $2, $3, $4, $5)
         RETURNING id",
    )
    .bind(agent_id)
    .bind(user_id)
    .bind(format!("task-{}", Uuid::new_v4()))
    .bind(format!("ctx-{}", Uuid::new_v4()))
    .bind(json!({"message": "which repo?"}))
    .fetch_one(&server.db)
    .await
    .expect("seed pending row");

    let res = reconnect(&server, user_id, hitl_id).await;
    assert_eq!(
        res.status(),
        400,
        "reconnecting on a still-pending row must fail cleanly, not hang"
    );
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not been resolved"),
        "error message should explain why: {body}"
    );

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn reconnecting_on_a_resolved_unmirrored_mcp_tool_row_fails_cleanly_instead_of_hanging() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;
    let agent_id = seed_running_agent(&server, user_id).await;

    // Resolved, but never mirrored into any direct_chat/agent_proxy/maf/orchestrator pause — a
    // raw MCP integration outside any chat, the ordinary case for most tool_approval rows.
    let hitl_id: Uuid = sqlx::query_scalar(
        "INSERT INTO hitl_requests
            (kind, origin, agent_id, owner_user_id, context_id, connector_id, tool_name, question, status, resolved_at)
         VALUES ('tool_approval', 'mcp_tool', $1, $2, $3, $4, $5, $6, 'resolved', now())
         RETURNING id",
    )
    .bind(agent_id)
    .bind(user_id)
    .bind(format!("mcp-ctx-{}", Uuid::new_v4()))
    .bind(Uuid::new_v4())
    .bind("github_create_issue")
    .bind(json!({"message": "Approve creating a GitHub issue?"}))
    .fetch_one(&server.db)
    .await
    .expect("seed resolved, unmirrored mcp_tool row");

    let res = reconnect(&server, user_id, hitl_id).await;
    assert_eq!(
        res.status(),
        400,
        "reconnecting on a resolved-but-never-mirrored mcp_tool row must fail cleanly, not hang"
    );
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no agent-visible continuation"),
        "error message should explain why: {body}"
    );

    server.cleanup().await;
}
