//! Flow participant records — the distributed invariant behind MCP gateway /
//! LLM router authorization (docs/MCP_GATEWAY_AGENT_AUTH.md §2.3, §3.4).
//!
//! Every dispatch path must insert a `flow_participants` row for the target
//! agent alongside its `flows` insert, synchronously, BEFORE the request
//! reaches the agent — the gateway and router deny any agent that isn't a
//! recorded participant of the traceparent-named flow, so a path that forgets
//! the write is an agent outage (fail closed), and one that writes late is a
//! race. These tests cover the two HTTP-reachable paths (direct proxy,
//! explicit-agent dispatch); the orchestrator cascade legs (`CpCallGuard`) and
//! MAF executor paths write through the same table and are covered by their
//! own flows' tests plus code review — they require a live LLM to drive.
//!
//! Also locks in the header contract replacing the delegation token: the agent
//! must receive `traceparent` and must NOT receive `x-nasiko-agent-token`.

mod common;

use axum::{Json, Router, extract::Request, routing::get, routing::post};
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

/// One `(flow_id, participant?)` snapshot per flow rooted at `agent_id`.
async fn flows_with_participation(
    server: &common::TestServer,
    agent_id: Uuid,
) -> Vec<(String, bool)> {
    sqlx::query_as(
        "SELECT f.flow_id, EXISTS(SELECT 1 FROM flow_participants fp \
                WHERE fp.flow_id = f.flow_id AND fp.agent_id = $1) \
         FROM flows f WHERE f.root_agent_id = $1",
    )
    .bind(agent_id)
    .fetch_all(&server.db)
    .await
    .unwrap()
}

/// Stub agent: echoes headers on GET /echo, answers any POST with a flat A2A
/// message result (what a2a_dispatch's non-streaming fallback parses).
async fn start_stub_agent() -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
) {
    let seen: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> = Default::default();
    let seen_get = seen.clone();
    let echo = get(move |req: Request| {
        let seen = seen_get.clone();
        async move {
            for (name, value) in req.headers() {
                seen.lock().unwrap().push((
                    name.as_str().to_string(),
                    value.to_str().unwrap_or("").to_string(),
                ));
            }
            Json(json!({"ok": true}))
        }
    });
    let a2a = post(|| async {
        Json(json!({
            "jsonrpc": "2.0", "id": "1",
            "result": {"kind": "message", "parts": [{"kind": "text", "text": "ok"}]}
        }))
    });
    let app = Router::new()
        .route("/echo", echo)
        .route("/", a2a.clone())
        .route("/jsonrpc", a2a);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

#[tokio::test]
#[serial]
async fn agent_proxy_records_flow_participant_before_forwarding() {
    let server = common::TestServer::start().await;
    let owner = init_admin(&server).await;
    let (stub_url, seen_headers) = start_stub_agent().await;
    let agent = seed_running_agent(&server, owner, "fp-proxy-agent", &stub_url).await;

    let res = common::as_superuser(
        server
            .client
            .get(server.url(&format!("/api/agents/{agent}/echo"))),
        &owner.to_string(),
        "admin",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 200);

    let flows = flows_with_participation(&server, agent).await;
    assert_eq!(flows.len(), 1, "the proxy must open exactly one flow");
    assert!(
        flows[0].1,
        "the target agent must be a recorded participant of its flow"
    );

    // Header contract: traceparent forwarded, no per-request MCP credential.
    let headers = seen_headers.lock().unwrap().clone();
    assert!(
        headers.iter().any(|(n, _)| n == "traceparent"),
        "traceparent must reach the agent: {headers:?}"
    );
    assert!(
        headers.iter().all(|(n, _)| n != "x-nasiko-agent-token"),
        "the delegation token header must be gone: {headers:?}"
    );

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn a2a_dispatch_records_flow_participant_for_explicit_agent() {
    let server = common::TestServer::start().await;
    let owner = init_admin(&server).await;
    let (stub_url, _) = start_stub_agent().await;
    let agent = seed_running_agent(&server, owner, "fp-dispatch-agent", &stub_url).await;

    let body = json!({
        "jsonrpc": "2.0",
        "method": "message/stream",
        "id": Uuid::new_v4().to_string(),
        "params": {
            "message": {
                "messageId": Uuid::new_v4().to_string(),
                "role": "ROLE_USER",
                "parts": [{ "text": "hello" }]
            },
            "metadata": { "agent_id": agent.to_string() }
        }
    });
    let res = common::as_superuser(
        server.client.post(server.url("/api/orchestrator/a2a")),
        &owner.to_string(),
        "admin",
    )
    .json(&body)
    .send()
    .await
    .unwrap();
    // The stub answers plain JSON, which the dispatch path accepts via its
    // non-streaming fallback — but the participant record must exist even if
    // the agent leg had failed, because it is written before the forward.
    assert!(
        res.status().is_success(),
        "dispatch should succeed against the stub: {}",
        res.status()
    );

    let flows = flows_with_participation(&server, agent).await;
    assert_eq!(flows.len(), 1, "dispatch must open exactly one flow");
    assert!(
        flows[0].1,
        "the dispatched agent must be a recorded participant of its flow"
    );

    server.cleanup().await;
}
