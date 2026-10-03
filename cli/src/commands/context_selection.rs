//! `nasiko budget` and `nasiko context-strategy` — the two per-user settings
//! that control how much session history a request carries, and how that
//! slice is chosen.
//!
//! Both are the same shape (`GET` prints, `PATCH` sets one enum-valued field),
//! so they share `show`/`update` below and differ only in route, field name
//! and the accepted values. Values are validated here, before any network
//! call, so a typo costs nothing and reports the valid set.

use anyhow::{Result, bail};

const BUDGET: Setting = Setting {
    path: "/me/pacms-budget",
    field: "level",
    label: "PACMS budget",
    allowed: &["low", "medium", "high"],
};

const STRATEGY: Setting = Setting {
    path: "/me/context-strategy",
    field: "strategy",
    label: "Context strategy",
    allowed: &["pacms", "topk", "lastk"],
};

struct Setting {
    /// API route, relative to the cluster base.
    path: &'static str,
    /// JSON field the route reads and echoes back.
    field: &'static str,
    /// Human-facing name used in both output lines.
    label: &'static str,
    /// Accepted values, checked before the request goes out.
    allowed: &'static [&'static str],
}

impl Setting {
    fn show(&self) -> Result<()> {
        let client = crate::api::Client::from_active_cluster()?;
        let resp: serde_json::Value = client.get_json(self.path)?;
        let value = resp
            .get(self.field)
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        println!("{}: {value}", self.label);
        Ok(())
    }

    fn update(&self, value: &str) -> Result<()> {
        let value = value.to_lowercase();
        if !self.allowed.contains(&value.as_str()) {
            bail!(
                "invalid value '{value}' for {} — must be one of: {}",
                self.label,
                self.allowed.join(", ")
            );
        }

        let client = crate::api::Client::from_active_cluster()?;
        let _: serde_json::Value =
            client.patch_json(self.path, &serde_json::json!({ self.field: value }))?;
        println!("{} set to: {value}", self.label);
        Ok(())
    }
}

/// Show the caller's persisted PACMS conversation-history budget tier.
pub fn budget_get() -> Result<()> {
    BUDGET.show()
}

/// Set the caller's PACMS conversation-history budget tier (low/medium/high).
pub fn budget_set(level: &str) -> Result<()> {
    BUDGET.update(level)
}

/// Show the caller's persisted conversation-history context-selection strategy.
pub fn strategy_get() -> Result<()> {
    STRATEGY.show()
}

/// Set the caller's conversation-history context-selection strategy (pacms/topk/lastk).
pub fn strategy_set(strategy: &str) -> Result<()> {
    STRATEGY.update(strategy)
}
