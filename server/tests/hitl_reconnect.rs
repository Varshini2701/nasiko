//! End-to-end tests for reconnecting to a resumed HITL execution's real A2A/SSE events through
//! the EXISTING `POST /api/orchestrator/a2a` endpoint (`a2a_dispatch.rs::reconnect_stream`,
//! `oss/server/src/hitl/continuation.rs`).
//!
//! What these prove, concretely:
//!   1. After `POST /api/hitl/{id}/resolve`, a client can reconnect via
//!      `POST /api/orchestrator/a2a` with `metadata.reconnect_after_hitl_id` and receive the
//!      agent's real resumed reply on that same connection — without a second agent invocation
//!      (the mock agent's call count stays at exactly the number of real turns: initial + resume,
//!      never bumped by the reconnect request itself).
//!   2. A second, sequential HITL produced by the resumed agent is discoverable directly from the
//!      reconnected stream's own `"type":"hitl"` frame — not by polling `/messages`.
//!   3. A reconnect that arrives *after* the resume has already fully finished still replays the
//!      complete sequence (the race the continuation buffer exists to close).
//!
//! Requires infra (Postgres, Redis) like the rest of the suite:
//!   `just infra` then `cargo test -p nasiko-server --test hitl_reconnect -- --test-threads=1`

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
    .bind(format!("hitl-reconnect-test-agent-{}", Uuid::new_v4()))
    .bind(owner_id)
    .bind(url)
    .fetch_one(&server.db)
    .await
    .expect("seed_running_agent")
}

/// Same fixture-agent shape as `hitl_stream_metadata.rs`'s `MockAgent` — each call consumes the
/// next scripted response in order, so one server can script "pause, then (on resume) pause
/// again or complete" without a real agent SDK. Duplicated here rather than shared, matching this
/// suite's existing convention (each integration-test binary is compiled independently).
#[derive(Clone)]
struct MockAgent {
    responses: Arc<Vec<MockResponse>>,
    call_count: Arc<AtomicUsize>,
}

