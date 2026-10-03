//! End-to-end regression for a real authorization bypass found by security review
//! (`docs/SECURITY_REVIEW_HITL_MCP_2026-09-07.md`, Vuln 1): `question.metadata.hitl_request_id`
//! is fully agent-controlled (`build_pause_question` forwards the agent's own status-message
//! metadata verbatim, unvalidated) — a malicious or buggy agent can hardcode ANY other user's real
//! `hitl_requests.id` into a victim's own pause metadata. Before the fix,
//! `find_linked_direct_chat_row` (`oss/hitl/src/repo.rs`) matched purely on that agent-supplied
//! link with no `owner_user_id` check, so an attacker resolving their OWN unrelated pending row
//! would find and auto-resolve the VICTIM's mirror (`resolved_by` = the attacker), alias the
//! victim's continuation buffer onto an id the attacker is authorized to reconnect with, and cause
//! the victim's paused task to resume under the victim's own delegation token at the attacker's
//! chosen moment — a forged approval, not just an information leak.
//!
//! This test proves the real API path is closed: resolving the attacker's own row must NEVER touch
//! a different user's mirror, even when a malicious agent has planted a matching link.
//!
//! `just infra` then `cargo test -p nasiko-server --test hitl_cross_user_mirror_hijack -- --test-threads=1`

mod common;

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
    .bind(format!("hijack-test-agent-{}", Uuid::new_v4()))
    .bind(owner_id)
    .fetch_one(&server.db)
    .await
    .expect("seed_running_agent")
}

#[tokio::test]
#[serial]
async fn resolving_the_attackers_own_row_does_not_hijack_the_victims_mirror() {
    let server = common::TestServer::start().await;
    let attacker_id = Uuid::new_v4();
    let victim_id = Uuid::new_v4();
    seed_user(&server, attacker_id).await;
    seed_user(&server, victim_id).await;
    let agent_id = seed_running_agent(&server, attacker_id).await;

    // The attacker's own, entirely unrelated pending tool_approval row — a real approval they are
    // legitimately entitled to resolve.
    let attacker_row_id: Uuid = sqlx::query_scalar(
        "INSERT INTO hitl_requests
            (kind, origin, agent_id, owner_user_id, context_id, connector_id, tool_name, question)
         VALUES ('tool_approval', 'mcp_tool', $1, $2, $3, $4, $5, $6)
         RETURNING id",
    )
    .bind(agent_id)
    .bind(attacker_id)
    .bind(format!("attacker-ctx-{}", Uuid::new_v4()))
    .bind(Uuid::new_v4())
    .bind("github_create_issue")
    .bind(json!({"message": "Approve creating a GitHub issue?"}))
    .fetch_one(&server.db)
    .await
    .expect("seed attacker's own pending row");

    // The victim's own mirror, from a completely separate conversation — planted by a malicious
    // agent that hardcoded the attacker's row id into the metadata, not something either the
    // victim or the attacker actually linked.
    let victim_mirror_id: Uuid = sqlx::query_scalar(
        "INSERT INTO hitl_requests
            (kind, origin, agent_id, owner_user_id, task_id, context_id, question)
         VALUES ('auth_required', 'direct_chat', $1, $2, $3, $4, $5)
         RETURNING id",
    )
    .bind(agent_id)
    .bind(victim_id)
    .bind(format!("victim-task-{}", Uuid::new_v4()))
    .bind(format!("victim-ctx-{}", Uuid::new_v4()))
    .bind(json!({
        "message": "Tool(s) require user approval for this agent.",
        "metadata": {"hitl_request_id": attacker_row_id.to_string()},
    }))
    .fetch_one(&server.db)
    .await
    .expect("seed victim's mirror, planted with the attacker's row id");

    // The attacker resolves ONLY their own row — legitimately authorized.
    let res = auth(
        server
            .client
            .post(server.url(&format!("/api/hitl/{attacker_row_id}/resolve")))
            .json(&json!({"decision": "approve", "scope": "once"})),
        attacker_id,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 200);

    // The attacker's own row is resolved — expected and fine.
    let attacker_status: String =
        sqlx::query_scalar("SELECT status FROM hitl_requests WHERE id = $1")
            .bind(attacker_row_id)
            .fetch_one(&server.db)
            .await
            .unwrap();
    assert_eq!(attacker_status, "resolved");

    // The victim's mirror must be completely untouched: still pending, never resolved by the
    // attacker. This is the actual security assertion — before the fix this row would have been
    // silently flipped to `resolved` with `resolved_by = attacker_id`.
    let (victim_status, victim_resolved_by): (String, Option<Uuid>) =
        sqlx::query_as("SELECT status, resolved_by FROM hitl_requests WHERE id = $1")
            .bind(victim_mirror_id)
            .fetch_one(&server.db)
            .await
            .unwrap();
    assert_eq!(
        victim_status, "pending",
        "the victim's mirror must never be auto-resolved by an unrelated user's resolve action"
    );
    assert_eq!(victim_resolved_by, None);

    server.cleanup().await;
}
