//! `build_worker::claim_next_job`'s same-target serialization clause.
//!
//! The build worker runs up to `BUILD_CONCURRENCY` jobs at once. Two builds for
//! the *same* agent resolve to the same `image_tag` (`agents::build_image_tag`
//! is name + version) and deploy the same `container_id`, so running them
//! concurrently makes the surviving image nondeterministic. The serial worker
//! prevented that implicitly; the claim query now has to do it explicitly.
//!
//! Like `build_status_enum_regression.rs`, this provisions its own isolated
//! database and runs migrations **without booting `AppState`** — that matters
//! here, because `AppState::from_config_with_db` spawns a live build worker that
//! would race these fixture rows.
//!
//! Run with infra up (`just infra`):
//!   `just test-one build_worker_claim`
//! Override the admin DSN with TEST_PG_ADMIN_URL if Postgres isn't on :5432.

use nasiko_server::agents::build_worker::claim_next_job;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

fn admin_url() -> String {
    std::env::var("TEST_PG_ADMIN_URL")
        .unwrap_or_else(|_| "postgres://nasiko:nasiko@localhost:5432/nasiko_dev".to_string())
}

/// Provision an isolated, migrated database, hand its URL to `body`, and drop
/// it afterwards even when `body` fails. Returns `None` when Postgres is
/// unreachable, so the caller can skip rather than fail.
async fn with_scratch_db<F, Fut>(label: &str, body: F) -> Option<Result<(), String>>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let admin_dsn = admin_url();
    let admin = match PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_dsn)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("SKIP: cannot reach Postgres at {admin_dsn}: {e}");
            return None;
        }
    };
    let db_name = format!("nasiko_test_{label}_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
        .execute(&admin)
        .await
        .expect("create test database");

    let base = admin_dsn
        .rsplit_once('/')
        .map(|(b, _)| b)
        .unwrap_or(&admin_dsn);
    let result = body(format!("{base}/{db_name}")).await;

    // Always drop the test DB, even on failure.
    let _ = sqlx::query(&format!(
        "DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)"
    ))
    .execute(&admin)
    .await;

    Some(result)
}

#[tokio::test]
async fn claim_serializes_per_target_but_not_across_targets() {
    let Some(result) =
        with_scratch_db("claim", |db_url| async move { run_checks(&db_url).await }).await
    else {
        return;
    };
    result.expect("claim_next_job same-target checks");
}

/// The sequential test above cannot reach the case the advisory lock exists
/// for: `NOT EXISTS` reads committed state, so a second replica claiming while
/// the first has not yet committed passes it and takes a sibling of the same
/// target — two builds resolving to one `image_tag`.
///
/// Firing concurrent `claim_next_job` calls does *not* reproduce that: they
/// each open their own short transaction and in practice the first commits
/// before the next one selects, so such a test passes with or without the lock
/// and proves nothing. Instead this holds the target's lock the way an
/// uncommitted peer claim would, and asserts the query declines to claim.
///
/// Two-sided on purpose: the same target must become claimable once the lock
/// is released. Without that second half, a test whose lock key drifted from
/// the query's would pass for the wrong reason — it would be observing a
/// target nothing can ever claim.
#[tokio::test]
async fn a_target_locked_by_another_claimer_is_not_claimed() {
    let Some(result) = with_scratch_db("lock", |db_url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&db_url)
            .await
            .map_err(|e| format!("connect test db: {e}"))?;
        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .map_err(|e| format!("run migrations: {e}"))?;

        let owner = insert_user(&pool).await?;
        let agent = insert_agent(&pool, owner).await?;
        let job = queue_agent_job(&pool, agent, owner).await?;

        // Stand in for a peer replica mid-claim: hold the target's lock in an
        // open transaction. The key must match what the query computes, hence
        // the same `'a:' || <uuid>` shape.
        let mut peer = pool
            .begin()
            .await
            .map_err(|e| format!("begin peer tx: {e}"))?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('a:' || $1::text))")
            .bind(agent)
            .execute(&mut *peer)
            .await
            .map_err(|e| format!("take peer lock: {e}"))?;

        if let Some(claimed) = claim(&pool).await? {
            return Err(format!(
                "claimed {claimed} while a peer held the target's lock — the \
                 cross-replica window is open"
            ));
        }
        if status_of(&pool, job).await? != "pending" {
            return Err("a job skipped for a held lock must stay pending".into());
        }

        // Peer finishes without claiming; the lock goes with its transaction.
        peer.rollback()
            .await
            .map_err(|e| format!("rollback peer tx: {e}"))?;

        let after = claim(&pool)
            .await?
            .ok_or("the job must be claimable once the peer's lock is released")?;
        if after != job {
            return Err(format!("expected {job}, got {after}"));
        }
        Ok(())
    })
    .await
    else {
        return;
    };
    result.expect("advisory-lock serialization");
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

    let owner = insert_user(&pool).await?;
    let agent_a = insert_agent(&pool, owner).await?;
    let agent_b = insert_agent(&pool, owner).await?;
    let connector = insert_connector(&pool, owner).await?;

    // ── Different targets claim concurrently ──────────────────────────────
    let job_a = queue_agent_job(&pool, agent_a, owner).await?;
    let job_b = queue_agent_job(&pool, agent_b, owner).await?;

    let first = claim(&pool)
        .await?
        .ok_or("expected to claim agent A's job")?;
    if first != job_a {
        return Err("claims must follow created_at order".into());
    }
    let second = claim(&pool)
        .await?
        .ok_or("a job for a *different* agent must be claimable while A builds")?;
    if second != job_b {
        return Err(format!("expected job_b, got {second}"));
    }

    // ── A second job for an in-flight agent is skipped, not failed ────────
    let job_a2 = queue_agent_job(&pool, agent_a, owner).await?;
    if let Some(id) = claim(&pool).await? {
        return Err(format!(
            "claimed {id} while its target already had a build in flight"
        ));
    }
    // Still pending — skipped, not consumed and not failed.
    if status_of(&pool, job_a2).await? != "pending" {
        return Err("a skipped job must stay pending".into());
    }

    // ...and becomes claimable once the sibling finishes.
    finish(&pool, job_a).await?;
    let after = claim(&pool)
        .await?
        .ok_or("job must become claimable once its sibling completes")?;
    if after != job_a2 {
        return Err(format!("expected job_a2, got {after}"));
    }

    // ── Same rule for MCP connectors ──────────────────────────────────────
    let conn_1 = queue_connector_job(&pool, connector, owner).await?;
    let conn_2 = queue_connector_job(&pool, connector, owner).await?;
    let claimed = claim(&pool)
        .await?
        .ok_or("expected to claim connector job")?;
    if claimed != conn_1 {
        return Err(format!("expected conn_1, got {claimed}"));
    }
    if let Some(id) = claim(&pool).await? {
        return Err(format!("claimed {id} for an already-building connector"));
    }

    // ── NULL-safety: an agent job must not be blocked by a connector job ──
    // `b.agent_id = j.agent_id` is NULL on both sides here, which is NULL (not
    // true), so the NOT EXISTS must not match. A `NOT IN` formulation would get
    // this wrong.
    let agent_c = insert_agent(&pool, owner).await?;
    let job_c = queue_agent_job(&pool, agent_c, owner).await?;
    let claimed = claim(&pool)
        .await?
        .ok_or("an agent job must not be blocked by an in-flight connector job")?;
    if claimed != job_c {
        return Err(format!("expected job_c, got {claimed}"));
    }
    // And the reverse direction, with only agent jobs in flight.
    if claim(&pool).await?.is_some() {
        return Err("no other job should have been claimable".into());
    }
    finish(&pool, conn_1).await?;
    let claimed = claim(&pool)
        .await?
        .ok_or("connector job must be claimable once its sibling completes")?;
    if claimed != conn_2 {
        return Err(format!("expected conn_2, got {claimed}"));
    }

    Ok(())
}

