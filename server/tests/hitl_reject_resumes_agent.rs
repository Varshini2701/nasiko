//! Regression test for the reject-hangs-forever bug: `claim_for_resume`
//! (`oss/hitl/src/store.rs`) only claimed `status = 'resolved'` rows, so rejecting a
//! `tool_approval`'s linked `direct_chat` mirror (auto-resolved to `HitlStatus::Rejected` by
//! `auto_resolve_linked_direct_chat_row`) was never picked up by the resume dispatcher — the
//! paused agent never learned the human's decision and the conversation hung forever. Also covers
//! `answer_text` (`oss/server/src/hitl/mod.rs`): before the fix it collapsed both `"confirmed"`
//! and `"denied"` into the literal reply `"authorized"`, which would have told the agent the
//! opposite of what actually happened.
//!
//! `just infra` then `cargo test -p nasiko-server --test hitl_reject_resumes_agent -- --test-threads=1`

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
    .bind(format!("reject-resume-agent-{}", Uuid::new_v4()))
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
    /// Set by the second call, so the test can assert exactly what text the resume dispatcher
    /// sent back for a REJECTED decision.
    resume_message_text: Arc<std::sync::Mutex<Option<String>>>,
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
        // Capture the human's reply text carried on the resume message.
        let text = req["params"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        *agent.resume_message_text.lock().unwrap() = Some(text);

        let mut event = serde_json::to_value(nasiko_types::a2a::status_event(
            nasiko_types::a2a::completed(&task_id, &context_id),
        ))
        .unwrap();
        if let Some(status) = event.pointer_mut("/statusUpdate/status") {
            status["message"] = json!({"parts": [{"text": "understood, not creating the issue."}]});
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

async fn start_mock_agent(mcp_row_id: Uuid) -> (String, Arc<std::sync::Mutex<Option<String>>>) {
    let resume_message_text = Arc::new(std::sync::Mutex::new(None));
    let agent = MockAgent {
        call_count: Arc::new(AtomicUsize::new(0)),
        mcp_row_id,
        resume_message_text: resume_message_text.clone(),
    };
    let app = Router::new()
        .route("/", post(mock_agent_handler))
        .with_state(agent);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), resume_message_text)
}

#[tokio::test]
#[serial]
async fn rejecting_a_mirrored_tool_approval_resumes_the_agent_with_denied() {
    let server = common::TestServer::start().await;
    let user_id = Uuid::new_v4();
    seed_user(&server, user_id).await;

    let mcp_question = json!({ "message": "Approve creating a GitHub issue?" });
    // Placeholder agent id — overwritten below once the mock server's real port is known.
    let agent_id = seed_running_agent(&server, user_id, "http://placeholder.invalid").await;
    let mcp_row_id = seed_mcp_tool_row(&server, agent_id, user_id, &mcp_question).await;

    let (agent_url, resume_message_text) = start_mock_agent(mcp_row_id).await;
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

    // REJECT, not approve — this is the exact path that used to hang forever.
    let resolve_res = auth(
        server
            .client
            .post(server.url(&format!("/api/hitl/{real_id}/resolve")))
            .json(&json!({"decision": "reject"})),
        user_id,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resolve_res.status(), 200);

    // The real row and its mirror must both land on `rejected`.
    let mcp_status: String = sqlx::query_scalar("SELECT status FROM hitl_requests WHERE id = $1")
        .bind(mcp_row_id)
        .fetch_one(&server.db)
        .await
        .unwrap();
    assert_eq!(mcp_status, "rejected");

    let mirror_status: String = sqlx::query_scalar(
        "SELECT status FROM hitl_requests WHERE origin = 'direct_chat' AND question->'metadata'->>'hitl_request_id' = $1",
    )
    .bind(mcp_row_id.to_string())
    .fetch_one(&server.db)
    .await
    .unwrap();
    assert_eq!(
        mirror_status, "rejected",
        "auto_resolve_linked_direct_chat_row must reject the mirror too"
    );

    // Poll for the dispatcher to actually pick up and deliver the rejected mirror — this is the
    // part that used to never happen at all (claim_for_resume excluded 'rejected').
    let delivered = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if resume_message_text.lock().unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    assert!(
        delivered.is_ok(),
        "the resume dispatcher must claim and deliver the rejected mirror — it never did before this fix"
    );

    let text = resume_message_text.lock().unwrap().clone().unwrap();
    assert_eq!(
        text, "denied",
        "a rejected tool_approval must resume the agent with \"denied\", not the literal \
         \"authorized\" `answer_text` used to produce for every auth_outcome"
    );

    server.cleanup().await;
}
