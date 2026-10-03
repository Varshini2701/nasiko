use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use serde::{Deserialize, Serialize};

use crate::auth::Claims;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    // Write route requires admin role — the middleware is applied here so the
    // state is available when the router is merged into the app.
    let write_settings = Router::new().route("/settings", axum::routing::put(update_settings));

    Router::new()
        .route("/settings", get(get_settings))
        .merge(write_settings)
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Settings {
    pub router_model: Option<String>,
    pub default_provider: Option<String>,
    pub max_flow_depth: Option<i32>,
    pub max_flow_fan_out: Option<i32>,
    pub max_flow_tokens: Option<i64>,
    pub flow_timeout_secs: Option<i32>,
    pub registry_url: Option<String>,
    /// Comma-separated tag names pinning the agent-catalog tab list.
    /// Unset/empty → the UI derives tabs from the most common agent tags.
    pub catalog_tabs: Option<String>,
}

impl Settings {
    /// What a deployment that has never saved settings behaves as if it had.
    ///
    /// One definition, used for BOTH "no row at all" and "a row whose column is
    /// still NULL". Those two used to disagree — the no-row case returned these
    /// values and the NULL case returned nulls — which was invisible only
    /// because nothing else created the row. `PUT /api/orchestrator/policy`
    /// (enterprise) now does: it names just its own two columns, so an operator
    /// who opens Settings → Orchestrator first materialises `id = 1` with every
    /// column here left NULL, and this endpoint started reporting blanks where
    /// it had reported defaults.
    fn defaults() -> Self {
        Self {
            router_model: Some("deepseek-v4-pro".into()),
            default_provider: Some("openai".into()),
            max_flow_depth: Some(5),
            max_flow_fan_out: Some(20),
            max_flow_tokens: Some(100000),
            flow_timeout_secs: Some(120),
            registry_url: None,
            catalog_tabs: None,
        }
    }