// ─── helpers ────────────────────────────────────────────────────────────────

async fn claim(pool: &PgPool) -> Result<Option<Uuid>, String> {
    claim_next_job(pool)
        .await
        .map(|job| job.map(|j| j.id))
        .map_err(|e| format!("claim_next_job: {e}"))
}

async fn status_of(pool: &PgPool, job_id: Uuid) -> Result<String, String> {
    sqlx::query_scalar("SELECT status FROM build_jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("read job status: {e}"))
}

/// Drive a claimed job to a terminal state, freeing its target.
async fn finish(pool: &PgPool, job_id: Uuid) -> Result<(), String> {
    sqlx::query("UPDATE build_jobs SET status = 'done', completed_at = now() WHERE id = $1")
        .bind(job_id)
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(|e| format!("finish job: {e}"))
}

async fn insert_user(pool: &PgPool) -> Result<Uuid, String> {
    sqlx::query_scalar("INSERT INTO users (username, email) VALUES ($1, $2) RETURNING id")
        .bind(format!("claim-{}", Uuid::new_v4().simple()))
        .bind(format!("claim-{}@test.local", Uuid::new_v4().simple()))
        .fetch_one(pool)
        .await
        .map_err(|e| format!("insert user: {e}"))
}

async fn insert_agent(pool: &PgPool, owner: Uuid) -> Result<Uuid, String> {
    sqlx::query_scalar("INSERT INTO agents (name, owner_id) VALUES ($1, $2) RETURNING id")
        .bind(format!("claim-agent-{}", Uuid::new_v4().simple()))
        .bind(owner)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("insert agent: {e}"))
}

async fn insert_connector(pool: &PgPool, owner: Uuid) -> Result<Uuid, String> {
    sqlx::query_scalar(
        "INSERT INTO mcp_connectors (provider_type, owner_id, name, source_kind, build_status, is_active) \
         VALUES ('mcp_server', $1, $2, 'uploaded_build', 'building', false) RETURNING id",
    )
    .bind(owner)
    .bind(format!("claim-connector-{}", Uuid::new_v4().simple()))
    .fetch_one(pool)
    .await
    .map_err(|e| format!("insert connector: {e}"))
}

/// The claim query never deserializes the payload, so an opaque object is
/// enough — this test is about *which* row is claimed, not what it contains.
async fn queue_agent_job(pool: &PgPool, agent_id: Uuid, owner: Uuid) -> Result<Uuid, String> {
    sqlx::query_scalar(
        "INSERT INTO build_jobs (agent_id, owner_id, payload) VALUES ($1, $2, '{}'::jsonb) RETURNING id",
    )
    .bind(agent_id)
    .bind(owner)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("queue agent job: {e}"))
}

async fn queue_connector_job(
    pool: &PgPool,
    connector_id: Uuid,
    owner: Uuid,
) -> Result<Uuid, String> {
    sqlx::query_scalar(
        "INSERT INTO build_jobs (connector_id, owner_id, payload) VALUES ($1, $2, '{}'::jsonb) RETURNING id",
    )
    .bind(connector_id)
    .bind(owner)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("queue connector job: {e}"))
}