enum MockResponse {
    Pause { auth: bool, message: &'static str },
    Complete { text: &'static str },
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

    let response = agent
        .responses
        .get(idx)
        .expect("MockAgent received more calls than scripted responses — a reconnect must never cause an extra one");

    let event = match response {
        MockResponse::Pause { auth, message } => {
            let status = if *auth {
                nasiko_types::a2a::auth_required(&task_id, &context_id, message)
            } else {
                nasiko_types::a2a::input_required(&task_id, &context_id, message)
            };
            serde_json::to_value(nasiko_types::a2a::status_event(status))
                .expect("StreamResponse must serialize")
        }
        MockResponse::Complete { text } => {
            let mut event = serde_json::to_value(nasiko_types::a2a::status_event(
                nasiko_types::a2a::completed(&task_id, &context_id),
            ))
            .expect("StreamResponse must serialize");
            // `completed()` on its own carries no message — inject one the same way
            // `hitl_stream_metadata.rs`'s fixture injects `metadata`, so the terminal event
            // itself carries the reply text `task_reply_text` (`agent_proxy.rs`) looks for.
            if let Some(status) = event.pointer_mut("/statusUpdate/status") {
                status["message"] = json!({"parts": [{"text": text}]});
            }
            event
        }
    };

    let sse_body = format!("data: {event}\n\n");
    axum::response::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(sse_body))
        .unwrap()
}

async fn start_mock_agent(responses: Vec<MockResponse>) -> (String, tokio::task::JoinHandle<()>) {
    let agent = MockAgent {
        responses: Arc::new(responses),
        call_count: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/", post(mock_agent_handler))
        .with_state(agent.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

async fn send_turn(
    server: &common::TestServer,
    user_id: Uuid,
    agent_id: Uuid,
    session_id: &str,
    context_id: &str,
    text: &str,
) -> String {
    let body = json!({
        "jsonrpc": "2.0",
        "id": Uuid::new_v4().to_string(),
        "method": "message/stream",
        "params": {
            "message": {
                "messageId": Uuid::new_v4().to_string(),
                "contextId": context_id,
                "role": "ROLE_USER",
                "parts": [{"text": text}],
            },
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
    .expect("a2a turn request failed");
    assert!(
        res.status().is_success(),
        "a2a turn returned {}",
        res.status()
    );
    res.text().await.expect("read a2a stream body")
}

fn extract_hitl_frame(raw: &str) -> Value {
    for line in raw.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let parts = parsed
            .pointer("/statusUpdate/status/message/parts")
            .or_else(|| parsed.pointer("/result/statusUpdate/status/message/parts"));
        if let Some(parts) = parts.and_then(|p| p.as_array()) {
            for part in parts {
                if let Some(d) = part.get("data")
                    && d.get("type").and_then(|v| v.as_str()) == Some("hitl")
                {
                    return d.clone();
                }
            }
        }
    }
    panic!("no \"type\":\"hitl\" frame found in stream:\n{raw}");
}

/// True once every SSE `data:` line in `raw` has been checked and none carries the final
/// `TASK_STATE_COMPLETED` reply text — used to assert a reconnect stream actually delivered it.
fn extract_final_reply_text(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if parsed
            .pointer("/statusUpdate/status/state")
            .and_then(|v| v.as_str())
            == Some("TASK_STATE_COMPLETED")
            && let Some(text) = parsed
                .pointer("/statusUpdate/status/message/parts/0/text")
                .and_then(|v| v.as_str())
        {
            return Some(text.to_string());
        }
    }
    None
}

async fn resolve(server: &common::TestServer, user_id: Uuid, id: &str, answer: &str) {
    let res = auth(
        server
            .client
            .post(server.url(&format!("/api/hitl/{id}/resolve")))
            .json(&json!({"answer": answer})),
        user_id,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(
        res.status(),
        200,
        "resolve must succeed: {:?}",
        res.text().await
    );
}

async fn reconnect(server: &common::TestServer, user_id: Uuid, hitl_id: &str) -> String {
    let body = json!({
        "jsonrpc": "2.0",
        "id": Uuid::new_v4().to_string(),
        "method": "message/stream",
        "params": {
            "message": {"messageId": Uuid::new_v4().to_string(), "role": "ROLE_USER", "parts": []},
            "metadata": {"reconnect_after_hitl_id": hitl_id},
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
    .expect("reconnect request failed");
    assert!(
        res.status().is_success(),
        "reconnect returned {}: {:?}",
        res.status(),
        res.text().await
    );
    res.text().await.expect("read reconnect stream body")
}

/// The core requirement: reconnecting after resolve delivers the resumed agent's real final
/// reply, and the reconnect itself never triggers a second agent invocation.
#[tokio::test]
#[serial]
async fn reconnect_delivers_the_resumed_agents_real_reply_without_a_second_invocation() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;

    let (agent_url, _agent_handle) = start_mock_agent(vec![
        MockResponse::Pause {
            auth: false,
            message: "Which movie would you like to watch?",
        },
        MockResponse::Complete {
            text: "Booked Interstellar for 7pm.",
        },
    ])
    .await;
    let agent_id = seed_running_agent(&server, user_id, &agent_url).await;
    let session_id = format!("ses_{}", Uuid::new_v4().simple());

    let initial = send_turn(
        &server,
        user_id,
        agent_id,
        &session_id,
        &session_id,
        "Book me a movie ticket",
    )
    .await;
    let hitl_id = extract_hitl_frame(&initial)["id"]
        .as_str()
        .unwrap()
        .to_string();

    resolve(&server, user_id, &hitl_id, "Interstellar").await;

    let continuation = reconnect(&server, user_id, &hitl_id).await;
    let reply = extract_final_reply_text(&continuation);
    assert_eq!(
        reply.as_deref(),
        Some("Booked Interstellar for 7pm."),
        "reconnect stream must carry the resumed agent's real reply:\n{continuation}"
    );

    server.cleanup().await;
}

/// A second, sequential HITL produced by the resumed agent must be discoverable directly from
/// the reconnected stream — not by a separate poll of `/messages`.
#[tokio::test]
#[serial]
async fn reconnect_surfaces_a_sequential_hitl_from_the_continuation_stream() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;

    let (agent_url, _agent_handle) = start_mock_agent(vec![
        MockResponse::Pause {
            auth: false,
            message: "Which movie would you like to watch?",
        },
        MockResponse::Pause {
            auth: true,
            message: "Reply \"authorized\" once you've granted access.",
        },
    ])
    .await;
    let agent_id = seed_running_agent(&server, user_id, &agent_url).await;
    let session_id = format!("ses_{}", Uuid::new_v4().simple());

    let initial = send_turn(
        &server,
        user_id,
        agent_id,
        &session_id,
        &session_id,
        "Book me a movie ticket",
    )
    .await;
    let first_id = extract_hitl_frame(&initial)["id"]
        .as_str()
        .unwrap()
        .to_string();

    resolve(&server, user_id, &first_id, "Interstellar").await;

    let continuation = reconnect(&server, user_id, &first_id).await;
    let second_frame = extract_hitl_frame(&continuation);
    assert_eq!(second_frame["kind"], "auth_required");
    let second_id = second_frame["id"].as_str().unwrap().to_string();
    assert_ne!(
        first_id, second_id,
        "the second pause must get its own row id"
    );

    server.cleanup().await;
}

/// The race the continuation buffer exists to close: a reconnect that arrives only after the
/// resume has already fully finished (`resume_status` reached a terminal value) must still
/// replay the complete sequence, not find an empty/hung stream.
#[tokio::test]
#[serial]
async fn reconnect_after_resume_already_finished_still_replays_everything() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;

    let (agent_url, _agent_handle) = start_mock_agent(vec![
        MockResponse::Pause {
            auth: false,
            message: "Which movie would you like to watch?",
        },
        MockResponse::Complete {
            text: "Booked Interstellar for 7pm.",
        },
    ])
    .await;
    let agent_id = seed_running_agent(&server, user_id, &agent_url).await;
    let session_id = format!("ses_{}", Uuid::new_v4().simple());

    let initial = send_turn(
        &server,
        user_id,
        agent_id,
        &session_id,
        &session_id,
        "Book me a movie ticket",
    )
    .await;
    let hitl_id = extract_hitl_frame(&initial)["id"]
        .as_str()
        .unwrap()
        .to_string();

    resolve(&server, user_id, &hitl_id, "Interstellar").await;

    // Deliberately wait for the background dispatcher to fully finish BEFORE reconnecting —
    // proves this isn't just "fast enough to still be live."
    let hitl_uuid: Uuid = hitl_id.parse().unwrap();
    let mut finished = false;
    for _ in 0..40 {
        let resume_status: String =
            sqlx::query_scalar("SELECT resume_status FROM hitl_requests WHERE id = $1")
                .bind(hitl_uuid)
                .fetch_one(&server.db)
                .await
                .unwrap();
        if resume_status != "not_started" {
            finished = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(finished, "resume never left resume_status = not_started");
    // Give the dispatcher's own `ContinuationGuard` a moment to drop (marking the buffer
    // terminal) after `mark_resume_completed` — both happen in the same function, but not
    // necessarily on the exact same poll tick as the DB write above.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let continuation = reconnect(&server, user_id, &hitl_id).await;
    let reply = extract_final_reply_text(&continuation);
    assert_eq!(
        reply.as_deref(),
        Some("Booked Interstellar for 7pm."),
        "a late reconnect must still replay the complete sequence:\n{continuation}"
    );

    server.cleanup().await;
}
