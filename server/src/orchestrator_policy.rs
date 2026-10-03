//! Where an operator's orchestrator policy is resolved from.
//!
//! Two orchestrators can be told what they are allowed to do, and they take
//! their instructions in different shapes: the chat-path ReAct loop wants a
//! `nasiko_react_agent::DelegationPolicy`, the MAF-path routing engine wants a
//! `nasiko_orchestrator::RoutingPolicy`. One source answers for both, so the two
//! can never end up governed by different settings — which is the failure mode
//! of letting each call site fetch its own.
//!
//! The open-source source is [`NoOrchestratorPolicy`]: it imposes nothing, and
//! the orchestrators behave as they do with no policy at all. A distribution
//! that has a policy to apply replaces this on [`crate::state::AppState`] at its
//! composition root, the same way it replaces `routing_engine` and `auth`.

use std::sync::Arc;

use async_trait::async_trait;
use nasiko_orchestrator::RoutingPolicy;
use nasiko_react_agent::DelegationPolicy;
use sqlx::PgPool;

/// What kind of turn the orchestrator is about to run, for a policy that treats
/// them differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnKind {
    /// Started by something the user just said. The ordinary case.
    User,
    /// Reports on work an earlier turn already did — the human-in-the-loop
    /// resume (`crate::hitl`). Its own prompt explicitly tells the model not to
    /// call any agent again, because the call it is reporting on already
    /// succeeded, so a policy that demands an agent call per turn must not
    /// apply to it: enforcing one here would replace every resumed answer with
    /// a refusal and break HITL end to end.
    Continuation,
}

/// Resolves the policy in force for one request.
///
/// Read per turn rather than cached, so a settings change takes effect without a
/// restart. Neither method may fail: an orchestrator that refused to run because
/// a policy lookup errored would be down over a settings read, so an
/// implementation that cannot load its policy decides for itself what to fall
/// back to and says so in its logs.
#[async_trait]
pub trait OrchestratorPolicySource: Send + Sync {
    /// Policy for the chat-path ReAct loop, or `None` to impose nothing.
    async fn chat_policy(&self, db: &PgPool, kind: TurnKind) -> Option<Arc<dyn DelegationPolicy>>;

    /// Policy for the MAF-path routing engine, or `None` to impose nothing.
    async fn routing_policy(&self, db: &PgPool) -> Option<Arc<dyn RoutingPolicy>>;
}

/// The open-source source: no policy, on either path.
pub struct NoOrchestratorPolicy;

#[async_trait]
impl OrchestratorPolicySource for NoOrchestratorPolicy {
    async fn chat_policy(
        &self,
        _db: &PgPool,
        _kind: TurnKind,
    ) -> Option<Arc<dyn DelegationPolicy>> {
        None
    }

    async fn routing_policy(&self, _db: &PgPool) -> Option<Arc<dyn RoutingPolicy>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool that is never connected to anything. Safe because the assertions
    /// below are precisely that `NoOrchestratorPolicy` does not touch it: if it
    /// ever grew a query, this test would hang or error rather than pass.
    fn unusable_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://127.0.0.1:1/does-not-exist")
            .expect("a lazy pool is constructed without connecting")
    }

    /// The open-source deployment imposes nothing, on either orchestrator, for
    /// every kind of turn — and reaches no database to decide it.
    ///
    /// This is what makes an operator-set policy an enterprise-only thing rather
    /// than a shared one: point an open-source binary at a database an
    /// enterprise deployment configured, rows and all, and it still asks for
    /// nothing and enforces nothing, because it never looks.
    #[tokio::test]
    async fn the_open_source_source_imposes_nothing_and_reads_nothing() {
        let db = unusable_pool();
        let source = NoOrchestratorPolicy;

        for kind in [TurnKind::User, TurnKind::Continuation] {
            assert!(
                source.chat_policy(&db, kind).await.is_none(),
                "the chat path must be unpoliced for {kind:?}"
            );
        }
        assert!(source.routing_policy(&db).await.is_none());
    }
}
