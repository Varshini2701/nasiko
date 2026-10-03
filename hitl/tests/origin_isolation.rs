//! Regression test for the two resume dispatchers' origin scoping (`repo::claim_for_resume` vs.
//! `PgHitlStore::claim_for_resume`, `oss/server/src/state.rs`). Before this scoping existed, both
//! claim queries matched *any* resolved row regardless of `origin` — running both dispatchers
//! unmodified would race on the same rows, and whichever claimed a row it can't actually deliver
//! (an `mcp_tool` row has no `task_id` for the direct-chat dispatcher's A2A task resume; a
//! `direct_chat` row is never even looked at by MCP's plain-nudge dispatcher) would just fail it.
//! Needs infra up (`just infra`; override with `TEST_PG_URL`), same convention as `tests/repo.rs`.

use nasiko_hitl::repo::{self, NewAuthRequired, ResolveDecision};
use nasiko_hitl::{HitlStatus, HitlStore, NewHitlRequest, PgHitlStore};
use uuid::Uuid;

mod common;
use common::TestDb;

impl TestDb {
    /// A resolved `origin = mcp_tool` row, seeded and resolved via `nasiko_hitl::repo` — the
    /// plain-function API MCP's own dispatcher/gateway code calls.
    async fn seed_resolved_mcp_tool_row(&self) -> Uuid {
        let created = repo::create_pending_auth_required(
            &self.pool,
            NewAuthRequired {
                agent_id: self.agent_id,
                owner_user_id: self.owner_user_id,
                connector_id: Uuid::new_v4(),
                context_id: "ctx-mcp".to_string(),
                question: serde_json::json!({"connector": "github"}),
            },
        )
        .await
        .expect("create pending auth_required");

        repo::resolve(
            &self.pool,
            created.id,
            ResolveDecision::Approve,
            self.owner_user_id,
            serde_json::json!({"decision": "approve"}),
        )
        .await
        .expect("resolve")
        .expect("row was pending");

        created.id
    }

    /// A resolved `origin = direct_chat` row, seeded and resolved via `HitlStore` — the
    /// trait-based API the direct-chat dispatcher's own code calls.
    async fn seed_resolved_direct_chat_row(&self) -> Uuid {
        let store = PgHitlStore::new(self.pool.clone());
        let created = store
            .create(NewHitlRequest::direct_chat(
                nasiko_hitl::HitlKind::InputRequired,
                self.agent_id,
                self.owner_user_id,
                "task-direct-chat",
                "ctx-direct-chat",
                serde_json::json!({"message": "need input"}),
            ))
            .await
            .expect("create pending direct_chat row");

        store
            .resolve(
                created.id,
                serde_json::json!({"answer": "here you go"}),
                self.owner_user_id,
                HitlStatus::Resolved,
            )
            .await
            .expect("resolve");

        created.id
    }

    /// A resolved `origin = maf` row, seeded and resolved via `HitlStore` — same trait-based API
    /// as `direct_chat`, since both are delivered by the same dispatcher (`oss/server/src/
    /// hitl/mod.rs::run`, which branches to `deliver_maf` on `HitlOrigin::Maf`).
    async fn seed_resolved_maf_row(&self) -> Uuid {
        let maf_execution_id = Uuid::new_v4();
        sqlx::query("INSERT INTO maf_executions (id, user_id) VALUES ($1, $2)")
            .bind(maf_execution_id)
            .bind(self.owner_user_id)
            .execute(&self.pool)
            .await
            .expect("seed maf_executions row");

        let store = PgHitlStore::new(self.pool.clone());
        let created = store
            .create(NewHitlRequest::maf(
                nasiko_hitl::HitlKind::InputRequired,
                self.agent_id,
                self.owner_user_id,
                "task-maf",
                "ctx-maf",
                maf_execution_id,
                0,
                serde_json::json!({"message": "which value should step 0 use?"}),
            ))
            .await
            .expect("create pending maf row");

        store
            .resolve(
                created.id,
                serde_json::json!({"answer": "42"}),
                self.owner_user_id,
                HitlStatus::Resolved,
            )
            .await
            .expect("resolve");

        created.id
    }