    /// Fill any column still NULL from [`Self::defaults`].
    ///
    /// Only the columns that HAVE a default are filled: `registry_url` and
    /// `catalog_tabs` default to `None`, so "unset" stays unset and keeps
    /// meaning what it means to their consumers (no registry configured; derive
    /// the catalog tabs from agent tags).
    fn with_defaults(self) -> Self {
        let d = Self::defaults();
        Self {
            router_model: self.router_model.or(d.router_model),
            default_provider: self.default_provider.or(d.default_provider),
            max_flow_depth: self.max_flow_depth.or(d.max_flow_depth),
            max_flow_fan_out: self.max_flow_fan_out.or(d.max_flow_fan_out),
            max_flow_tokens: self.max_flow_tokens.or(d.max_flow_tokens),
            flow_timeout_secs: self.flow_timeout_secs.or(d.flow_timeout_secs),
            registry_url: self.registry_url,
            catalog_tabs: self.catalog_tabs,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SettingsUpdate {
    pub router_model: Option<String>,
    pub default_provider: Option<String>,
    pub max_flow_depth: Option<i32>,
    pub max_flow_fan_out: Option<i32>,
    pub max_flow_tokens: Option<i64>,
    pub flow_timeout_secs: Option<i32>,
    pub registry_url: Option<String>,
    pub catalog_tabs: Option<String>,
}

async fn get_settings(State(state): State<AppState>, _claims: Claims) -> impl IntoResponse {
    let row = sqlx::query_as::<_, Settings>(
        r#"SELECT
            router_model, default_provider, max_flow_depth,
            max_flow_fan_out, max_flow_tokens, flow_timeout_secs,
            registry_url, catalog_tabs
        FROM settings LIMIT 1"#,
    )
    .fetch_optional(&state.db)
    .await;

    match row {
        Ok(Some(s)) => Json(s.with_defaults()).into_response(),
        Ok(None) => Json(Settings::defaults()).into_response(),
        Err(e) => {
            tracing::error!(%e, "get_settings: db error");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

async fn update_settings(
    State(state): State<AppState>,
    claims: Claims,
    Json(body): Json<SettingsUpdate>,
) -> impl IntoResponse {
    let identity: nasiko_auth::Identity = claims.into();
    if !state.auth.can_manage_users(&identity).await {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "requires admin role"})),
        )
            .into_response();
    }
    let result = sqlx::query_as::<_, Settings>(
        r#"INSERT INTO settings (
               id, router_model, default_provider, max_flow_depth, max_flow_fan_out,
               max_flow_tokens, flow_timeout_secs, registry_url, catalog_tabs
           )
           VALUES (1, $1, $2, $3, $4, $5, $6, $7, $8)
           ON CONFLICT (id) DO UPDATE SET
             router_model = EXCLUDED.router_model,
             default_provider = EXCLUDED.default_provider,
             max_flow_depth = EXCLUDED.max_flow_depth,
             max_flow_fan_out = EXCLUDED.max_flow_fan_out,
             max_flow_tokens = EXCLUDED.max_flow_tokens,
             flow_timeout_secs = EXCLUDED.flow_timeout_secs,
             registry_url = EXCLUDED.registry_url,
             catalog_tabs = EXCLUDED.catalog_tabs
           RETURNING
             router_model, default_provider, max_flow_depth, max_flow_fan_out,
             max_flow_tokens, flow_timeout_secs, registry_url, catalog_tabs"#,
    )
    .bind(&body.router_model)
    .bind(&body.default_provider)
    .bind(body.max_flow_depth)
    .bind(body.max_flow_fan_out)
    .bind(body.max_flow_tokens)
    .bind(body.flow_timeout_secs)
    .bind(&body.registry_url)
    .bind(&body.catalog_tabs)
    .fetch_one(&state.db)
    .await;

    match result {
        Ok(s) => Json(s).into_response(),
        Err(e) => {
            tracing::error!(%e, "update_settings: db error");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a row created by another endpoint — `PUT
    /// /api/orchestrator/policy` names only its own two columns — leaves every
    /// column here NULL, and `GET /api/settings` reported those nulls instead
    /// of the defaults it returns when there is no row at all. Same deployment,
    /// same configuration, two different answers depending on which settings
    /// page the operator happened to open first.
    #[test]
    fn a_row_of_nulls_reads_the_same_as_no_row_at_all() {
        let materialised = Settings {
            router_model: None,
            default_provider: None,
            max_flow_depth: None,
            max_flow_fan_out: None,
            max_flow_tokens: None,
            flow_timeout_secs: None,
            registry_url: None,
            catalog_tabs: None,
        }
        .with_defaults();

        let no_row = Settings::defaults();
        assert_eq!(materialised.router_model, no_row.router_model);
        assert_eq!(materialised.default_provider, no_row.default_provider);
        assert_eq!(materialised.max_flow_depth, no_row.max_flow_depth);
        assert_eq!(materialised.max_flow_fan_out, no_row.max_flow_fan_out);
        assert_eq!(materialised.max_flow_tokens, no_row.max_flow_tokens);
        assert_eq!(materialised.flow_timeout_secs, no_row.flow_timeout_secs);
    }

    /// Defaults fill gaps; they never overwrite what the operator stored.
    #[test]
    fn a_stored_value_survives_the_defaults() {
        let stored = Settings {
            router_model: Some("gpt-4o".into()),
            default_provider: None,
            max_flow_depth: Some(1),
            max_flow_fan_out: None,
            max_flow_tokens: None,
            flow_timeout_secs: None,
            registry_url: Some("https://registry.example.com".into()),
            catalog_tabs: Some("a,b".into()),
        }
        .with_defaults();

        assert_eq!(stored.router_model.as_deref(), Some("gpt-4o"));
        assert_eq!(stored.max_flow_depth, Some(1));
        assert_eq!(
            stored.registry_url.as_deref(),
            Some("https://registry.example.com")
        );
        assert_eq!(stored.catalog_tabs.as_deref(), Some("a,b"));
        // …and the gaps are filled.
        assert_eq!(stored.default_provider.as_deref(), Some("openai"));
        assert_eq!(stored.max_flow_fan_out, Some(20));
    }

    /// These two genuinely mean "unset" to their consumers — no registry
    /// configured, and derive the catalog tabs from agent tags — so they must
    /// not acquire a value they never had.
    #[test]
    fn unset_optional_columns_are_not_invented() {
        let filled = Settings::defaults();
        assert!(filled.registry_url.is_none());
        assert!(filled.catalog_tabs.is_none());
    }
}
