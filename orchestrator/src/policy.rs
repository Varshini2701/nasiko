//! The routing-policy seam.
//!
//! Stage 3 of the routing engine picks an agent. Whether an operator lets it
//! pick *anything* — how sure the model has to be, what house rules the choice
//! has to respect — is not this crate's decision, and in an unconfigured
//! deployment no such decision has been made at all.
//!
//! Hooks live here; behaviour does not. Every sentence an operator would read,
//! every threshold, and every extra field the model is asked to produce belongs
//! to an implementation of [`RoutingPolicy`], so this crate never carries a
//! rule it cannot itself enforce. The chat path's equivalent seam is
//! `nasiko_react_agent::DelegationPolicy`.

use std::fmt::Debug;

/// A JSON-schema fragment a policy adds to the selector's structured output.
///
/// Merged into the schema rather than replacing it: `agent_id`, `agent_name`
/// and `reasoning` are what the engine itself needs back, whatever the policy
/// is, and a policy that could rewrite the whole schema could break selection.
#[derive(Debug, Clone, Default)]
pub struct SelectionSchemaExtra {
    /// Property name → its JSON Schema, merged into the selection schema.
    pub properties: serde_json::Map<String, serde_json::Value>,
    /// Property names appended to the schema's `required` list.
    pub required: Vec<String>,
}

/// Operator policy applied to one routing decision.
///
/// Every method has a do-nothing default, so an implementation states only what
/// it actually constrains. No policy at all and a policy that overrides nothing
/// behave identically — the former is just cheaper.
pub trait RoutingPolicy: Debug + Send + Sync {
    /// Text inserted immediately after the selector's opening line, before the
    /// agent list. Empty adds nothing.
    fn prompt_prefix(&self) -> String {
        String::new()
    }

    /// Replaces the selector's own closing instruction. `None` keeps it.
    ///
    /// A threshold and the built-in "if no perfect match, choose the closest
    /// option" are contradictory advice — the second is precisely what makes a
    /// selector always return *something* — so a policy that can refuse is
    /// expected to replace that sentence rather than argue with it.
    fn closing_instruction(&self) -> Option<String> {
        None
    }

    /// Extra fields the model must return alongside its pick, and which of them
    /// are required. `None` leaves the selection schema untouched.
    ///
    /// Requested only because something reads them: this schema is `strict`, so
    /// a field listed here is a field every provider must fill, and asking for a
    /// number nothing checks trains the model to produce one carelessly.
    fn selection_schema_extra(&self) -> Option<SelectionSchemaExtra> {
        None
    }

    /// Judge one selection from the model's raw JSON, before the engine acts on
    /// it. `Err(reason)` refuses the routing decision outright.
    ///
    /// The reason reaches the operator verbatim — in the router log and in the
    /// 400 a workflow step fails with — so it should say what fell short and
    /// what to do about it, in the policy's own words.
    ///
    /// Raw JSON rather than a typed struct: the fields a policy judges on are
    /// the ones it asked for in [`Self::selection_schema_extra`], which this
    /// crate has no reason to know the shape of.
    fn check_selection(&self, _selection: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }
}
