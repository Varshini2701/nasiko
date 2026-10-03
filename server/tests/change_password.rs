//! Self-service password change (`POST /api/auth/change-password`).
//!
//! The behaviour that matters here is that this route is reachable *without*
//! superuser: before it existed, the only way to set a password was
//! `PUT /api/users/{id}`, which sits behind `require_superuser`, so no ordinary
//! user — and, because the UI blanked superuser rows, not even the admin —
//! could rotate their own credential.

mod common;

use common::{as_member, as_superuser};
use serde_json::{Value, json};
use serial_test::serial;
use uuid::Uuid;

async fn init_admin(server: &common::TestServer) -> Value {
    server
        .client
        .post(server.url("/api/auth/initialize-admin"))
        .json(&json!({"username": "admin", "email": "admin@test.local"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()
}

/// Raw login status — these tests care about rejection, not the body.
async fn login_status(
    server: &common::TestServer,
    username: &str,
    password: &str,
) -> reqwest::StatusCode {
    server
        .client
        .post(server.url("/api/auth/login"))
        .json(&json!({"username": username, "password": password}))
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
#[serial]
async fn change_password_rotates_the_credential() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let id = admin["user_id"].as_str().unwrap();
    let old = admin["access_secret"].as_str().unwrap();

    let res = as_superuser(
        server.client.post(server.url("/api/auth/change-password")),
        id,
        "admin",
    )
    .json(&json!({"current_password": old, "new_password": "A-brand-new-password9"}))
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 200, "change should succeed");

    assert_eq!(
        login_status(&server, "admin", old).await,
        401,
        "the replaced password must stop working"
    );
    assert_eq!(
        login_status(&server, "admin", "A-brand-new-password9").await,
        200,
        "the new password must work"
    );
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn change_password_rejects_a_wrong_current_password() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let id = admin["user_id"].as_str().unwrap();
    let old = admin["access_secret"].as_str().unwrap();

    let res = as_superuser(
        server.client.post(server.url("/api/auth/change-password")),
        id,
        "admin",
    )
    .json(&json!({"current_password": "Not-the-password9", "new_password": "Another-password9"}))
    .send()
    .await
    .unwrap();
    // 403, not 401: the caller is authenticated and merely failed a confirmation
    // factor. A 401 would make the shared apiFetch client treat the session as
    // dead and bounce the user to /login.html on a simple typo.
    assert_eq!(res.status(), 403);

    // The credential must be untouched after a failed attempt.
    assert_eq!(login_status(&server, "admin", old).await, 200);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn change_password_enforces_minimum_length_and_difference() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let id = admin["user_id"].as_str().unwrap();
    let old = admin["access_secret"].as_str().unwrap();

    let short = as_superuser(
        server.client.post(server.url("/api/auth/change-password")),
        id,
        "admin",
    )
    .json(&json!({"current_password": old, "new_password": "short"}))
    .send()
    .await
    .unwrap();
    assert_eq!(
        short.status(),
        400,
        "under the minimum length must be rejected"
    );

    // Each composition rule is refused with its own slug, so a client can point
    // at the rule the user missed instead of restating the whole policy.
    for (password, code) in [
        ("CORRECT-HORSE9", "password_missing_lowercase"),
        ("correct-horse9", "password_missing_uppercase"),
        ("Correct-Horsey", "password_missing_digit"),
        ("CorrectHorse99", "password_missing_symbol"),
    ] {
        let res = as_superuser(
            server.client.post(server.url("/api/auth/change-password")),
            id,
            "admin",
        )
        .json(&json!({"current_password": old, "new_password": password}))
        .send()
        .await
        .unwrap();
        assert_eq!(res.status(), 400, "{password} must be rejected");
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["code"], code, "wrong slug for {password}");
    }

    let same = as_superuser(
        server.client.post(server.url("/api/auth/change-password")),
        id,
        "admin",
    )
    .json(&json!({"current_password": old, "new_password": old}))
    .send()
    .await
    .unwrap();
    assert_eq!(
        same.status(),
        400,
        "reusing the current password is rejected"
    );
    server.cleanup().await;
}

/// The regression this feature exists for: a non-superuser can rotate their own
/// credential. Every pre-existing password-setting path required superuser.
#[tokio::test]
#[serial]
async fn a_non_superuser_can_change_their_own_password() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let admin_id = admin["user_id"].as_str().unwrap();

    let alice = as_superuser(
        server.client.post(server.url("/api/users")),
        admin_id,
        "admin",
    )
    .json(&json!({"username": "alice", "email": "alice@test.local"}))
    .send()
    .await
    .unwrap()
    .json::<Value>()
    .await
    .unwrap();

    let alice_id = alice["user_id"]
        .as_str()
        .or_else(|| alice["id"].as_str())
        .expect("create_user returns the new user id");
    let alice_secret = alice["access_secret"].as_str().unwrap();

    let res = as_member(
        server.client.post(server.url("/api/auth/change-password")),
        alice_id,
        "alice",
    )
    .json(&json!({"current_password": alice_secret, "new_password": "Alice-new-password9"}))
    .send()
    .await
    .unwrap();
    assert_eq!(
        res.status(),
        200,
        "a member must be able to change their own password"
    );

    assert_eq!(
        login_status(&server, "alice", "Alice-new-password9").await,
        200
    );
    server.cleanup().await;
}