    /// A resolved `origin = orchestrator` row, seeded and resolved via `HitlStore` — same
    /// trait-based API as `direct_chat`/`maf`, since a resolved orchestrator row is delivered by
    /// the same dispatcher (`oss/server/src/hitl/mod.rs::run`, which resumes it via a real A2A
    /// task resume, same as `direct_chat`/`agent_proxy`).
    async fn seed_resolved_orchestrator_row(&self) -> Uuid {
        let chat_session_id = format!("chat-session-orchestrator-{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO chat_sessions (session_id, user_id, agent_id, title) VALUES ($1, $2, $3, $4)",
        )
        .bind(&chat_session_id)
        .bind(self.owner_user_id)
        .bind(self.agent_id)
        .bind("origin-isolation-test-session")
        .execute(&self.pool)
        .await
        .expect("seed chat_sessions row");

        let store = PgHitlStore::new(self.pool.clone());
        let created = store
            .create(NewHitlRequest::orchestrator(
                nasiko_hitl::HitlKind::InputRequired,
                self.agent_id,
                self.owner_user_id,
                "task-orchestrator",
                "ctx-orchestrator",
                chat_session_id,
                serde_json::json!({"message": "which sub-agent value should be used?"}),
            ))
            .await
            .expect("create pending orchestrator row");

        store
            .resolve(
                created.id,
                serde_json::json!({"answer": "use the staging value"}),
                self.owner_user_id,
                HitlStatus::Resolved,
            )
            .await
            .expect("resolve");

        created.id
    }
}

#[tokio::test]
async fn mcp_dispatcher_never_claims_a_direct_chat_row() {
    let db = TestDb::new("hitl_origin_isolation_test").await;
    let mcp_row = db.seed_resolved_mcp_tool_row().await;
    let direct_chat_row = db.seed_resolved_direct_chat_row().await;

    let claimed = repo::claim_for_resume(&db.pool)
        .await
        .expect("claim_for_resume")
        .expect("must claim the one mcp_tool row available");
    assert_eq!(
        claimed.id, mcp_row,
        "must claim the mcp_tool row, never the direct_chat one"
    );

    // Nothing else left for this dispatcher to claim — the direct_chat row is invisible to it,
    // not merely deprioritized.
    let second = repo::claim_for_resume(&db.pool)
        .await
        .expect("claim_for_resume");
    assert!(
        second.is_none(),
        "the direct_chat row must never be claimable by MCP's dispatcher: {second:?}"
    );

    let direct_chat_status: String =
        sqlx::query_scalar("SELECT resume_status FROM hitl_requests WHERE id = $1")
            .bind(direct_chat_row)
            .fetch_one(&db.pool)
            .await
            .expect("fetch resume_status");
    assert_eq!(
        direct_chat_status, "not_started",
        "the direct_chat row must be completely untouched by MCP's dispatcher"
    );
}

#[tokio::test]
async fn direct_chat_dispatcher_never_claims_an_mcp_tool_row() {
    let db = TestDb::new("hitl_origin_isolation_test").await;
    let mcp_row = db.seed_resolved_mcp_tool_row().await;
    let direct_chat_row = db.seed_resolved_direct_chat_row().await;

    let store = PgHitlStore::new(db.pool.clone());
    let claimed = store
        .claim_for_resume(120)
        .await
        .expect("claim_for_resume")
        .expect("must claim the one direct_chat row available");
    assert_eq!(
        claimed.id, direct_chat_row,
        "must claim the direct_chat row, never the mcp_tool one"
    );

    let second = store.claim_for_resume(120).await.expect("claim_for_resume");
    assert!(
        second.is_none(),
        "the mcp_tool row must never be claimable by the direct-chat dispatcher: {second:?}"
    );

    let mcp_row_status: String =
        sqlx::query_scalar("SELECT resume_status FROM hitl_requests WHERE id = $1")
            .bind(mcp_row)
            .fetch_one(&db.pool)
            .await
            .expect("fetch resume_status");
    assert_eq!(
        mcp_row_status, "not_started",
        "the mcp_tool row must be completely untouched by the direct-chat dispatcher"
    );
}

