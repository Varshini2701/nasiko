//! Regression test: reconnect (`metadata.reconnect_after_hitl_id`, `a2a_dispatch.rs::
//! reconnect_stream`) for a mirror-linked `tool_approval` row, where the frontend only ever holds
//! the REAL `mcp_tool` row's id — never the mirror's own, per `resolve_display_row` substitution
//! (§2.3 of the frontend contract). Without following that link back to the mirror (the row
//! `deliver()` actually processes, and the continuation buffer is keyed by), reconnecting with the
//! only id the frontend legitimately has hangs forever with no data and no error — confirmed live
//! before `find_linked_row_any_status` (`oss/hitl/src/repo.rs`) was added to fix it.
//!
//! `just infra` then `cargo test -p nasiko-server --test hitl_reconnect_mirror_check -- --test-threads=1`

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::{Router, extract::State, routing::post};
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

async fn seed_running_agent(server: &common::TestServer, owner_id: Uuid, url: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO agents (name, owner_id, url, status) VALUES ($1, $2, $3, 'running') RETURNING id",
    )
    .bind(format!("mirror-check-agent-{}", Uuid::new_v4()))
    .bind(owner_id)
    .bind(url)
    .fetch_one(&server.db)
    .await
    .expect("seed_running_agent")
}

async fn seed_mcp_tool_row(
    server: &common::TestServer,
    agent_id: Uuid,
    owner_user_id: Uuid,
    question: &Value,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO hitl_requests
            (kind, origin, agent_id, owner_user_id, context_id, connector_id, tool_name, question)
         VALUES ('tool_approval', 'mcp_tool', $1, $2, $3, $4, $5, $6)
         RETURNING id",
    )
    .bind(agent_id)
    .bind(owner_user_id)
    .bind(format!("mcp-ctx-{}", Uuid::new_v4()))
    .bind(Uuid::new_v4())
    .bind("github_create_issue")
    .bind(question)
    .fetch_one(&server.db)
    .await
    .expect("seed_mcp_tool_row")
}

#[derive(Clone)]
struct MockAgent {
    call_count: Arc<AtomicUsize>,
    mcp_row_id: Uuid,
}

async fn mock_agent_handler(
    State(agent): State<MockAgent>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let idx = agent.call_count.fetch_add(1, Ordering::SeqCst);
    let req: Value = serde_json::from_slice(&body).unwrap_or_default();
    let task_id = req["params"]["message"]["taskId"]
        .as_str()
        .unwrap_or("mock-task")
        .to_string();
    let context_id = req["params"]["message"]["contextId"]
        .as_str()
        .unwrap_or("mock-ctx")
        .to_string();

    let event = if idx == 0 {
        let status =
            nasiko_types::a2a::auth_required(&task_id, &context_id, "Please authorize with GitHub");
        let mut event = serde_json::to_value(nasiko_types::a2a::status_event(status)).unwrap();
        if let Some(msg) = event.pointer_mut("/statusUpdate/status/message") {
            msg["metadata"] = json!({ "hitl_request_id": agent.mcp_row_id.to_string() });
        }
        event
    } else {
        let mut event = serde_json::to_value(nasiko_types::a2a::status_event(
            nasiko_types::a2a::completed(&task_id, &context_id),
        ))
        .unwrap();
        if let Some(status) = event.pointer_mut("/statusUpdate/status") {
            status["message"] = json!({"parts": [{"text": "issue created."}]});
        }
        event
    };
    let sse_body = format!("data: {event}\n\n");
    axum::response::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(sse_body))
        .unwrap()
}

async fn start_mock_agent(mcp_row_id: Uuid) -> (String, tokio::task::JoinHandle<()>) {
    let agent = MockAgent {
        call_count: Arc::new(AtomicUsize::new(0)),
        mcp_row_id,
    };
    let app = Router::new()
        .route("/", post(mock_agent_handler))
        .with_state(agent);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
#[serial]
async fn reconnect_with_the_real_mcp_tool_rows_id_follows_the_mirror() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;

    let mcp_question = json!({ "message": "Approve creating a GitHub issue?" });
    let agent_id = seed_running_agent(&server, user_id, "http://placeholder.invalid").await;
    let mcp_row_id = seed_mcp_tool_row(&server, agent_id, user_id, &mcp_question).await;

    let (agent_url, _agent_handle) = start_mock_agent(mcp_row_id).await;
    sqlx::query("UPDATE agents SET url = $1 WHERE id = $2")
        .bind(&agent_url)
        .bind(agent_id)
        .execute(&server.db)
        .await
        .unwrap();

    let session_id = format!("ses_{}", Uuid::new_v4().simple());
    let body = json!({
        "jsonrpc": "2.0", "id": Uuid::new_v4().to_string(), "method": "message/stream",
        "params": {
            "message": {"messageId": Uuid::new_v4().to_string(), "contextId": session_id, "role": "ROLE_USER", "parts": [{"text": "create a github issue"}]},
            "metadata": {"agent_id": agent_id.to_string(), "session_id": session_id},
        },
    });
    let res = auth(
        server
            .client
            .post(server.url("/api/orchestrator/a2a"))
            .json(&body),
        user_id,
    )
    .send()
    .await
    .unwrap();
    let raw = res.text().await.unwrap();

    let mut real_id = None;
    for line in raw.lines() {
        if let Some(data) = line.strip_prefix("data: ")
            && let Ok(parsed) = serde_json::from_str::<Value>(data)
            && let Some(parts) = parsed
                .pointer("/statusUpdate/status/message/parts")
                .and_then(|p| p.as_array())
        {
            for part in parts {
                if let Some(d) = part.get("data")
                    && d.get("type").and_then(|v| v.as_str()) == Some("hitl")
                {
                    real_id = d.get("id").and_then(|v| v.as_str()).map(String::from);
                }
            }
        }
    }
    let real_id = real_id.expect("must find a hitl frame");
    assert_eq!(
        real_id,
        mcp_row_id.to_string(),
        "the frontend sees the REAL mcp_tool row's id, per §2.3"
    );

    let resolve_res = auth(
        server
            .client
            .post(server.url(&format!("/api/hitl/{real_id}/resolve")))
            .json(&json!({"decision": "approve", "scope": "once"})),
        user_id,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resolve_res.status(), 200);

    tokio::time::sleep(Duration::from_millis(800)).await;

    let reconnect_body = json!({
        "jsonrpc": "2.0", "id": Uuid::new_v4().to_string(), "method": "message/stream",
        "params": {
            "message": {"messageId": Uuid::new_v4().to_string(), "role": "ROLE_USER", "parts": []},
            "metadata": {"reconnect_after_hitl_id": real_id},
        },
    });
    let reconnect_res = tokio::time::timeout(
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
    .expect("reconnect request itself must not hang")
    .expect("reconnect request must succeed");
    assert!(reconnect_res.status().is_success());

    let text = tokio::time::timeout(Duration::from_secs(5), reconnect_res.text())
        .await
        .expect("reading the reconnect stream must not hang now that the mirror fix is in place")
        .unwrap();
    assert!(
        text.contains("issue created."),
        "reconnect via the real mcp_tool row's id must deliver the mirror's real resumed reply:\n{text}"
    );

    server.cleanup().await;
}
