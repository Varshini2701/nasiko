//! Tests for MAF's HITL discovery contract: `GET /api/maf/execution/{id}` and
//! `GET /api/maf/workflow/result/{exec_id}` now return a `hitl` array joined from
//! `hitl_requests` by `maf_execution_id`, so the frontend never has to call
//! `GET /api/hitl/pending` to correlate a paused step back to its HITL id.
//!
//! These tests seed `maf_executions`/`hitl_requests` rows directly rather than driving a live
//! worker through a real pause — the pause-detection and resume-dispatch machinery is
//! pre-existing and covered elsewhere (`bc563dbf`'s own test pass); what's new here is purely the
//! execution-response's join/shape/ownership, so that's what these isolate.
//!
//! Requires infra (Postgres, Redis) like the rest of the suite:
//!   `just infra` then `cargo test -p nasiko-server --test maf_hitl_discovery -- --test-threads=1`

mod common;

use serde_json::{Value, json};
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

async fn seed_agent(server: &common::TestServer, owner_id: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO agents (name, owner_id, url) VALUES ('MAF HITL Discovery Test Agent', $1, 'http://fake-agent.local/a2a') RETURNING id",
    )
    .bind(owner_id)
    .fetch_one(&server.db)
    .await
    .expect("seed_agent")
}

/// Seeds a `maf_executions` row directly — `maf_id` left NULL (nullable, `ON DELETE SET NULL`),
/// no real workflow needed for these response-shape tests.
async fn seed_execution(server: &common::TestServer, user_id: Uuid, status: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO maf_executions (user_id, status) VALUES ($1, $2) RETURNING id")
        .bind(user_id)
        .bind(status)
        .fetch_one(&server.db)
        .await
        .expect("seed_execution")
}

#[allow(clippy::too_many_arguments)]
async fn seed_hitl(
    server: &common::TestServer,
    agent_id: Uuid,
    owner_user_id: Uuid,
    maf_execution_id: Uuid,
    maf_step_index: i32,
    status: &str,
    question: &str,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO hitl_requests
            (kind, origin, status, agent_id, owner_user_id, task_id, context_id,
             maf_execution_id, maf_step_index, question, expires_at)
         VALUES ('input_required', 'maf', $1, $2, $3, $4, $5, $6, $7, $8, now() + interval '1 day')
         RETURNING id",
    )
    .bind(status)
    .bind(agent_id)
    .bind(owner_user_id)
    .bind(format!("task-{}", Uuid::new_v4()))
    .bind(maf_execution_id.to_string())
    .bind(maf_execution_id)
    .bind(maf_step_index)
    .bind(json!({ "message": question }))
    .fetch_one(&server.db)
    .await
    .expect("seed_hitl")
}

async fn get_execution(
    server: &common::TestServer,
    user_id: Uuid,
    execution_id: Uuid,
) -> (u16, Value) {
    let res = auth(
        server
            .client
            .get(server.url(&format!("/api/maf/execution/{execution_id}"))),
        user_id,
    )
    .send()
    .await
    .unwrap();
    let status = res.status().as_u16();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    (status, body)
}

/// The core contract: a `hitl_requests` row tied to an `awaiting_human` execution is visible
/// directly in the execution response, with the fields needed to resolve it — no separate
/// `/api/hitl/pending` call.
#[tokio::test]
#[serial]
async fn execution_response_surfaces_pending_hitl() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;
    let agent_id = seed_agent(&server, user_id).await;
    let exec_id = seed_execution(&server, user_id, "awaiting_human").await;
    let hitl_id = seed_hitl(
        &server,
        agent_id,
        user_id,
        exec_id,
        1,
        "pending",
        "Which venue?",
    )
    .await;

    let (status, body) = get_execution(&server, user_id, exec_id).await;
    assert_eq!(status, 200);
    assert_eq!(body["data"]["status"], "awaiting_human");

    let hitl = body["data"]["hitl"].as_array().expect("hitl array present");
    assert_eq!(hitl.len(), 1);
    assert_eq!(hitl[0]["id"], hitl_id.to_string());
    assert_eq!(hitl[0]["kind"], "input_required");
    assert_eq!(hitl[0]["status"], "pending");
    assert_eq!(hitl[0]["question"]["message"], "Which venue?");
    assert_eq!(
        hitl[0]["execution"]["maf_execution_id"],
        exec_id.to_string()
    );
    assert_eq!(hitl[0]["execution"]["maf_step_index"], 1);
    assert_eq!(hitl[0]["execution"]["agent_id"], agent_id.to_string());
    // Never leaked, on this endpoint any more than on GET /api/hitl/{id}.
    assert!(hitl[0].get("resume_state").is_none());

    server.cleanup().await;
}

/// A normal (non-paused) execution reports an empty array, not an error or a missing field.
#[tokio::test]
#[serial]
async fn execution_response_has_empty_hitl_when_none_pending() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;
    let exec_id = seed_execution(&server, user_id, "success").await;

    let (status, body) = get_execution(&server, user_id, exec_id).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["data"]["hitl"]
            .as_array()
            .expect("hitl array present")
            .len(),
        0
    );

    server.cleanup().await;
}

