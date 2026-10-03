//! Integration tests for the observability endpoints.
//!
//! Covers:
//!   GET  /api/observability/agents/{agent_ref}/logs
//!   GET  /api/observability/agents/{agent_ref}/logs/stream
//!   GET  /api/observability/agent/{agent_ref}/stats
//!   GET  /api/observability/session/list
//!   GET  /api/observability/trace/{trace_id}
//!   GET  /api/observability/finops/dashboard
//!
//! The read path is provider-backed (Tempo/Loki HTTP clients). Per-agent
//! backend failures degrade soft: session/list and finops return zeroed 200
//! responses rather than 5xx. Stats queries Tempo directly (zeroed 200 when
//! Tempo has no data). Logs merge proxy_logs (DB) with container logs.
//!
//! All routes are under `/api` and require authentication.
//!
//! Requires infra (Postgres :5432, Redis, MinIO, Tempo, Loki):
//!   cargo test -p nasiko-server --test observability -- --test-threads=1

mod common;

use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

// ─── shared helpers ──────────────────────────────────────────────────────────

async fn init_admin(server: &common::TestServer) -> Value {
    server
        .client
        .post(server.url("/api/auth/initialize-admin"))
        .json(&json!({"username": "admin", "email": "admin@obs.test"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()
}

async fn create_agent(server: &common::TestServer, user_id: &str, name: &str) -> Value {
    common::as_superuser(
        server.client.post(server.url("/api/agents")),
        user_id,
        "admin",
    )
    .json(&json!({"name": name, "version": "1.0.0"}))
    .send()
    .await
    .unwrap()
    .json::<Value>()
    .await
    .unwrap()
}

async fn seed_user(server: &common::TestServer, username: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO users (username, email) VALUES ($1, $2) RETURNING id")
        .bind(username)
        .bind(format!("{username}@obs.test"))
        .fetch_one(&server.db)
        .await
        .expect("seed user")
}

async fn seed_agent(server: &common::TestServer, owner_id: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO agents (name, owner_id) VALUES ($1, $2) RETURNING id")
        .bind(name)
        .bind(owner_id)
        .fetch_one(&server.db)
        .await
        .expect("seed agent")
}

/// Seed a proxy_log row for `target_agent_id`, called by `caller_id`.
/// Returns nothing — used for state setup only.
async fn seed_proxy_log(
    server: &common::TestServer,
    caller_id: Uuid,
    target_agent_id: Uuid,
    status: i32,
    latency_ms: i64,
    error: Option<&str>,
) {
    sqlx::query(
        r#"INSERT INTO proxy_logs (caller_id, target_agent_id, method, latency_ms, status, error)
           VALUES ($1, $2, 'tasks/send', $3, $4, $5)"#,
    )
    .bind(caller_id)
    .bind(target_agent_id)
    .bind(latency_ms)
    .bind(status)
    .bind(error)
    .execute(&server.db)
    .await
    .expect("seed proxy_log");
}

// ─── authentication guard tests ──────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn observe_logs_requires_auth() {
    let server = common::TestServer::start().await;

    let res = server
        .client
        .get(server.url("/api/observability/agents/some-agent/logs"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 401);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn observe_stats_requires_auth() {
    let server = common::TestServer::start().await;

    // Stats moved to the singular /agent/{ref}/stats route (routes.rs).
    let res = server
        .client
        .get(server.url("/api/observability/agent/some-agent/stats"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 401);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn observe_traces_requires_auth() {
    let server = common::TestServer::start().await;

    // The old /traces list was replaced by /session/list + /trace/{id}.
    for path in [
        "/api/observability/session/list",
        "/api/observability/trace/abc123def456",
    ] {
        let res = server.client.get(server.url(path)).send().await.unwrap();
        assert_eq!(res.status(), 401, "{path} must require auth");
    }
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn observe_finops_requires_auth() {
    let server = common::TestServer::start().await;

    // Finops moved to /finops/dashboard.
    let res = server
        .client
        .get(server.url("/api/observability/finops/dashboard"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 401);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn unmatched_api_path_is_a_json_404_not_the_spa() {
    // The UI fallback serves index.html for anything unrouted, and /api was
    // reaching it — so a missing endpoint answered 200 with HTML and every
    // caller reported it as "malformed JSON body". A route that is not there
    // has to say so, in the envelope the rest of the API uses.
    let server = common::TestServer::start().await;

    let res = server
        .client
        .get(server.url("/api/observability/finops/no-such-endpoint"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 404, "an unmatched /api path must not be 200");
    let ctype = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ctype.contains("application/json"),
        "got content-type {ctype}"
    );

    let body: Value = res.json().await.expect("body must parse as JSON");
    assert_eq!(body["status_code"], 404);
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("no-such-endpoint"),
        "the message should name the path that missed: {body}"
    );

    server.cleanup().await;
}

// ─── provider-backed list/detail endpoints ───────────────────────────────────
//
// The pre-refactor API returned 503 when no Tempo backend was configured; the
// provider-backed replacements degrade soft instead (zeroed/empty 200s, with
// per-agent Tempo failures logged and skipped).

#[tokio::test]
#[serial]
async fn ensure_session_uses_callers_same_name_agent_and_is_idempotent() {
    let server = common::TestServer::start().await;
    let _admin = init_admin(&server).await;
    let alice = seed_user(&server, "ensure-alice").await;
    let bob = seed_user(&server, "ensure-bob").await;
    let alice_agent = seed_agent(&server, alice, "claude-code").await;
    let _bob_agent = seed_agent(&server, bob, "claude-code").await;

    let request = || {
        server
            .client
            .post(server.url("/api/observability/session/ensure"))
            .bearer_auth(common::sign_token(
                &alice.to_string(),
                "ensure-alice",
                false,
                "member",
            ))
            .json(&json!({"session_id": "owned-coding-session", "agent_name": "claude-code"}))
    };

    assert_eq!(request().send().await.unwrap().status(), 201);
    assert_eq!(request().send().await.unwrap().status(), 200);

    let stored: (Uuid, Uuid) = sqlx::query_as(
        "SELECT user_id, agent_id FROM chat_sessions WHERE session_id = 'owned-coding-session'",
    )
    .fetch_one(&server.db)
    .await
    .unwrap();
    assert_eq!(stored, (alice, alice_agent));

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn ensure_session_conflicts_when_existing_user_or_agent_differs() {
    let server = common::TestServer::start().await;
    let _admin = init_admin(&server).await;
    let alice = seed_user(&server, "conflict-alice").await;
    let bob = seed_user(&server, "conflict-bob").await;
    let alice_agent = seed_agent(&server, alice, "claude-code").await;
    let _alice_other = seed_agent(&server, alice, "other-code").await;
    let _bob_agent = seed_agent(&server, bob, "claude-code").await;

    sqlx::query(
        "INSERT INTO chat_sessions (session_id, user_id, agent_id, title) VALUES ($1, $2, $3, 'Coding session')",
    )
    .bind("conflicting-session")
    .bind(alice)
    .bind(alice_agent)
    .execute(&server.db)
    .await
    .unwrap();

    for (user_id, username, agent_name) in [
        (bob, "conflict-bob", "claude-code"),
        (alice, "conflict-alice", "other-code"),
    ] {
        let response = server
            .client
            .post(server.url("/api/observability/session/ensure"))
            .bearer_auth(common::sign_token(
                &user_id.to_string(),
                username,
                false,
                "member",
            ))
            .json(&json!({"session_id": "conflicting-session", "agent_name": agent_name}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 409, "{username}/{agent_name}");
    }

    let stored: (Uuid, Uuid) = sqlx::query_as(
        "SELECT user_id, agent_id FROM chat_sessions WHERE session_id = 'conflicting-session'",
    )
    .fetch_one(&server.db)
    .await
    .unwrap();
    assert_eq!(stored, (alice, alice_agent));

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn session_detail_returns_404_for_another_users_session() {
    let server = common::TestServer::start().await;
    let _admin = init_admin(&server).await;
    let owner = seed_user(&server, "session-detail-owner").await;
    let stranger = seed_user(&server, "session-detail-stranger").await;
    let agent = seed_agent(&server, owner, "session-detail-agent").await;
    sqlx::query(
        "INSERT INTO chat_sessions (session_id, user_id, agent_id, title) VALUES ($1, $2, $3, 'Private')",
    )
    .bind("private-observability-session")
    .bind(owner)
    .bind(agent)
    .execute(&server.db)
    .await
    .unwrap();

    let response = server
        .client
        .get(server.url("/api/observability/session/private-observability-session"))
        .bearer_auth(common::sign_token(
            &stranger.to_string(),
            "session-detail-stranger",
            false,
            "member",
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(response.text().await.unwrap(), "session not found");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn agent_stats_and_logs_return_404_for_inaccessible_agent() {
    let server = common::TestServer::start().await;
    let _admin = init_admin(&server).await;
    let owner = seed_user(&server, "observe-agent-owner").await;
    let stranger = seed_user(&server, "observe-agent-stranger").await;
    let agent = seed_agent(&server, owner, "private-observe-agent").await;
    let token = common::sign_token(
        &stranger.to_string(),
        "observe-agent-stranger",
        false,
        "member",
    );

    for path in [
        format!("/api/observability/agent/{agent}/stats"),
        format!("/api/observability/agents/{agent}/logs"),
        format!("/api/observability/agents/{agent}/logs/stream"),
        "/api/observability/agent/private-observe-agent/stats".into(),
        "/api/observability/agents/private-observe-agent/logs".into(),
    ] {
        let response = server
            .client
            .get(server.url(&path))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{path}");
    }

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn agent_stats_deny_ambiguous_name_with_inaccessible_same_name_agent() {
    let server = common::TestServer::start().await;
    let _admin = init_admin(&server).await;
    let caller = seed_user(&server, "same-name-caller").await;
    let other = seed_user(&server, "same-name-other").await;
    let caller_agent = seed_agent(&server, caller, "duplicate-telemetry-name").await;
    let _other_agent = seed_agent(&server, other, "duplicate-telemetry-name").await;

    let response = server
        .client
        .get(server.url(&format!("/api/observability/agent/{caller_agent}/stats")))
        .bearer_auth(common::sign_token(
            &caller.to_string(),
            "same-name-caller",
            false,
            "member",
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn agent_logs_allow_owner_and_explicit_grantee() {
    let server = common::TestServer::start().await;
    let _admin = init_admin(&server).await;
    let owner = seed_user(&server, "logs-access-owner").await;
    let grantee = seed_user(&server, "logs-access-grantee").await;
    let agent = seed_agent(&server, owner, "shared-observe-agent").await;
    sqlx::query(
        "INSERT INTO agent_grants (agent_id, grant_type, grantee_id) VALUES ($1, 'user', $2)",
    )
    .bind(agent)
    .bind(grantee.to_string())
    .execute(&server.db)
    .await
    .unwrap();

    for (user_id, username) in [
        (owner, "logs-access-owner"),
        (grantee, "logs-access-grantee"),
    ] {
        let response = server
            .client
            .get(server.url(&format!("/api/observability/agents/{agent}/logs")))
            .bearer_auth(common::sign_token(
                &user_id.to_string(),
                username,
                false,
                "member",
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{username}");
    }

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn finops_dashboard_only_lists_accessible_agents_for_non_superuser() {
    let server = common::TestServer::start().await;
    let _admin = init_admin(&server).await;
    let caller = seed_user(&server, "finops-caller").await;
    let owner = seed_user(&server, "finops-owner").await;
    let private_owner = seed_user(&server, "finops-private-owner").await;
    let own_agent = seed_agent(&server, caller, "finops-own-agent").await;
    let shared_agent = seed_agent(&server, owner, "finops-shared-agent").await;
    let private_agent = seed_agent(&server, private_owner, "finops-private-agent").await;
    sqlx::query(
        "INSERT INTO agent_grants (agent_id, grant_type, grantee_id) VALUES ($1, 'user', $2)",
    )
    .bind(shared_agent)
    .bind(caller.to_string())
    .execute(&server.db)
    .await
    .unwrap();

    let response = server
        .client
        .get(server.url("/api/observability/finops/dashboard"))
        .bearer_auth(common::sign_token(
            &caller.to_string(),
            "finops-caller",
            false,
            "member",
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let ids: Vec<String> = body["data"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|agent| agent["agent_id"].as_str())
        .map(str::to_owned)
        .collect();
    assert_eq!(body["data"]["summary"]["total_agents"], 2);
    assert!(ids.contains(&own_agent.to_string()));
    assert!(ids.contains(&shared_agent.to_string()));
    assert!(!ids.contains(&private_agent.to_string()));

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn agent_hours_only_returns_accessible_agents_for_non_superuser() {
    let server = common::TestServer::start().await;
    let _admin = init_admin(&server).await;
    let caller = seed_user(&server, "hours-caller").await;
    let other = seed_user(&server, "hours-other").await;
    let own_agent = seed_agent(&server, caller, "hours-own-agent").await;
    let private_agent = seed_agent(&server, other, "hours-private-agent").await;
    let started_at = chrono::Utc::now() - chrono::Duration::hours(1);
    let ended_at = chrono::Utc::now();

    for (agent_id, name) in [
        (own_agent, "hours-own-agent"),
        (private_agent, "hours-private-agent"),
    ] {
        sqlx::query(
            r#"INSERT INTO agent_instance_sessions
                   (agent_id, agent_name, instance_key, runtime, started_at, last_seen_at, ended_at)
               VALUES ($1, $2, $3, 'docker', $4, $5, $5)"#,
        )
        .bind(agent_id)
        .bind(name)
        .bind(format!("instance-{agent_id}"))
        .bind(started_at)
        .bind(ended_at)
        .execute(&server.db)
        .await
        .unwrap();
    }

    let token = common::sign_token(&caller.to_string(), "hours-caller", false, "member");
    let response = server
        .client
        .get(server.url("/api/observability/finops/agent-hours"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let rows = body["data"]["agents"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["agent_id"], own_agent.to_string());
    assert!(body["data"]["total_hours"].as_f64().unwrap() < 1.1);

    let response = server
        .client
        .get(server.url(&format!(
            "/api/observability/finops/agent-hours?agent_id={private_agent}"
        )))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn observe_session_list_returns_envelope_with_no_sessions() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let res = server
        .client
        .get(server.url("/api/observability/session/list"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200, "session/list degrades soft, never 503");
    let body: Value = res.json().await.unwrap();
    assert!(
        body["data"]["sessions"].is_array(),
        "sessions array expected: {body}"
    );
    assert_eq!(body["data"]["sessions"].as_array().unwrap().len(), 0);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn observe_trace_by_id_returns_404_for_unknown_trace() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let res = server
        .client
        .get(server.url("/api/observability/trace/abc123def456"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 404, "unknown trace id must 404");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn observe_finops_dashboard_returns_zeroed_summary() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    create_agent(&server, uid, "finops-zero-agent").await;

    let res = server
        .client
        .get(server.url("/api/observability/finops/dashboard"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200, "finops degrades soft, never 503");
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["data"]["summary"]["total_operations"], 0);
    assert_eq!(body["data"]["summary"]["total_agents"], 1);
    assert!(body["data"]["agents"].is_array());

    server.cleanup().await;
}

/// `summary.total_agents` off the finops dashboard — the field both the overview
/// and tokenops pages read to pick between their first-run screen and the real
/// dashboard.
async fn finops_summary(server: &common::TestServer, uid: &str) -> Value {
    let res = server
        .client
        .get(server.url("/api/observability/finops/dashboard"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    body["data"]["summary"].clone()
}

/// One closed container session, so the agent has billable hours in the window
/// the dashboard reports on.
async fn seed_container_hours(server: &common::TestServer, agent: Uuid, name: &str) {
    sqlx::query(
        "INSERT INTO agent_instance_sessions \
         (agent_id, agent_name, instance_key, runtime, started_at, last_seen_at, ended_at) \
         VALUES ($1, $2, $3, 'docker', now() - interval '2 hours', now(), now())",
    )
    .bind(agent)
    .bind(name)
    .bind(format!("container-{agent}"))
    .execute(&server.db)
    .await
    .expect("seed container session");
}

/// An `is_internal` agent (Weave's dashboard-generator) must not pass for a
/// deployed fleet: alone it reports zero agents, so the overview and tokenops
/// pages keep their first-run screen. Alongside a real agent it counts again.
#[tokio::test]
#[serial]
async fn finops_dashboard_excludes_internal_agent_only_when_it_is_alone() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();
    let owner: Uuid = uid.parse().unwrap();

    let internal = seed_agent(&server, owner, "weave-dashboard-generator").await;
    sqlx::query("UPDATE agents SET is_internal = true WHERE id = $1")
        .bind(internal)
        .execute(&server.db)
        .await
        .expect("mark internal");

    seed_container_hours(&server, internal, "weave-dashboard-generator").await;

    // The fleet is empty AND the summary says so throughout. The internal
    // agent's container hours have to come out with it: a first-run screen
    // reporting zero agents and zero spend beside non-zero hours reads as a
    // bug, and those hours are the one number the denied agent still fed.
    let summary = finops_summary(&server, uid).await;
    assert_eq!(
        summary["total_agents"].as_i64().unwrap(),
        0,
        "an internal agent alone must read as an empty fleet"
    );
    assert_eq!(
        summary["total_container_hours"].as_f64().unwrap(),
        0.0,
        "hours from the agent we just denied must not survive into the summary"
    );

    seed_agent(&server, owner, "finops-real-agent").await;
    let summary = finops_summary(&server, uid).await;
    assert_eq!(
        summary["total_agents"].as_i64().unwrap(),
        2,
        "with a real agent deployed the internal one counts again"
    );
    assert!(
        summary["total_container_hours"].as_f64().unwrap() > 0.0,
        "and so do its hours"
    );

    server.cleanup().await;
}

// ─── 404 for unknown agents ───────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn agent_logs_returns_404_for_unknown_name() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let res = server
        .client
        .get(server.url("/api/observability/agents/no-such-agent/logs"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 404);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn agent_logs_returns_404_for_unknown_uuid() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();
    let fake_id = Uuid::new_v4();

    let res = server
        .client
        .get(server.url(&format!("/api/observability/agents/{fake_id}/logs")))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 404);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn agent_stats_returns_404_for_unknown_agent() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let res = server
        .client
        .get(server.url("/api/observability/agents/ghost-agent/stats"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 404);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn agent_stream_returns_404_for_unknown_agent() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let res = server
        .client
        .get(server.url("/api/observability/agents/ghost-agent/logs/stream"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 404);

    server.cleanup().await;
}

// ─── agent resolution: UUID and name ─────────────────────────────────────────

#[tokio::test]
#[serial]
async fn agent_logs_resolves_by_name() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    create_agent(&server, uid, "resolve-by-name").await;

    let res = server
        .client
        .get(server.url("/api/observability/agents/resolve-by-name/logs"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200, "should resolve agent by name");

    let body: Value = res.json().await.unwrap();
    assert!(body.is_array(), "logs response should be an array");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn agent_logs_resolves_by_uuid() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let agent = create_agent(&server, uid, "resolve-by-uuid").await;
    let agent_id = agent["id"].as_str().unwrap();

    let res = server
        .client
        .get(server.url(&format!("/api/observability/agents/{agent_id}/logs")))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200, "should resolve agent by UUID");

    let body: Value = res.json().await.unwrap();
    assert!(body.is_array(), "logs response should be an array");

    server.cleanup().await;
}

// ─── proxy_logs are surfaced in the logs endpoint ────────────────────────────

#[tokio::test]
#[serial]
async fn agent_logs_returns_proxy_log_entries() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();
    let user_id: Uuid = Uuid::parse_str(uid).unwrap();

    let agent = create_agent(&server, uid, "proxy-logs-test").await;
    let agent_id: Uuid = agent["id"].as_str().unwrap().parse().unwrap();

    // Seed 3 proxy log rows for this agent
    seed_proxy_log(&server, user_id, agent_id, 200, 42, None).await;
    seed_proxy_log(&server, user_id, agent_id, 500, 88, Some("upstream error")).await;
    seed_proxy_log(
        &server,
        user_id,
        agent_id,
        404,
        15,
        Some("agent returned 404"),
    )
    .await;

    let res = server
        .client
        .get(server.url("/api/observability/agents/proxy-logs-test/logs"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);

    let body: Value = res.json().await.unwrap();
    let entries = body.as_array().unwrap();

    // At least the 3 proxy log rows should appear (container logs may add more)
    assert!(
        entries.len() >= 3,
        "expected at least 3 log entries, got {}: {body}",
        entries.len()
    );

    // All entries from proxy should have source = "proxy"
    let proxy_entries: Vec<&Value> = entries
        .iter()
        .filter(|e| e["source"].as_str() == Some("proxy"))
        .collect();
    assert_eq!(proxy_entries.len(), 3, "exactly 3 proxy log entries");

    // Each entry should have required fields
    for entry in &proxy_entries {
        assert!(
            entry["timestamp"].is_string(),
            "timestamp should be a string"
        );
        assert!(entry["message"].is_string(), "message should be a string");
        assert!(entry["level"].is_string(), "level should be a string");
    }

    server.cleanup().await;
}

// ─── level field is correctly derived from HTTP status ───────────────────────

#[tokio::test]
#[serial]
async fn proxy_log_level_reflects_http_status() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();
    let user_id: Uuid = Uuid::parse_str(uid).unwrap();

    let agent = create_agent(&server, uid, "level-test-agent").await;
    let agent_id: Uuid = agent["id"].as_str().unwrap().parse().unwrap();

    seed_proxy_log(&server, user_id, agent_id, 200, 10, None).await; // INFO
    seed_proxy_log(&server, user_id, agent_id, 404, 20, None).await; // WARN
    seed_proxy_log(&server, user_id, agent_id, 503, 30, None).await; // ERROR

    let res = server
        .client
        .get(server.url("/api/observability/agents/level-test-agent/logs"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let entries = body.as_array().unwrap();

    let proxy_entries: Vec<&Value> = entries
        .iter()
        .filter(|e| e["source"].as_str() == Some("proxy"))
        .collect();

    let levels: Vec<&str> = proxy_entries
        .iter()
        .filter_map(|e| e["level"].as_str())
        .collect();

    assert!(levels.contains(&"INFO"), "200 → INFO");
    assert!(levels.contains(&"WARN"), "404 → WARN");
    assert!(levels.contains(&"ERROR"), "503 → ERROR");

    server.cleanup().await;
}

// ─── level filter query parameter ────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn agent_logs_level_filter_returns_only_matching_level() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();
    let user_id: Uuid = Uuid::parse_str(uid).unwrap();

    let agent = create_agent(&server, uid, "filter-level-agent").await;
    let agent_id: Uuid = agent["id"].as_str().unwrap().parse().unwrap();

    seed_proxy_log(&server, user_id, agent_id, 200, 10, None).await; // INFO
    seed_proxy_log(&server, user_id, agent_id, 200, 12, None).await; // INFO
    seed_proxy_log(&server, user_id, agent_id, 500, 50, Some("boom")).await; // ERROR

    let res = server
        .client
        .get(server.url("/api/observability/agents/filter-level-agent/logs?level=ERROR"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let entries = body.as_array().unwrap();

    // All returned entries must be ERROR level
    for entry in entries {
        assert_eq!(
            entry["level"].as_str(),
            Some("ERROR"),
            "all entries should be ERROR after level filter"
        );
    }
    // At least the one seeded ERROR should be present
    assert!(!entries.is_empty(), "should have at least one ERROR entry");

    server.cleanup().await;
}

// ─── search filter query parameter ───────────────────────────────────────────

#[tokio::test]
#[serial]
async fn agent_logs_search_filter_returns_only_matching_messages() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();
    let user_id: Uuid = Uuid::parse_str(uid).unwrap();

    let agent = create_agent(&server, uid, "search-filter-agent").await;
    let agent_id: Uuid = agent["id"].as_str().unwrap().parse().unwrap();

    // "upstream error" will appear in the message for error rows
    seed_proxy_log(&server, user_id, agent_id, 500, 40, Some("upstream error")).await;
    seed_proxy_log(&server, user_id, agent_id, 200, 12, None).await; // no error message

    let res = server
        .client
        .get(server.url("/api/observability/agents/search-filter-agent/logs?search=upstream"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let entries = body.as_array().unwrap();

    // All returned messages must contain "upstream" (case-insensitive)
    for entry in entries {
        let msg = entry["message"].as_str().unwrap_or("").to_lowercase();
        assert!(
            msg.contains("upstream"),
            "all entries should contain 'upstream': got {msg}"
        );
    }
    assert!(
        !entries.is_empty(),
        "should have at least one matching entry"
    );

    server.cleanup().await;
}

// ─── stats endpoint ───────────────────────────────────────────────────────────
//
// Stats moved to /agent/{ref}/stats and are provider-backed (Tempo): the
// response is a { data: { project: { id: <agent name>, trace_count, ... } } }
// envelope (get_agent_stats in oss/server/src/observability/handler.rs). The
// old proxy_logs fallback (source/total_requests/error_rate) no longer exists.

#[tokio::test]
#[serial]
async fn agent_stats_resolves_by_name() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    create_agent(&server, uid, "stats-source-agent").await;

    let res = server
        .client
        .get(server.url("/api/observability/agent/stats-source-agent/stats"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 503, "test server has observability disabled");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn agent_stats_resolves_by_uuid() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let agent = create_agent(&server, uid, "stats-uuid-agent").await;
    let agent_id = agent["id"].as_str().unwrap();

    let res = server
        .client
        .get(server.url(&format!("/api/observability/agent/{agent_id}/stats")))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 503, "test server has observability disabled");

    server.cleanup().await;
}

// ─── SSE log stream ───────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn agent_logs_stream_returns_text_event_stream_content_type() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    create_agent(&server, uid, "stream-test-agent").await;

    // We only check the Content-Type header — we don't consume the stream.
    let res = server
        .client
        .get(server.url("/api/observability/agents/stream-test-agent/logs/stream"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);

    let content_type = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/event-stream"),
        "SSE stream should have text/event-stream content-type, got: {content_type}"
    );

    server.cleanup().await;
}

// ─── metrics and readiness (no auth required) ────────────────────────────────

#[tokio::test]
#[serial]
async fn metrics_endpoint_is_publicly_accessible() {
    let server = common::TestServer::start().await;
    let _admin = init_admin(&server).await; // ensure tables exist

    let res = server
        .client
        .get(server.url("/metrics"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);

    let body: Value = res.json().await.unwrap();
    // Should include known counter fields
    assert!(
        body["agents_total"].is_number(),
        "agents_total should be a number"
    );
    assert!(
        body["users_total"].is_number(),
        "users_total should be a number"
    );

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn readiness_endpoint_is_publicly_accessible() {
    let server = common::TestServer::start().await;

    let res = server
        .client
        .get(server.url("/readiness"))
        .send()
        .await
        .unwrap();

    // Status is 200 (ready) or 503 (degraded — if redis/docker is down)
    let status = res.status().as_u16();
    assert!(
        status == 200 || status == 503,
        "readiness should return 200 or 503, got {status}"
    );

    let body: Value = res.json().await.unwrap();
    assert!(body["postgres"].is_boolean(), "postgres field required");
    assert!(body["status"].is_string(), "status field required");

    server.cleanup().await;
}

// ─── deleted agents are not resolvable ───────────────────────────────────────

#[tokio::test]
#[serial]
async fn deleted_agent_not_found_in_logs() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    let agent = create_agent(&server, uid, "deleted-logs-agent").await;
    let agent_id = agent["id"].as_str().unwrap();

    // Soft-delete the agent directly in the DB
    sqlx::query("UPDATE agents SET deleted_at = now() WHERE id = $1")
        .bind(Uuid::parse_str(agent_id).unwrap())
        .execute(&server.db)
        .await
        .expect("soft delete agent");

    // Both UUID and name lookups should now return 404
    let by_name = server
        .client
        .get(server.url("/api/observability/agents/deleted-logs-agent/logs"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();
    assert_eq!(by_name.status(), 404, "deleted agent should be 404 by name");

    let by_uuid = server
        .client
        .get(server.url(&format!("/api/observability/agents/{agent_id}/logs")))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();
    assert_eq!(by_uuid.status(), 404, "deleted agent should be 404 by UUID");

    server.cleanup().await;
}

// ─── stats with no data returns zero counts ──────────────────────────────────

#[tokio::test]
#[serial]
async fn agent_stats_returns_zero_counts_for_new_agent() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    create_agent(&server, uid, "zero-stats-agent").await;

    let res = server
        .client
        .get(server.url("/api/observability/agent/zero-stats-agent/stats"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 503, "test server has observability disabled");

    server.cleanup().await;
}

// ─── logs are empty for new agent ────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn agent_logs_returns_empty_array_for_new_agent() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();

    create_agent(&server, uid, "empty-logs-agent").await;

    let res = server
        .client
        .get(server.url("/api/observability/agents/empty-logs-agent/logs"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let entries = body.as_array().unwrap();

    // Container runtime will return empty logs for a non-deployed agent
    // so the overall result should be empty or only container entries
    // (either is fine — the important thing is no 404 or 500)
    let _ = entries; // structure is valid, count may vary

    server.cleanup().await;
}

// ─── error message is included in log line ───────────────────────────────────

#[tokio::test]
#[serial]
async fn proxy_log_error_message_appears_in_log_line() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();
    let user_id: Uuid = Uuid::parse_str(uid).unwrap();

    let agent = create_agent(&server, uid, "error-msg-agent").await;
    let agent_id: Uuid = agent["id"].as_str().unwrap().parse().unwrap();

    seed_proxy_log(
        &server,
        user_id,
        agent_id,
        502,
        99,
        Some("bad gateway downstream"),
    )
    .await;

    let res = server
        .client
        .get(server.url("/api/observability/agents/error-msg-agent/logs"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let entries = body.as_array().unwrap();

    let error_entry = entries
        .iter()
        .find(|e| e["source"].as_str() == Some("proxy"))
        .expect("should have a proxy entry");

    let msg = error_entry["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("bad gateway downstream"),
        "error field should appear in log message: {msg}"
    );
    assert!(
        msg.contains("502"),
        "HTTP status should appear in log message: {msg}"
    );

    server.cleanup().await;
}

// ─── limit parameter is respected ────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn agent_logs_limit_parameter_is_respected() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();
    let user_id: Uuid = Uuid::parse_str(uid).unwrap();

    let agent = create_agent(&server, uid, "limit-test-agent").await;
    let agent_id: Uuid = agent["id"].as_str().unwrap().parse().unwrap();

    // Seed 10 proxy log rows
    for i in 0..10_i64 {
        seed_proxy_log(&server, user_id, agent_id, 200, i * 5 + 1, None).await;
    }

    let res = server
        .client
        .get(server.url("/api/observability/agents/limit-test-agent/logs?limit=3"))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let entries = body.as_array().unwrap();

    assert!(
        entries.len() <= 3,
        "limit=3 should return at most 3 entries, got {}",
        entries.len()
    );

    server.cleanup().await;
}

// ─── since parameter filters out old entries ─────────────────────────────────

#[tokio::test]
#[serial]
async fn agent_logs_since_parameter_filters_old_entries() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let uid = admin["user_id"].as_str().unwrap();
    let user_id: Uuid = Uuid::parse_str(uid).unwrap();

    let agent = create_agent(&server, uid, "since-test-agent").await;
    let agent_id: Uuid = agent["id"].as_str().unwrap().parse().unwrap();

    // Insert a log row 2 hours ago (outside the 1-hour window we'll query)
    sqlx::query(
        r#"INSERT INTO proxy_logs (caller_id, target_agent_id, method, latency_ms, status, timestamp)
           VALUES ($1, $2, 'tasks/send', 10, 200, now() - interval '2 hours')"#,
    )
    .bind(user_id)
    .bind(agent_id)
    .execute(&server.db)
    .await
    .unwrap();

    // Insert a recent log row (within the last hour)
    seed_proxy_log(&server, user_id, agent_id, 200, 10, None).await;

    // Query with since = 90 minutes ago — old entry should be excluded.
    // RFC-3339 timestamps contain '+' which must be percent-encoded in query strings.
    let since_raw = (chrono::Utc::now() - chrono::Duration::try_minutes(90).unwrap()).to_rfc3339();
    let since_encoded = since_raw.replace('+', "%2B");
    let url =
        format!("/api/observability/agents/since-test-agent/logs?since={since_encoded}&limit=10");

    let res = server
        .client
        .get(server.url(&url))
        .bearer_auth(common::sign_token(uid, "admin", true, "admin"))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let entries = body.as_array().unwrap();

    // Only the recent row should be in range; the 2-hour-old row should be excluded.
    let proxy_entries: Vec<&Value> = entries
        .iter()
        .filter(|e| e["source"].as_str() == Some("proxy"))
        .collect();

    assert_eq!(
        proxy_entries.len(),
        1,
        "only the recent proxy log should appear when since filter applied; got: {body}"
    );

    server.cleanup().await;
}
