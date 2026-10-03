//! End-to-end regression test for the dual-origin single-approval bug: found
//! live testing a real deployed agent — an agent that maps MCP's own
//! `ask_required`/`auth_required` JSON-RPC signal onto the A2A
//! `AUTH_REQUIRED` task state (a real, intentional design choice, not a
//! mistake) causes direct-chat's own pause detection
//! (`a2a_dispatch.rs`/`agent_proxy.rs`) to create its own row for the exact
//! same event MCP's gateway already persisted its own row for.
//!
//! An earlier version of this fix suppressed creating that second row
//! entirely — which broke live: direct-chat's own dispatcher
//! (`oss/server/src/hitl/mod.rs::deliver`) is the *only* mechanism that
//! resumes the specific, visible chat task a human is watching (it needs
//! `task_id`, which only a `direct_chat`/`agent_proxy`-origin row carries at
//! all) — MCP's own dispatcher only ever sends a stateless, task-blind
//! nudge that can never reach that task. Suppressing the row meant nothing
//! ever resumed the visible conversation, even though the MCP-side
//! permission grant worked correctly (confirmed live: "resume dispatcher:
//! delivered" in the logs, and a chat tab that never updated).
//!
//! The actual fix keeps both rows, but auto-resolves the linked
//! `direct_chat`/`agent_proxy` row the moment the `mcp_tool` row is resolved
//! (`router/hitl.rs::auto_resolve_linked_direct_chat_row`) — a single human
//! action grants the real permission (MCP's row) *and* resumes the specific
//! visible task (the linked row, through its own existing, unmodified
//! dispatcher). This file proves that, plus that an entirely unrelated
//! genuine direct-chat pause is completely unaffected.
//!
//!   cargo test -p nasiko-server --test mcp_direct_chat_duplicate_pause -- --test-threads=1

mod common;

use axum::response::IntoResponse;
use axum::routing::post;
use serde_json::{Value, json};
use serial_test::serial;
use sqlx::PgPool;
use uuid::Uuid;

async fn init_admin(server: &common::TestServer) -> (String, Uuid) {
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
    let id = v["user_id"].as_str().unwrap().to_string();
    (id.clone(), Uuid::parse_str(&id).unwrap())
}

async fn seed_running_agent(db: &PgPool, owner: Uuid, name: &str, url: &str) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO agents (name, description, status, owner_id, url, skills, tags)
         VALUES ($1, 'Duplicate-pause regression test agent', 'running', $2, $3, '[]'::jsonb, '{}')
         RETURNING id",
    )
    .bind(name)
    .bind(owner)
    .bind(url)
    .fetch_one(db)
    .await
    .expect("insert running agent")
}

/// Point a seeded agent at the stub's real URL. The stub can only start once `mcp_row_id` exists,
/// and that row has to name the agent — so the agent is seeded first with a placeholder.
async fn point_agent_at(db: &PgPool, agent_id: Uuid, url: &str) {
    sqlx::query("UPDATE agents SET url = $1 WHERE id = $2")
        .bind(url)
        .bind(agent_id)
        .execute(db)
        .await
        .expect("point agent at stub");
}

/// A stub agent that unconditionally replies with a single, plain-JSON
/// `TASK_STATE_AUTH_REQUIRED` response — `Content-Type: application/json`
/// (not `text/event-stream`) routes dispatch straight into the non-streaming
/// pause branch on the first request, no SSE framing needed. `message`
/// carries whatever metadata the test wants to attach, exactly mirroring
/// what a real agent's own status-message metadata looks like on the wire.
async fn start_stub_agent(message_metadata: Value) -> String {
    async fn respond(
        axum::extract::State(metadata): axum::extract::State<Value>,
    ) -> impl IntoResponse {
        axum::Json(json!({
            "result": {
                "statusUpdate": {
                    "taskId": "stub-task-1",
                    "contextId": "stub-context-1",
                    "status": {
                        "state": "TASK_STATE_AUTH_REQUIRED",
                        "message": {
                            "parts": [{"text": "Tool(s) require user approval for this agent."}],
                            "metadata": metadata,
                        }
                    }
                }
            },
            "id": "1",
            "jsonrpc": "2.0"
        }))
    }
    let app = axum::Router::new()
        .route("/", post(respond))
        .with_state(message_metadata);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{port}/")
}

