//! Postgres-backed session ↔ trace correlation resolver.
//!
//! `agent_proxy` inserts one `session_traces` row per forwarded user query;
//! this resolver serves both directions for the observability provider, which
//! needs them for agents that never set `session.id` on their spans (anything
//! not running the Python auto-instrumentation patch).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nasiko_observability::provider::SessionIdResolver;
use sqlx::PgPool;

pub struct PgSessionIdResolver {
    db: PgPool,
}

impl PgSessionIdResolver {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }
}

#[async_trait]
impl SessionIdResolver for PgSessionIdResolver {
    async fn session_for_trace(&self, trace_id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT session_id FROM session_traces WHERE trace_id = $1")
            .bind(trace_id)
            .fetch_optional(&self.db)
            .await
            .ok()
            .flatten()
    }

    async fn traces_for_session(&self, session_id: &str) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT trace_id FROM session_traces WHERE session_id = $1 ORDER BY created_at",
        )
        .bind(session_id)
        .fetch_all(&self.db)
        .await
        .unwrap_or_default()
    }

    async fn traces_for_agent(
        &self,
        agent_name: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Vec<String> {
        // `agent_name` is the denormalised column agent_proxy stamps at forward
        // time, so this survives agent deletion. Bounded like the Tempo search
        // it supplements (1000-row cap).
        sqlx::query_scalar(
            "SELECT trace_id FROM session_traces \
             WHERE agent_name = $1 AND created_at >= $2 AND created_at <= $3 \
             ORDER BY created_at \
             LIMIT 1000",
        )
        .bind(agent_name)
        .bind(start)
        .bind(end)
        .fetch_all(&self.db)
        .await
        .unwrap_or_default()
    }
}
