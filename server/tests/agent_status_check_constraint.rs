//! Regression guard for the `agents.status` CHECK constraint in
//! `0001_schema.sql`, which must accept every status the platform actually
//! writes.
//!
//! The bug this locks out: the CHECK listed only
//! `registered/deploying/running/stopped/failed`, but the crash-loop guardian
//! (Kubernetes runtime) writes `'crashed'` when a deployment enters
//! CrashLoopBackOff. That `UPDATE` failed with SQLSTATE 23514, and because the
//! guardian only logs the error, the row stayed `'running'` forever — so a
//! crashed agent kept reporting healthy through `GET /api/agents` and the UI.
//! A silent constraint violation is the worst shape for this: nothing surfaces,
//! and the state is wrong in the reassuring direction.
//!
//! Asserted for the whole set rather than just `'crashed'`, so adding a new
//! lifecycle state without widening the CHECK fails here instead of in
//! production.
//!
//! Like `build_status_enum_regression.rs`, this provisions an isolated database
//! and runs migrations, so it never depends on seed data.
//!
//! Run with infra up (`just infra`):
//!   `cargo test -p nasiko-server --test agent_status_check_constraint`
//! Override the admin DSN with TEST_PG_ADMIN_URL if Postgres isn't on :5432.

use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

/// Every value the platform writes to `agents.status`, across both editions.
/// Kept in sync by this test failing when the schema and the writers disagree.
const WRITTEN_STATUSES: [&str; 6] = [
    "registered", // column default
    "deploying",
    "running",
    "stopped",
    "failed",
    "crashed", // crash-loop guardian, Kubernetes runtime
];

fn admin_url() -> String {
    std::env::var("TEST_PG_ADMIN_URL")
        .unwrap_or_else(|_| "postgres://nasiko:nasiko@localhost:5432/nasiko_dev".to_string())
}

#[tokio::test]
async fn agents_status_check_accepts_every_status_the_platform_writes() {
    let admin_dsn = admin_url();

    let admin = match PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_dsn)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SKIP: cannot reach Postgres at {admin_dsn}: {e}");
            return;
        }
    };
    let db_name = format!("nasiko_test_agentstatus_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
        .execute(&admin)
        .await
        .expect("create test database");

    let db_url = {
        let base = admin_dsn
            .rsplit_once('/')
            .map(|(b, _)| b)
            .unwrap_or(&admin_dsn);
        format!("{base}/{db_name}")
    };

    let result = run_checks(&db_url).await;

    // Always drop the test DB, even on failure.
    let _ = sqlx::query(&format!(
        "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
    ))
    .execute(&admin)
    .await;

    result.expect("agents.status CHECK constraint accepts all written statuses");
}

async fn run_checks(db_url: &str) -> Result<(), String> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(db_url)
        .await
        .map_err(|e| format!("connect test db: {e}"))?;

    sqlx::migrate!("../migrations")
        .run(&pool)
        .await
        .map_err(|e| format!("run migrations: {e}"))?;

    let owner: Uuid =
        sqlx::query_scalar("INSERT INTO users (username, email) VALUES ($1, $2) RETURNING id")
            .bind(format!("statuscheck-{}", Uuid::new_v4().simple()))
            .bind(format!("sc-{}@test.local", Uuid::new_v4().simple()))
            .fetch_one(&pool)
            .await
            .map_err(|e| format!("insert user: {e}"))?;

    let agent_id: Uuid =
        sqlx::query_scalar("INSERT INTO agents (name, owner_id) VALUES ($1, $2) RETURNING id")
            .bind(format!("statuscheck-{}", Uuid::new_v4().simple()))
            .bind(owner)
            .fetch_one(&pool)
            .await
            .map_err(|e| format!("insert agent: {e}"))?;

    for status in WRITTEN_STATUSES {
        sqlx::query("UPDATE agents SET status = $2, updated_at = now() WHERE id = $1")
            .bind(agent_id)
            .bind(status)
            .execute(&pool)
            .await
            .map_err(|e| {
                format!(
                    "agents.status rejected '{status}', which the platform writes: {e} \
                     — widen the CHECK in 0001_schema.sql"
                )
            })?;

        let stored: String = sqlx::query_scalar("SELECT status FROM agents WHERE id = $1")
            .bind(agent_id)
            .fetch_one(&pool)
            .await
            .map_err(|e| format!("read back '{status}': {e}"))?;
        if stored != status {
            return Err(format!("wrote '{status}' but read back '{stored}'"));
        }
    }

    // The constraint must still reject genuine typos — a CHECK widened to
    // everything would pass the loop above while protecting nothing.
    let bogus = sqlx::query("UPDATE agents SET status = 'not_a_real_status' WHERE id = $1")
        .bind(agent_id)
        .execute(&pool)
        .await;
    if bogus.is_ok() {
        return Err(
            "agents.status accepted 'not_a_real_status'; the CHECK is not enforcing".into(),
        );
    }

    Ok(())
}