fn dispatch_body(agent_id: Uuid) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "message/stream",
        "id": Uuid::new_v4().to_string(),
        "params": {
            "message": {
                "messageId": Uuid::new_v4().to_string(),
                "role": "ROLE_USER",
                "parts": [{ "text": "list my repos" }]
            },
            "metadata": { "agent_id": agent_id.to_string() }
        }
    })
}

#[tokio::test]
#[serial]
async fn resolving_the_mcp_row_auto_resolves_the_linked_direct_chat_row() {
    let server = common::TestServer::start().await;
    let (admin_id, admin_uuid) = init_admin(&server).await;

    let connector_id: Uuid = sqlx::query_scalar(
        "INSERT INTO mcp_connectors (provider_type, owner_id, name, url, auth_type)
         VALUES ('mcp_server', $1, 'dup-pause-test-connector', 'https://example.com', 'none')
         RETURNING id",
    )
    .bind(admin_uuid)
    .fetch_one(&server.db)
    .await
    .unwrap();

    // Both halves of a mirror name the SAME agent in production: the gateway files the `mcp_tool`
    // row against the calling agent, and it is that agent's own task that pauses.
    // `find_linked_direct_chat_row` enforces it — a cross-agent link can only be forged or stale —
    // so the agent is seeded first and the mcp row filed against it.
    let chat_agent_id = seed_running_agent(
        &server.db,
        admin_uuid,
        "dup-pause-chat-agent",
        "http://placeholder.invalid",
    )
    .await;

    // The real MCP-origin row this pause is meant to mirror — created the
    // same way `protocol::create_tool_approval_id` does.
    let mcp_row_id: Uuid = sqlx::query_scalar(
        "INSERT INTO hitl_requests \
            (kind, origin, agent_id, owner_user_id, connector_id, tool_name, context_id, question, status, expires_at) \
         VALUES ('tool_approval', 'mcp_tool', $1, $2, $3, 'GITHUB_LIST_REPOS', 'ses_dup_pause_test', '{}'::jsonb, \
                 'pending', now() + interval '7 days') \
         RETURNING id",
    )
    .bind(chat_agent_id)
    .bind(admin_uuid)
    .bind(connector_id)
    .fetch_one(&server.db)
    .await
    .unwrap();

    let stub_url = start_stub_agent(json!({
        "auth_kind": "mcp_tool_approval",
        "hitl_request_id": mcp_row_id.to_string(),
        "tool_slug": "GITHUB_LIST_REPOS",
    }))
    .await;
    point_agent_at(&server.db, chat_agent_id, &stub_url).await;

    let req = server.client.post(server.url("/api/orchestrator/a2a"));
    let res = common::as_superuser(req, &admin_id, "admin")
        .json(&dispatch_body(chat_agent_id))
        .send()
        .await
        .unwrap();
    assert!(
        res.status().is_success(),
        "dispatch itself must still succeed: {}",
        res.status()
    );
    let _ = res.bytes().await;

    // Both rows must exist — the direct-chat pause is NOT suppressed
    // anymore, since it's the only thing that can ever resume the visible
    // task.
    let total_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM hitl_requests")
        .fetch_one(&server.db)
        .await
        .unwrap();
    assert_eq!(
        total_rows, 2,
        "both the mcp_tool row and its direct_chat mirror must exist"
    );

    let (linked_row_id, linked_status_before): (Uuid, String) =
        sqlx::query_as("SELECT id, status FROM hitl_requests WHERE origin = 'direct_chat'")
            .fetch_one(&server.db)
            .await
            .unwrap();
    assert_eq!(linked_status_before, "pending");

    // Resolve ONLY the mcp_tool row, through the real API — exactly the one
    // action a human takes in the console.
    let req = server
        .client
        .post(server.url(&format!("/api/hitl/{mcp_row_id}/resolve")));
    let res = common::as_superuser(req, &admin_id, "admin")
        .json(&json!({"decision": "approve", "scope": "once"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    // The linked direct_chat row must now ALSO be resolved — auto-resolved
    // in lockstep, no second manual action.
    let (linked_status_after, human_response): (String, Option<Value>) =
        sqlx::query_as("SELECT status, human_response FROM hitl_requests WHERE id = $1")
            .bind(linked_row_id)
            .fetch_one(&server.db)
            .await
            .unwrap();
    assert_eq!(
        linked_status_after, "resolved",
        "resolving the mcp_tool row must auto-resolve its linked direct_chat row"
    );
    assert_eq!(
        human_response
            .as_ref()
            .and_then(|v| v["auth_outcome"].as_str()),
        Some("confirmed")
    );

    server.cleanup().await;
}

/// Same guarantee as `resolving_the_mcp_row_auto_resolves_the_linked_direct_chat_row`, but for a
/// genuinely broken connector credential (`kind = auth_required`, resolved via the two-click
/// `auth_action: confirm` flow) rather than a permission gate (`kind = tool_approval`) — a
/// separate code path in `router/hitl.rs::resolve()` that needs its own call into
/// `auto_resolve_linked_direct_chat_row`, found missing by code review.
#[tokio::test]
#[serial]
async fn confirming_an_auth_required_mcp_row_auto_resolves_the_linked_direct_chat_row() {
    let server = common::TestServer::start().await;
    let (admin_id, admin_uuid) = init_admin(&server).await;

    let connector_id: Uuid = sqlx::query_scalar(
        "INSERT INTO mcp_connectors (provider_type, owner_id, name, url, auth_type)
         VALUES ('mcp_server', $1, 'dup-pause-auth-connector', 'https://example.com', 'none')
         RETURNING id",
    )
    .bind(admin_uuid)
    .fetch_one(&server.db)
    .await
    .unwrap();

    // Same agent on both halves of the mirror — see the first test's own note.
    let chat_agent_id = seed_running_agent(
        &server.db,
        admin_uuid,
        "dup-pause-auth-chat-agent",
        "http://placeholder.invalid",
    )
    .await;

    // A genuine broken-connector-credential row — `create_pending_auth_required`'s shape, not
    // `create_tool_approval_id`'s.
    let mcp_row_id: Uuid = sqlx::query_scalar(
        "INSERT INTO hitl_requests \
            (kind, origin, agent_id, owner_user_id, connector_id, context_id, question, status, expires_at) \
         VALUES ('auth_required', 'mcp_tool', $1, $2, $3, 'ses_dup_pause_auth_test', '{}'::jsonb, \
                 'pending', now() + interval '7 days') \
         RETURNING id",
    )
    .bind(chat_agent_id)
    .bind(admin_uuid)
    .bind(connector_id)
    .fetch_one(&server.db)
    .await
    .unwrap();

    let stub_url = start_stub_agent(json!({
        "auth_kind": "mcp_connector",
        "hitl_request_id": mcp_row_id.to_string(),
    }))
    .await;
    point_agent_at(&server.db, chat_agent_id, &stub_url).await;

    let req = server.client.post(server.url("/api/orchestrator/a2a"));
    let res = common::as_superuser(req, &admin_id, "admin")
        .json(&dispatch_body(chat_agent_id))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());
    let _ = res.bytes().await;

    let (linked_row_id, linked_status_before): (Uuid, String) = sqlx::query_as(
        "SELECT id, status FROM hitl_requests WHERE origin = 'direct_chat' AND agent_id = $1",
    )
    .bind(chat_agent_id)
    .fetch_one(&server.db)
    .await
    .unwrap();
    assert_eq!(linked_status_before, "pending");

    // The real, manual two-click confirm a human takes in the console for a broken-credential
    // pause — not the OAuth-callback bulk path.
    let req = server
        .client
        .post(server.url(&format!("/api/hitl/{mcp_row_id}/resolve")));
    let res = common::as_superuser(req, &admin_id, "admin")
        .json(&json!({"auth_action": "confirm"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    let (linked_status_after, human_response): (String, Option<Value>) =
        sqlx::query_as("SELECT status, human_response FROM hitl_requests WHERE id = $1")
            .bind(linked_row_id)
            .fetch_one(&server.db)
            .await
            .unwrap();
    assert_eq!(
        linked_status_after, "resolved",
        "confirming the auth_required mcp_tool row must auto-resolve its linked direct_chat row"
    );
    assert_eq!(
        human_response
            .as_ref()
            .and_then(|v| v["auth_outcome"].as_str()),
        Some("confirmed")
    );

    server.cleanup().await;
}

/// Regression guard: an entirely unrelated, genuine direct-chat pause (no
/// `hitl_request_id` at all — the ordinary case for e.g. `ask_human`) is
/// completely unaffected by the linkage mechanism. It persists its own row
/// exactly as before, and nothing auto-resolves it.
#[tokio::test]
#[serial]
async fn a_genuine_direct_chat_pause_with_no_mcp_link_is_unaffected() {
    let server = common::TestServer::start().await;
    let (admin_id, admin_uuid) = init_admin(&server).await;

    let stub_url = start_stub_agent(json!({})).await;
    let chat_agent_id = seed_running_agent(
        &server.db,
        admin_uuid,
        "genuine-pause-chat-agent",
        &stub_url,
    )
    .await;

    let req = server.client.post(server.url("/api/orchestrator/a2a"));
    let res = common::as_superuser(req, &admin_id, "admin")
        .json(&dispatch_body(chat_agent_id))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());
    let _ = res.bytes().await;

    let (kind, origin, status): (String, String, String) =
        sqlx::query_as("SELECT kind, origin, status FROM hitl_requests WHERE owner_user_id = $1")
            .bind(admin_uuid)
            .fetch_one(&server.db)
            .await
            .expect("a genuine direct_chat pause must still persist its own row");
    assert_eq!(kind, "auth_required");
    assert_eq!(origin, "direct_chat");
    assert_eq!(status, "pending", "nothing should have auto-resolved it");

    server.cleanup().await;
}

/// A `hitl_request_id` that doesn't resolve to anything real (typo, stale
/// reference, or an id from a row that was later deleted) must not break
/// the lookup at resolve time — `find_linked_direct_chat_row` simply finds
/// nothing to auto-resolve, and the direct_chat row (created independently,
/// exactly as before) stays pending on its own until a human resolves it
/// directly.
#[tokio::test]
#[serial]
async fn a_nonexistent_hitl_request_id_is_ignored_not_an_error() {
    let server = common::TestServer::start().await;
    let (admin_id, admin_uuid) = init_admin(&server).await;

    let stub_url = start_stub_agent(json!({
        "hitl_request_id": Uuid::new_v4().to_string(),
    }))
    .await;
    let chat_agent_id =
        seed_running_agent(&server.db, admin_uuid, "bogus-id-chat-agent", &stub_url).await;

    let req = server.client.post(server.url("/api/orchestrator/a2a"));
    let res = common::as_superuser(req, &admin_id, "admin")
        .json(&dispatch_body(chat_agent_id))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());
    let _ = res.bytes().await;

    let direct_chat_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM hitl_requests WHERE origin = 'direct_chat'")
            .fetch_one(&server.db)
            .await
            .unwrap();
    assert_eq!(
        direct_chat_rows, 1,
        "a bogus hitl_request_id must not prevent the direct_chat row from persisting"
    );

    server.cleanup().await;
}
