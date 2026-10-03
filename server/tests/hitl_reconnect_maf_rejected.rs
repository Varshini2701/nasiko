//! Regression test: reconnecting (`metadata.reconnect_after_hitl_id`) with a MAF-origin id must
//! fail cleanly (`400`), not hang. `deliver_maf()` (`oss/server/src/hitl/mod.rs`) hands off to the
//! MAF worker over Redis and never touches the continuation registry — without the explicit
//! rejection in `a2a_dispatch.rs::reconnect_stream`, this would `watch()` a buffer nothing ever
//! appends to or terminates, leaving the connection open forever with no error.
//!
//! `just infra` then `cargo test -p nasiko-server --test hitl_reconnect_maf_rejected -- --test-threads=1`

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
    .bind(format!("maf-reject-agent-{}", Uuid::new_v4()))
    .bind(owner_id)
    .fetch_one(&server.db)
    .await
    .expect("seed_running_agent")
}

/// `maf_id` is nullable (`ON DELETE SET NULL`) — no real workflow needed for this test, same
/// pattern `maf_hitl_discovery.rs::seed_execution` already uses.
async fn seed_maf_execution(server: &common::TestServer, user_id: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO maf_executions (user_id, status) VALUES ($1, 'running') RETURNING id",
    )
    .bind(user_id)
    .fetch_one(&server.db)
    .await
    .expect("seed_execution")
}

async fn seed_maf_hitl(
    server: &common::TestServer,
    agent_id: Uuid,
    owner_user_id: Uuid,
    execution_id: Uuid,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO hitl_requests
            (kind, origin, agent_id, owner_user_id, task_id, context_id, maf_execution_id, maf_step_index, question, expires_at)
         VALUES ('input_required', 'maf', $1, $2, $3, $4, $5, 1, $6, now() + interval '1 day')
         RETURNING id",
    )
    .bind(agent_id)
    .bind(owner_user_id)
    .bind(format!("task-{}", Uuid::new_v4()))
    .bind(format!("ctx-{}", Uuid::new_v4()))
    .bind(execution_id)
    .bind(json!({"message": "which step?"}))
    .fetch_one(&server.db)
    .await
    .expect("seed_maf_hitl")
}

#[tokio::test]
#[serial]
async fn reconnect_with_a_maf_origin_id_fails_cleanly_instead_of_hanging() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;
    let agent_id = seed_running_agent(&server, user_id).await;
    let execution_id = seed_maf_execution(&server, user_id).await;
    let hitl_id = seed_maf_hitl(&server, agent_id, user_id, execution_id).await;

    let reconnect_body = json!({
        "jsonrpc": "2.0", "id": Uuid::new_v4().to_string(), "method": "message/stream",
        "params": {
            "message": {"messageId": Uuid::new_v4().to_string(), "role": "ROLE_USER", "parts": []},
            "metadata": {"reconnect_after_hitl_id": hitl_id.to_string()},
        },
    });

    let res = tokio::time::timeout(
        Duration::from_secs(5),
        auth(
            server
                .client
                .post(server.url("/api/orchestrator/a2a"))
                .json(&reconnect_body),
            user_id,
        )
        .send(),
    )
    .await
    .expect("the request itself must return promptly, not hang")
    .unwrap();

    assert_eq!(
        res.status(),
        400,
        "a MAF-origin reconnect must fail cleanly with 400, not hang or succeed"
    );
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("MAF"),
        "error message should explain why: {body}"
    );

    server.cleanup().await;
}
