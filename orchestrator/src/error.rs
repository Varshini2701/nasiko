use thiserror::Error;

#[derive(Debug, Error)]
pub enum RouterError {
    #[error("no agents available")]
    NoAgentsAvailable,
    /// Agents exist and were considered, but the caller's `RoutingPolicy`
    /// refused the pick. Distinct from `NoAgentsAvailable` (an empty fleet)
    /// because the remedy differs: deploy an agent vs. act on whatever the
    /// policy said. Callers must NOT paper over this with a fallback pick —
    /// doing so reinstates exactly the "route to something, anything" behaviour
    /// a policy exists to prevent.
    ///
    /// `reason` is the policy's own wording, relayed verbatim to the operator.
    #[error("the routing policy refused every candidate: {reason}")]
    PolicyRefused { reason: String },
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("embedding error: {0}")]
    Embedding(String),
    #[error("selection failed: {0}")]
    Selection(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<crate::selector::SelectorError> for RouterError {
    fn from(e: crate::selector::SelectorError) -> Self {
        RouterError::Selection(e.to_string())
    }
}