/// Regression: the direct-chat dispatcher's claim query was scoped to `origin IN ('direct_chat',
/// 'agent_proxy')` only — correct when that scoping was first added (to avoid racing MCP's own
/// dispatcher), but `'maf'` was never added to the list when MAF's own HITL support was merged in
/// later, even though `deliver()` has always known how to handle `HitlOrigin::Maf` (branches to
/// `deliver_maf`). A resolved `maf` row was therefore claimable by neither dispatcher at all —
/// `resume_status` stuck at `not_started` forever, the workflow never resumes past
/// `awaiting_human` even though the `hitl_requests` row itself shows `resolved`.
#[tokio::test]
async fn direct_chat_dispatcher_claims_a_resolved_maf_row() {
    let db = TestDb::new("hitl_origin_isolation_test").await;
    let mcp_row = db.seed_resolved_mcp_tool_row().await;
    let maf_row = db.seed_resolved_maf_row().await;

    let store = PgHitlStore::new(db.pool.clone());
    let claimed = store
        .claim_for_resume(120)
        .await
        .expect("claim_for_resume")
        .expect("must claim the maf row — this is the regression this test guards against");
    assert_eq!(
        claimed.id, maf_row,
        "must claim the maf row, never the mcp_tool one"
    );
    assert_eq!(claimed.origin, nasiko_hitl::HitlOrigin::Maf);

    // Isolation from MCP's dispatcher still holds — adding `maf` to the list must not also
    // accidentally widen it to `mcp_tool`.
    let second = store.claim_for_resume(120).await.expect("claim_for_resume");
    assert!(
        second.is_none(),
        "the mcp_tool row must still never be claimable by the direct-chat dispatcher: {second:?}"
    );

    let mcp_row_status: String =
        sqlx::query_scalar("SELECT resume_status FROM hitl_requests WHERE id = $1")
            .bind(mcp_row)
            .fetch_one(&db.pool)
            .await
            .expect("fetch resume_status");
    assert_eq!(mcp_row_status, "not_started");
}

/// Same class of regression as `direct_chat_dispatcher_claims_a_resolved_maf_row`, for the
/// `orchestrator` origin merged in later from `feature/orchestrator-hitl`: that branch's own
/// `claim_for_resume` had no origin scoping at all (it never needed to race a second dispatcher),
/// so merging it alongside the origin-scoped dispatcher from `feat/hitl-mcp-impl` left
/// `orchestrator` out of the allowlist — a resolved orchestrator-origin row would have been
/// claimable by neither dispatcher, stuck at `resume_status = 'not_started'` forever.
#[tokio::test]
async fn direct_chat_dispatcher_claims_a_resolved_orchestrator_row() {
    let db = TestDb::new("hitl_origin_isolation_test").await;
    let mcp_row = db.seed_resolved_mcp_tool_row().await;
    let orchestrator_row = db.seed_resolved_orchestrator_row().await;

    let store = PgHitlStore::new(db.pool.clone());
    let claimed = store
        .claim_for_resume(120)
        .await
        .expect("claim_for_resume")
        .expect(
            "must claim the orchestrator row — this is the regression this test guards against",
        );
    assert_eq!(
        claimed.id, orchestrator_row,
        "must claim the orchestrator row, never the mcp_tool one"
    );
    assert_eq!(claimed.origin, nasiko_hitl::HitlOrigin::Orchestrator);

    let second = store.claim_for_resume(120).await.expect("claim_for_resume");
    assert!(
        second.is_none(),
        "the mcp_tool row must still never be claimable by the direct-chat dispatcher: {second:?}"
    );

    let mcp_row_status: String =
        sqlx::query_scalar("SELECT resume_status FROM hitl_requests WHERE id = $1")
            .bind(mcp_row)
            .fetch_one(&db.pool)
            .await
            .expect("fetch resume_status");
    assert_eq!(mcp_row_status, "not_started");
}
