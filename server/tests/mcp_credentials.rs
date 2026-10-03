//! HTTP-level tests for per-user credential management on custom connectors.
//!
//!   cargo test -p nasiko-server --test mcp_credentials -- --test-threads=1

mod common;

use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

fn allow_private_urls() {
    // SAFETY: serialized by `#[serial]`.
    unsafe { std::env::set_var("MCP_ALLOW_PRIVATE_URLS", "true") };
}
fn disallow_private_urls() {
    // SAFETY: serialized by `#[serial]`.
    unsafe { std::env::remove_var("MCP_ALLOW_PRIVATE_URLS") };
}

async fn init_admin(server: &common::TestServer) -> String {
    server
        .client
        .post(server.url("/api/auth/initialize-admin"))
        .json(&json!({"username": "admin", "email": "admin@test.local"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["user_id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn create_user(
    server: &common::TestServer,
    admin_id: &str,
    username: &str,
) -> (String, Uuid) {
    let v = common::as_superuser(
        server.client.post(server.url("/api/users")),
        admin_id,
        "admin",
    )
    .json(&json!({"username": username, "email": format!("{username}@test.local")}))
    .send()
    .await
    .unwrap()
    .json::<Value>()
    .await
    .unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    (id.clone(), Uuid::parse_str(&id).unwrap())
}

/// A real, live MCP backend that answers any JSON-RPC call with an empty
/// `tools/list` result — needed wherever a test registers a credential and
/// expects it to actually verify successfully (`verify_connector_live` makes
/// a genuine call now, unlike before).
async fn start_stub_mcp_server_ok() -> String {
    async fn respond() -> axum::Json<Value> {
        axum::Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}}))
    }
    let app = axum::Router::new().route("/", axum::routing::post(respond));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{port}/")
}

async fn seed_connector(
    server: &common::TestServer,
    owner: Uuid,
    name: &str,
    auth_type: &str,
    url: &str,
) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO mcp_connectors (provider_type, owner_id, name, url, auth_type)
         VALUES ('mcp_server', $1, $2, $3, $4) RETURNING id",
    )
    .bind(owner)
    .bind(name)
    .bind(url)
    .bind(auth_type)
    .fetch_one(&server.db)
    .await
    .unwrap()
}

#[tokio::test]
#[serial]
async fn register_status_and_delete_credential() {
    allow_private_urls();
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let admin_uuid = Uuid::parse_str(&admin).unwrap();
    let backend_url = start_stub_mcp_server_ok().await;
    let cid = seed_connector(&server, admin_uuid, "cred-tool", "bearer", &backend_url).await;

    // Register.
    let res = common::as_superuser(
        server
            .client
            .post(server.url(&format!("/api/mcp/connectors/{cid}/credential"))),
        &admin,
        "admin",
    )
    .json(&json!({"value": "sk-abc"}))
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 201);
    assert_eq!(
        res.json::<Value>().await.unwrap()["data"]["connected"],
        true
    );

    // Status: connected.
    let body: Value = common::as_superuser(
        server
            .client
            .get(server.url(&format!("/api/mcp/connectors/{cid}/credential/status"))),
        &admin,
        "admin",
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(body["data"]["connected"], true);
    assert_eq!(body["data"]["auth_type"], "bearer");

    // Delete → 200 (envelope), then status: not connected.
    let res = common::as_superuser(
        server
            .client
            .delete(server.url(&format!("/api/mcp/connectors/{cid}/credential"))),
        &admin,
        "admin",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 200);

    let body: Value = common::as_superuser(
        server
            .client
            .get(server.url(&format!("/api/mcp/connectors/{cid}/credential/status"))),
        &admin,
        "admin",
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(body["data"]["connected"], false);

    disallow_private_urls();
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn register_credential_on_inaccessible_connector_forbidden() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let (_alice_id, alice_uuid) = create_user(&server, &admin, "cr-alice").await;
    let (bob_id, _) = create_user(&server, &admin, "cr-bob").await;
    let cid = seed_connector(
        &server,
        alice_uuid,
        "alice-cred-tool",
        "bearer",
        "https://example.com",
    )
    .await;

    // Bob can't reach alice's private connector.
    let res = common::as_member(
        server
            .client
            .post(server.url(&format!("/api/mcp/connectors/{cid}/credential"))),
        &bob_id,
        "cr-bob",
    )
    .json(&json!({"value": "x"}))
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 403);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn register_credential_on_none_auth_is_bad_request() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let admin_uuid = Uuid::parse_str(&admin).unwrap();
    let cid = seed_connector(
        &server,
        admin_uuid,
        "noauth-tool",
        "none",
        "https://example.com",
    )
    .await;

    let res = common::as_superuser(
        server
            .client
            .post(server.url(&format!("/api/mcp/connectors/{cid}/credential"))),
        &admin,
        "admin",
    )
    .json(&json!({"value": "x"}))
    .send()
    .await
    .unwrap();
    assert_eq!(
        res.status(),
        400,
        "credentials only apply to bearer/basic/url_param"
    );

    server.cleanup().await;
}

/// The third instance of the same auto-resolve gap the generic OAuth2 and
/// Composio callbacks had: `credentials.rs::register_credential` — how a
/// bearer/basic connector's credential actually gets fixed, since this
/// connector type has no OAuth callback at all — verifies the new
/// credential live but, before this fix, never told HITL about it either.
#[tokio::test]
#[serial]
async fn registering_a_working_credential_auto_resolves_pending_auth_required_hitl_row() {
    allow_private_urls();
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let admin_uuid = Uuid::parse_str(&admin).unwrap();
    let backend_url = start_stub_mcp_server_ok().await;
    let cid = seed_connector(
        &server,
        admin_uuid,
        "cred-auto-resolve-tool",
        "bearer",
        &backend_url,
    )
    .await;

    let agent_id: Uuid =
        sqlx::query_scalar("INSERT INTO agents (name, owner_id) VALUES ($1, $2) RETURNING id")
            .bind("cred-auto-resolve-agent")
            .bind(admin_uuid)
            .fetch_one(&server.db)
            .await
            .unwrap();

    let pending = nasiko_hitl::repo::create_pending_auth_required(
        &server.db,
        nasiko_hitl::NewAuthRequired {
            agent_id,
            owner_user_id: admin_uuid,
            connector_id: cid,
            context_id: "ses_cred_auto_resolve_test".to_string(),
            question: serde_json::json!({"message": "Tool requires re-authentication.", "connector": "cred-auto-resolve-tool"}),
        },
    )
    .await
    .unwrap();
    assert_eq!(pending.status, nasiko_hitl::HitlStatus::Pending);

    let res = common::as_superuser(
        server
            .client
            .post(server.url(&format!("/api/mcp/connectors/{cid}/credential"))),
        &admin,
        "admin",
    )
    .json(&json!({"value": "sk-new-working-token"}))
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 201);
    assert_eq!(
        res.json::<Value>().await.unwrap()["data"]["connected"],
        true
    );

    let (status, human_response): (String, Option<Value>) =
        sqlx::query_as("SELECT status, human_response FROM hitl_requests WHERE id = $1")
            .bind(pending.id)
            .fetch_one(&server.db)
            .await
            .unwrap();
    assert_eq!(
        status, "resolved",
        "the pending auth_required row must auto-resolve once a working credential is registered"
    );
    assert_eq!(
        human_response.as_ref().and_then(|v| v["decision"].as_str()),
        Some("approve")
    );

    disallow_private_urls();
    server.cleanup().await;
}
