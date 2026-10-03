//! Generic hook fired when an agent is deleted, for enterprise-only cleanup of agent-keyed state
//! that OSS has no knowledge of. Deliberately generic-named — no enterprise-specific naming —
//! same pattern as `prompt_context`: OSS defines the shape and ships a no-op default, EE
//! composition roots override it.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use uuid::Uuid;

#[async_trait]
pub trait AgentDeletionHook: Send + Sync {
    /// Called once, best-effort, right after `agents.deleted_at` has been set for `agent_id` —
    /// a chance for enterprise-only, agent-keyed state that OSS's schema has no FK for (so a soft
    /// delete can't cascade to it) to clean itself up. Must never fail or block the delete
    /// request: implementations should log and swallow their own errors, the same way the MCP
    /// gateway token revoke right above this call site does.
    async fn on_agent_deleted(&self, agent_id: Uuid);
}

/// OSS default — nothing enterprise-only is wired up, so there is nothing to clean up.
pub struct NoopAgentDeletionHook;

#[async_trait]
impl AgentDeletionHook for NoopAgentDeletionHook {
    async fn on_agent_deleted(&self, _agent_id: Uuid) {}
}

/// Holds an `AgentDeletionHook` behind a lock so the implementation can be swapped after
/// `AppState` has already been cloned — needed for exactly the reason
/// `prompt_context::SwappablePromptContext` exists (see its doc comment): the EE composition
/// root's real implementation is installed by `build_ee_app`, but the build worker is spawned
/// with an `AppState` clone taken earlier, inside `AppState::from_config_with_db`, strictly
/// before `build_ee_app` ever runs. Reassigning a plain `Arc<dyn AgentDeletionHook>` field only
/// rebinds *that* later clone's own pointer — the worker's earlier clone would keep calling the
/// OSS no-op for its entire lifetime, silently defeating every hard-delete cleanup path that
/// runs inside it (`oss/server/src/agents/{build_worker,upload,utils}.rs`). Installing *through*
/// this shared cell instead makes the swap visible to every existing and future clone.
pub struct SwappableAgentDeletionHook(RwLock<Arc<dyn AgentDeletionHook>>);

impl SwappableAgentDeletionHook {
    pub fn new(initial: Arc<dyn AgentDeletionHook>) -> Self {
        Self(RwLock::new(initial))
    }

    /// Replaces the underlying implementation for every clone, past and future, of the
    /// `AppState` this cell lives in.
    pub fn install(&self, new: Arc<dyn AgentDeletionHook>) {
        *self.0.write().expect("agent deletion hook lock poisoned") = new;
    }

    pub async fn on_agent_deleted(&self, agent_id: Uuid) {
        let current = self
            .0
            .read()
            .expect("agent deletion hook lock poisoned")
            .clone();
        current.on_agent_deleted(agent_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingHook(std::sync::Mutex<Vec<Uuid>>);

    #[async_trait]
    impl AgentDeletionHook for RecordingHook {
        async fn on_agent_deleted(&self, agent_id: Uuid) {
            self.0
                .lock()
                .expect("recording hook lock poisoned")
                .push(agent_id);
        }
    }

    /// Reproduces the exact bug this cell exists to prevent: a background task clones `AppState`
    /// (here, just the `Arc<SwappableAgentDeletionHook>` field) BEFORE the EE composition root
    /// installs the real implementation. The clone must still see the swap, because
    /// `AppState::clone()` only clones the `Arc` pointer to this one shared cell.
    #[tokio::test]
    async fn install_is_visible_to_a_clone_taken_before_the_install_call() {
        let cell = Arc::new(SwappableAgentDeletionHook::new(Arc::new(
            NoopAgentDeletionHook,
        )));

        // Simulates `worker_state = state.clone()`, captured before `build_ee_app` runs.
        let pre_install_clone = cell.clone();

        let recorder = Arc::new(RecordingHook(std::sync::Mutex::new(Vec::new())));
        // Simulates `build_ee_app`'s `state.agent_deletion_hook.install(...)`.
        cell.install(recorder.clone());

        let agent_id = Uuid::new_v4();
        // The EARLIER clone — not `cell` itself — must now see the installed implementation.
        pre_install_clone.on_agent_deleted(agent_id).await;

        assert_eq!(
            recorder
                .0
                .lock()
                .expect("recording hook lock poisoned")
                .as_slice(),
            &[agent_id],
            "a clone taken before install() must still observe the swap"
        );
    }
}
