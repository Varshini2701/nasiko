//! Shared Postgres fixture for `nasiko-hitl`'s integration tests (`dispatcher.rs`,
//! `origin_isolation.rs`, `repo.rs`) — needs infra up (`just infra` from the repo root; override
//! the admin connection with `TEST_PG_URL`), same convention `oss/server/tests/common` uses. Each
//! call creates and migrates its own scratch database so tests can run concurrently without
//! colliding; nothing here is shared *state* across tests, only shared *setup code*.
//!
//! Each test file wraps this with its own `impl TestDb { ... }` block for file-specific seed
//! helpers (e.g. `seed_resolved_tool_approval`) — inherent impls aren't module-scoped, so this
//! works from any module in the same test binary without needing a wrapper type.

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

pub fn pg_admin_url() -> String {
    std::env::var("TEST_PG_URL")
        .unwrap_or_else(|_| "postgres://nasiko:nasiko@localhost:5432/nasiko_dev".into())
}

/// Fresh, migrated scratch database with one seed user and one seed agent — `hitl_requests`'s
/// `agent_id`/`owner_user_id` are `NOT NULL` foreign keys, so every test needs both to exist
/// before it can insert a row.
pub struct TestDb {
    pub pool: PgPool,
    pub agent_id: Uuid,
    pub owner_user_id: Uuid,
    /// Retained so `Drop` can remove the scratch database — see its own comment.
    db_name: String,
}

impl TestDb {
    /// `prefix` names the scratch database and seed rows (e.g. `"hitl_dispatch_test"`) so a
    /// failure is traceable to the suite that left it behind.
    pub async fn new(prefix: &str) -> Self {
        let pg_admin = pg_admin_url();
        let db_name = format!("nasiko_{prefix}_{}", Uuid::new_v4().simple());

        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&pg_admin)
            .await
            .expect("connect to postgres — is infra up? (set TEST_PG_URL to override; `just infra` starts it)");
        sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
            .execute(&admin)
            .await
            .expect("create scratch test database");

        let base = pg_admin
            .rsplit_once('/')
            .map_or(pg_admin.as_str(), |(b, _)| b);
        let db_url = format!("{base}/{db_name}");
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(&db_url)
            .await
            .expect("connect to scratch test database");

        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("run oss/migrations against scratch database");

        let owner_user_id = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, username, email) VALUES ($1, $2, $3)")
            .bind(owner_user_id)
            .bind(format!("{prefix}-{}", owner_user_id.simple()))
            .bind(format!("{prefix}-{}@example.com", owner_user_id.simple()))
            .execute(&pool)
            .await
            .expect("seed user");

        let agent_id = Uuid::new_v4();
        sqlx::query("INSERT INTO agents (id, name, owner_id) VALUES ($1, $2, $3)")
            .bind(agent_id)
            .bind(format!("{prefix}-agent-{}", agent_id.simple()))
            .bind(owner_user_id)
            .execute(&pool)
            .await
            .expect("seed agent");

        Self {
            pool,
            agent_id,
            owner_user_id,
            db_name,
        }
    }

    /// Seeds a minimal, real `mcp_connectors` row and returns its id — needed by any test that
    /// inserts into `mcp_session_tool_grants`, whose `connector_id` column gained a real FK to
    /// this table (`0026_mcp_session_tool_grants_fk.sql`); a synthetic `Uuid::new_v4()` connector
    /// id (the previous pattern in these tests) now violates that constraint.
    ///
    /// `url` is not optional padding: `source_kind` defaults to `external_url`, and
    /// `chk_connectors_provider_fields` (`0003_mcp.sql`) requires `url IS NOT NULL` for that
    /// combination, so a `(provider_type, name)`-only insert fails the CHECK. Never dialed — this
    /// row exists only to satisfy the FK above.
    #[allow(dead_code)] // only repo.rs's session-grant tests construct this
    pub async fn seed_connector(&self, prefix: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO mcp_connectors (provider_type, name, url) \
             VALUES ('mcp_server', $1, 'http://127.0.0.1:1/mcp') \
             RETURNING id",
        )
        .bind(format!("{prefix}-connector-{}", Uuid::new_v4().simple()))
        .fetch_one(&self.pool)
        .await
        .expect("seed connector")
    }
}

/// Drop the scratch database when the fixture goes out of scope.
///
/// Without this every test leaked its database — a local Postgres had accumulated 1062 of them,
/// and `CREATE DATABASE` degrades as `pg_database` grows, which is what turned a 5s test binary
/// into a 27s one and started timing out poll loops. `oss/server/tests/common` has always dropped
/// its own via an explicit `cleanup()`; these fixtures had no equivalent.
///
/// Done on a detached thread with its own runtime because `Drop` cannot await and the test's
/// runtime may already be shutting down, and `join()`ed so the drop actually completes before the
/// process exits. `WITH (FORCE)` terminates the pool's remaining backends — otherwise the open
/// connections this fixture still holds would block the drop.
impl Drop for TestDb {
    fn drop(&mut self) {
        let url = pg_admin_url();
        let name = std::mem::take(&mut self.db_name);
        if name.is_empty() {
            return;
        }
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                if let Ok(admin) = PgPoolOptions::new().max_connections(1).connect(&url).await {
                    let _ =
                        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&admin)
                            .await;
                }
            });
        })
        .join();
    }
}