/// Two executions, each with their own HITL — fetching one must never surface the other's.
#[tokio::test]
#[serial]
async fn execution_hitl_correlation_does_not_cross_executions() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;
    let agent_id = seed_agent(&server, user_id).await;

    let exec_a = seed_execution(&server, user_id, "awaiting_human").await;
    let exec_b = seed_execution(&server, user_id, "awaiting_human").await;
    let hitl_a = seed_hitl(
        &server,
        agent_id,
        user_id,
        exec_a,
        0,
        "pending",
        "Question A",
    )
    .await;
    let hitl_b = seed_hitl(
        &server,
        agent_id,
        user_id,
        exec_b,
        0,
        "pending",
        "Question B",
    )
    .await;

    let (_, body_a) = get_execution(&server, user_id, exec_a).await;
    let hitl_in_a = body_a["data"]["hitl"].as_array().unwrap();
    assert_eq!(hitl_in_a.len(), 1);
    assert_eq!(hitl_in_a[0]["id"], hitl_a.to_string());
    assert_ne!(hitl_in_a[0]["id"], hitl_b.to_string());

    let (_, body_b) = get_execution(&server, user_id, exec_b).await;
    let hitl_in_b = body_b["data"]["hitl"].as_array().unwrap();
    assert_eq!(hitl_in_b.len(), 1);
    assert_eq!(hitl_in_b[0]["id"], hitl_b.to_string());

    server.cleanup().await;
}

/// A different user cannot reach another owner's execution (and therefore its HITL) at all —
/// the existing `row.user_id == user_id` gate on `get_execution` covers this, this test just
/// confirms adding the `hitl` join didn't weaken it.
#[tokio::test]
#[serial]
async fn execution_hitl_not_visible_to_other_user() {
    let server = common::TestServer::start().await;
    let owner_id = Uuid::new_v4();
    let other_id = Uuid::new_v4();
    seed_user(&server, owner_id).await;
    seed_user(&server, other_id).await;
    let agent_id = seed_agent(&server, owner_id).await;
    let exec_id = seed_execution(&server, owner_id, "awaiting_human").await;
    seed_hitl(
        &server,
        agent_id,
        owner_id,
        exec_id,
        0,
        "pending",
        "Owner's question",
    )
    .await;

    let (status, _) = get_execution(&server, other_id, exec_id).await;
    assert_eq!(status, 403);

    server.cleanup().await;
}

/// Sequential HITLs on the same execution: an earlier, already-resolved round stays visible as
/// history, and the currently-actionable one is unambiguously identifiable (exactly one
/// `status: "pending"` entry).
#[tokio::test]
#[serial]
async fn sequential_hitls_identify_the_current_pending_one() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;
    let agent_id = seed_agent(&server, user_id).await;
    let exec_id = seed_execution(&server, user_id, "awaiting_human").await;

    let first = seed_hitl(
        &server,
        agent_id,
        user_id,
        exec_id,
        0,
        "resolved",
        "Which movie?",
    )
    .await;
    let second = seed_hitl(
        &server,
        agent_id,
        user_id,
        exec_id,
        1,
        "pending",
        "Which venue?",
    )
    .await;

    let (_, body) = get_execution(&server, user_id, exec_id).await;
    let hitl = body["data"]["hitl"].as_array().unwrap();
    assert_eq!(hitl.len(), 2, "both rounds must remain visible");

    let pending: Vec<&Value> = hitl.iter().filter(|h| h["status"] == "pending").collect();
    assert_eq!(
        pending.len(),
        1,
        "exactly one entry must be the current, actionable one"
    );
    assert_eq!(pending[0]["id"], second.to_string());

    let resolved: Vec<&Value> = hitl.iter().filter(|h| h["status"] == "resolved").collect();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0]["id"], first.to_string());

    server.cleanup().await;
}

/// The id obtained from the execution response is a real, resolvable id through the existing,
/// unmodified generic resolve endpoint — proving discovery and resume are still properly wired
/// together end to end, even though this change only touched discovery.
#[tokio::test]
#[serial]
async fn hitl_id_from_execution_response_resolves_through_the_existing_endpoint() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;
    let agent_id = seed_agent(&server, user_id).await;
    let exec_id = seed_execution(&server, user_id, "awaiting_human").await;
    seed_hitl(
        &server,
        agent_id,
        user_id,
        exec_id,
        0,
        "pending",
        "Which repo?",
    )
    .await;

    let (_, body) = get_execution(&server, user_id, exec_id).await;
    let hitl_id = body["data"]["hitl"][0]["id"].as_str().unwrap().to_string();

    let resolve_res = auth(
        server
            .client
            .post(server.url(&format!("/api/hitl/{hitl_id}/resolve")))
            .json(&json!({ "answer": "owner/repo" })),
        user_id,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resolve_res.status(), 200);
    let resolved: Value = resolve_res.json().await.unwrap();
    assert_eq!(resolved["status"], "resolved");
    assert_eq!(resolved["human_response"]["answer"], "owner/repo");

    server.cleanup().await;
}

/// `GET /api/maf/workflow/result/{exec_id}` shares the same response wrapper as
/// `GET /api/maf/execution/{id}` — confirm it got the same `hitl` field, not just the latter.
#[tokio::test]
#[serial]
async fn workflow_result_endpoint_also_surfaces_hitl() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;
    let agent_id = seed_agent(&server, user_id).await;
    let exec_id = seed_execution(&server, user_id, "awaiting_human").await;
    let hitl_id = seed_hitl(
        &server,
        agent_id,
        user_id,
        exec_id,
        0,
        "pending",
        "Which movie?",
    )
    .await;

    let res = auth(
        server
            .client
            .get(server.url(&format!("/api/maf/workflow/result/{exec_id}"))),
        user_id,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let hitl = body["data"]["hitl"].as_array().expect("hitl array present");
    assert_eq!(hitl.len(), 1);
    assert_eq!(hitl[0]["id"], hitl_id.to_string());

    server.cleanup().await;
}
