//! Regression test for persisting `--writable` on the ad-hoc `POST /api/containers`
//! deploy path.
//!
//! That path used to mount the volume from the request without recording it on
//! the `agents` row, so every later restart/update/rollback — all of which read
//! the flag back from the row — silently dropped the mount and lost the agent's
//! files. It now sources the flag from the catalog and writes the effective
//! value back.
//!
//! (Surfacing `--writable` on the *other* on-ramps — GitHub clone, and the
//! catalog toggle/detail wire shape — is a separate change and is not covered
//! here.)
//!
//! Requires infra (Postgres :5432, Redis, S3):
//!   cargo test -p nasiko-server --test writable_deploy -- --test-threads=1

mod common;

use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

async fn init_admin(server: &common::TestServer) -> Uuid {
    let body: Value = server
        .client
        .post(server.url("/api/auth/initialize-admin"))
        .json(&json!({"username": "admin", "email": "admin@test.local"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["user_id"].as_str().unwrap().parse().unwrap()
}

async fn seed_agent(server: &common::TestServer, owner_id: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO agents (name, owner_id, image, status) \
         VALUES ($1, $2, 'nasiko/echo:1.0.0', 'running') RETURNING id",
    )
    .bind(name)
    .bind(owner_id)
    .fetch_one(&server.db)
    .await
    .unwrap()
}

async fn stored(server: &common::TestServer, agent_id: Uuid) -> (bool, Option<String>) {
    sqlx::query_as::<_, (bool, Option<String>)>(
        "SELECT writable, writable_path FROM agents WHERE id = $1",
    )
    .bind(agent_id)
    .fetch_one(&server.db)
    .await
    .unwrap()
}

async fn deploy(server: &common::TestServer, user_id: Uuid, body: Value) -> reqwest::Response {
    common::as_superuser(
        server.client.post(server.url("/api/containers")),
        &user_id.to_string(),
        "admin",
    )
    .json(&body)
    .send()
    .await
    .unwrap()
}

#[tokio::test]
#[serial]
async fn deploy_persists_writable_so_restart_keeps_the_mount() {
    let server = common::TestServer::start().await;
    let uid = init_admin(&server).await;
    let agent_id = seed_agent(&server, uid, "wr-deploy").await;

    let res = deploy(
        &server,
        uid,
        json!({
            "image": "nasiko/echo:1.0.0",
            "name": "wr-deploy",
            "writable": true,
            "writable_path": "/app/data",
        }),
    )
    .await;
    assert!(
        res.status().is_success(),
        "deploy should succeed, got {}",
        res.status()
    );

    // The whole point: restart/update/rollback read these back from the row.
    let (writable, path) = stored(&server, agent_id).await;
    assert!(writable, "deploy --writable must persist writable=true");
    assert_eq!(path.as_deref(), Some("/app/data"));

    server.cleanup().await;
}