/// SSO-provisioned users are inserted with no `user_credentials` row. That must
/// read as a 409 ("no local password"), not a 500 — it becomes reachable the
/// moment an identity provider is wired up.
#[tokio::test]
#[serial]
async fn user_without_local_credentials_gets_a_conflict() {
    let server = common::TestServer::start().await;
    let id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO users (id, username, email, is_superuser, is_active, role)
         VALUES ($1, 'sso-user', 'sso@test.local', false, true, 'member'::user_role)",
    )
    .bind(id)
    .execute(&server.db)
    .await
    .unwrap();

    let res = as_member(
        server.client.post(server.url("/api/auth/change-password")),
        &id.to_string(),
        "sso-user",
    )
    .json(&json!({"current_password": "anything", "new_password": "A-valid-password9"}))
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 409);
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn change_password_requires_authentication() {
    let server = common::TestServer::start().await;
    let res = server
        .client
        .post(server.url("/api/auth/change-password"))
        .json(&json!({"current_password": "x", "new_password": "A-valid-password9"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    server.cleanup().await;
}

/// The handler revokes every session and only then mints the replacement, so the
/// token it hands back must survive. Swapping those two steps is an easy
/// refactor to make and would sign the caller out on the very request that
/// returned them a session — assert the ordering directly against `auth_tokens`,
/// because the HTTP surface alone cannot distinguish the two.
#[tokio::test]
#[serial]
async fn change_password_revokes_old_sessions_but_not_the_new_one() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let id = admin["user_id"].as_str().unwrap();
    let old = admin["access_secret"].as_str().unwrap();
    let user_uuid = uuid::Uuid::parse_str(id).unwrap();

    // A second, pre-existing session that the change must invalidate.
    let stale: Value = server
        .client
        .post(server.url("/api/auth/login"))
        .json(&json!({"username": "admin", "password": old}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let stale_token = stale["token"].as_str().unwrap().to_string();

    let res = as_superuser(
        server.client.post(server.url("/api/auth/change-password")),
        id,
        "admin",
    )
    .json(&json!({"current_password": old, "new_password": "Ordering-is-load-bearing9"}))
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let fresh_token = body["data"]["token"]
        .as_str()
        .expect("data envelope with a token");

    // The pre-existing session is gone.
    let stale_res = server
        .client
        .get(server.url("/api/me"))
        .bearer_auth(&stale_token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        stale_res.status(),
        401,
        "a session established with the old password must be rejected"
    );

    // The replacement is not, which is only true if it was issued after the revoke.
    let fresh_res = server
        .client
        .get(server.url("/api/me"))
        .bearer_auth(fresh_token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        fresh_res.status(),
        200,
        "the token returned by the change must be usable"
    );

    let (revoked, live): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE revoked_at IS NOT NULL), \
                count(*) FILTER (WHERE revoked_at IS NULL) \
         FROM auth_tokens WHERE user_id = $1",
    )
    .bind(user_uuid)
    .fetch_one(&server.db)
    .await
    .unwrap();
    assert!(revoked >= 1, "the old session row must be marked revoked");
    assert_eq!(live, 1, "exactly the replacement session may remain live");

    server.cleanup().await;
}

/// `update_user`'s password branch revokes too — previously only a deactivation
/// did, so an admin resetting a compromised user's password left the attacker's
/// session alive for the rest of its 7-day life.
#[tokio::test]
#[serial]
async fn admin_password_reset_revokes_the_targets_sessions() {
    let server = common::TestServer::start().await;
    let admin = init_admin(&server).await;
    let admin_id = admin["user_id"].as_str().unwrap();

    let created: Value = as_superuser(
        server.client.post(server.url("/api/users")),
        admin_id,
        "admin",
    )
    .json(&json!({"username": "mallory", "email": "mallory@test.local"}))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let target_id = created["id"].as_str().unwrap().to_string();
    let secret = created["access_secret"].as_str().unwrap().to_string();

    let session: Value = server
        .client
        .post(server.url("/api/auth/login"))
        .json(&json!({"username": "mallory", "password": secret}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let victim_token = session["token"].as_str().unwrap().to_string();

    let res = as_superuser(
        server
            .client
            .put(server.url(&format!("/api/users/{target_id}"))),
        admin_id,
        "admin",
    )
    .json(&json!({
        "username": "mallory",
        "email": "mallory@test.local",
        "password": "Reset-by-an-administrator9"
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(res.status(), 200);

    let after = server
        .client
        .get(server.url("/api/me"))
        .bearer_auth(&victim_token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        after.status(),
        401,
        "an admin-initiated password reset must end the target's sessions"
    );

    server.cleanup().await;
}
