//! How a registered endpoint speaks the OpenAI wire protocol.
//!
//! Custom (DB-registered) providers are reached through the OpenAI spoke, because
//! their request and response *bodies* are the OpenAI ones. What differs between
//! them is only the envelope: where the URL puts the model, which header carries the
//! credential, and whether a query string is required. [`ProviderDialect`] is that
//! envelope, so one client covers every OpenAI-shaped endpoint instead of a fork per
//! vendor.
//!
//! Three call sites share it — the provider client ([`super::OpenAiProvider`]), the
//! model-catalog sync ([`crate::routing::catalog`]) and the server's registration
//! probe — so an endpoint is described once and every path agrees on how to reach it.

/// Wire dialect of an OpenAI-shaped endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderDialect {
    /// Plain OpenAI-compatible (OpenAI itself, vLLM, DeepSeek, Together, a private
    /// gateway …): `POST {base}/chat/completions`, `Authorization: Bearer <key>`,
    /// `GET {base}/models`.
    OpenAi,
    /// Azure OpenAI's classic data plane. Three differences from the above, all in
    /// the envelope: the model is a **deployment** name carried in the path rather
    /// than only in the body, the credential is an `api-key` header (`Authorization:
    /// Bearer` there means an Entra ID token, not an API key), and every call needs
    /// an `?api-version=`.
    AzureOpenAi { api_version: String },
    /// AWS Bedrock Converse API. The model is in the URL path
    /// (`/model/{model}/converse`), the credential is a bearer token, and the request
    /// and response bodies are Bedrock's own shape (not OpenAI). Supports all Bedrock
    /// models including INFERENCE_PROFILE-only ones (Claude, GPT-6, Grok, …) that the
    /// OpenAI-compatible Mantle endpoint cannot serve.
    BedrockConverse,
}

/// Stored `custom_providers.kind` for [`ProviderDialect::OpenAi`].
pub const KIND_OPENAI: &str = "openai";
/// Stored `custom_providers.kind` for [`ProviderDialect::AzureOpenAi`].
pub const KIND_AZURE_OPENAI: &str = "azure-openai";
/// Stored `custom_providers.kind` for [`ProviderDialect::BedrockConverse`].
pub const KIND_BEDROCK_CONVERSE: &str = "bedrock-converse";

/// The `api-version` used for the deployment *listing* only. Azure's data-plane
/// deployments list is an older surface than the inference API and was dropped from
/// the newer api-versions, so pinning the listing here keeps model discovery working
/// while the admin picks a current api-version for inference. A resource that no
/// longer answers it simply falls through to the manual model-list escape hatch.
pub const AZURE_DEPLOYMENTS_LIST_API_VERSION: &str = "2023-03-15-preview";

impl ProviderDialect {
    /// Build a dialect from the stored `kind` + `api_version` columns. An unknown
    /// kind degrades to [`ProviderDialect::OpenAi`] rather than failing the call:
    /// the column is CHECK-constrained, so a surprise here means a newer writer, and
    /// the OpenAI shape is the one every endpoint is most likely to answer.
    pub fn from_kind(kind: &str, api_version: Option<&str>) -> Self {
        match kind {
            KIND_AZURE_OPENAI => Self::AzureOpenAi {
                // The DB CHECK guarantees a version for this kind; the fallback only
                // covers a row written around it.
                api_version: api_version.unwrap_or("2024-10-21").to_string(),
            },
            KIND_BEDROCK_CONVERSE => Self::BedrockConverse,
            _ => Self::OpenAi,
        }
    }

    /// The stored `custom_providers.kind` for this dialect.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::OpenAi => KIND_OPENAI,
            Self::AzureOpenAi { .. } => KIND_AZURE_OPENAI,
            Self::BedrockConverse => KIND_BEDROCK_CONVERSE,
        }
    }

    /// Normalize an admin-entered base URL: drop a trailing slash, and for Azure also
    /// drop a trailing `/openai` so both `https://r.openai.azure.com` and
    /// `https://r.openai.azure.com/openai` (the form the portal shows) work — this
    /// dialect appends the `/openai` segment itself.
    pub fn normalize_base(&self, base: &str) -> String {
        let base = base.trim().trim_end_matches('/');
        match self {
            Self::OpenAi | Self::BedrockConverse => base.to_string(),
            Self::AzureOpenAi { .. } => base
                .strip_suffix("/openai")
                .unwrap_or(base)
                .trim_end_matches('/')
                .to_string(),
        }
    }

    /// Chat-completions URL for `model` (a deployment name under Azure, a model ID
    /// under Bedrock Converse).
    pub fn chat_url(&self, base: &str, model: &str) -> String {
        match self {
            Self::BedrockConverse => {
                let base = self.normalize_base(base);
                format!("{base}/model/{model}/converse")
            }
            _ => self.deployment_url(base, model, "chat/completions"),
        }
    }

    /// Streaming URL. Bedrock Converse has a separate `/converse-stream` endpoint;
    /// OpenAI and Azure use the same URL with `stream: true` in the body.
    pub fn chat_stream_url(&self, base: &str, model: &str) -> String {
        match self {
            Self::BedrockConverse => {
                let base = self.normalize_base(base);
                format!("{base}/model/{model}/converse-stream")
            }
            _ => self.chat_url(base, model),
        }
    }

    /// Embeddings URL for `model` (a deployment name under Azure).
    pub fn embeddings_url(&self, base: &str, model: &str) -> String {
        self.deployment_url(base, model, "embeddings")
    }

    /// Model-listing URL. Azure has no `/models` listing of *deployments*, which is
    /// what our catalog needs (`/openai/models` lists what the region offers, not
    /// what this resource has deployed), so it lists deployments instead. Both answer
    /// `{"data": [{"id": …}]}`, which is the shape the catalog already parses.
    pub fn models_url(&self, base: &str) -> String {
        match self {
            Self::OpenAi => format!("{}/models", self.normalize_base(base)),
            Self::AzureOpenAi { .. } => format!(
                "{}/openai/deployments?api-version={AZURE_DEPLOYMENTS_LIST_API_VERSION}",
                self.normalize_base(base)
            ),
            // Bedrock Runtime has no /models endpoint. The catalog sync uses the
            // control-plane API which returns all Converse-capable models with
            // their inference type (ON_DEMAND vs INFERENCE_PROFILE).
            Self::BedrockConverse => {
                let url = bedrock_control_plane_url(base);
                format!("{url}/foundation-models")
            }
        }
    }

    /// Attach this dialect's credential header to a request.
    pub fn authorize(
        &self,
        req: reqwest::RequestBuilder,
        api_key: &str,
    ) -> reqwest::RequestBuilder {
        match self {
            Self::OpenAi | Self::BedrockConverse => req.bearer_auth(api_key),
            Self::AzureOpenAi { .. } => req.header("api-key", api_key),
        }
    }

    /// A parameter this dialect's error body reports as unaccepted, beyond the OpenAI
    /// `{"error":{"param":…,"code":"unsupported_parameter"}}` shape the caller already
    /// handles. Azure rejects a param the api-version doesn't know with a plain
    /// message and no `param` field, so the field name is read out of the message.
    /// Feeds the same drop-and-retry path as the OpenAI shape.
    pub fn extra_droppable_param(&self, body: &str) -> Option<String> {
        match self {
            Self::OpenAi | Self::BedrockConverse => None,
            Self::AzureOpenAi { .. } => {
                let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
                let message = parsed.get("error")?.get("message")?.as_str()?;
                azure_unrecognized_argument(message)
            }
        }
    }

    /// `{base}/openai/deployments/{model}/{suffix}?api-version=…` for Azure, plain
    /// `{base}/{suffix}` otherwise. Azure deployment names are limited to letters,
    /// digits, `-` and `_`, so they need no percent-encoding.
    fn deployment_url(&self, base: &str, model: &str, suffix: &str) -> String {
        let base = self.normalize_base(base);
        match self {
            Self::OpenAi => format!("{base}/{suffix}"),
            Self::AzureOpenAi { api_version } => {
                format!("{base}/openai/deployments/{model}/{suffix}?api-version={api_version}")
            }
            // BedrockConverse URLs are built directly by chat_url/chat_stream_url;
            // this arm is never reached but must be exhaustive.
            Self::BedrockConverse => format!("{base}/model/{model}/{suffix}"),
        }
    }
}

/// Derive the Bedrock control-plane URL from a Bedrock Runtime base URL.
/// `https://bedrock-runtime.us-west-2.amazonaws.com` → `https://bedrock.us-west-2.amazonaws.com`.
pub fn bedrock_control_plane_url(runtime_base: &str) -> String {
    if let Some(region) = bedrock_region(runtime_base) {
        format!("https://bedrock.{region}.amazonaws.com")
    } else {
        runtime_base.trim_end_matches('/').to_string()
    }
}

/// Extract the AWS region from a Bedrock Runtime base URL.
/// `https://bedrock-runtime.us-west-2.amazonaws.com` → `us-west-2`.
pub fn bedrock_region(base: &str) -> Option<&str> {
    let host = base.split("//").nth(1)?.split('/').next()?;
    host.strip_prefix("bedrock-runtime.")?
        .strip_suffix(".amazonaws.com")
}

/// Map an AWS region to the inference-profile prefix. INFERENCE_PROFILE models
/// must be called with this prefix; ON_DEMAND models must NOT have it.
pub fn bedrock_region_prefix(region: &str) -> &str {
    if region.starts_with("us-") {
        "us"
    } else if region.starts_with("eu-") {
        "eu"
    } else if region.starts_with("ap-") {
        "ap"
    } else {
        "us"
    }
}

/// Pull the first field name out of Azure's "Unrecognized request argument supplied:
/// max_completion_tokens" (and its plural, comma-separated form). One name per call is
/// enough — the executor drops it and retries, so a second unknown param is reported
/// again on the next attempt.
fn azure_unrecognized_argument(message: &str) -> Option<String> {
    // Matches both "…argument supplied:" and the plural "…arguments supplied:".
    if !message.contains("nrecognized request argument") {
        return None;
    }
    let tail = message.split_once("supplied:")?.1;
    let first = tail.split(',').next()?.trim();
    let name: String = first
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn azure() -> ProviderDialect {
        ProviderDialect::AzureOpenAi {
            api_version: "2024-10-21".into(),
        }
    }

    #[test]
    fn openai_urls_are_the_plain_shape() {
        let d = ProviderDialect::OpenAi;
        assert_eq!(
            d.chat_url("https://gw.internal/v1", "llama-3.1-8b"),
            "https://gw.internal/v1/chat/completions"
        );
        assert_eq!(
            d.embeddings_url("https://gw.internal/v1/", "e5"),
            "https://gw.internal/v1/embeddings"
        );
        assert_eq!(
            d.models_url("https://gw.internal/v1"),
            "https://gw.internal/v1/models"
        );
    }

    #[test]
    fn azure_puts_the_deployment_in_the_path_with_an_api_version() {
        let d = azure();
        assert_eq!(
            d.chat_url("https://acme.openai.azure.com", "prod-gpt4o"),
            "https://acme.openai.azure.com/openai/deployments/prod-gpt4o/chat/completions?api-version=2024-10-21"
        );
        assert_eq!(
            d.embeddings_url("https://acme.openai.azure.com", "embed-3"),
            "https://acme.openai.azure.com/openai/deployments/embed-3/embeddings?api-version=2024-10-21"
        );
        assert_eq!(
            d.models_url("https://acme.openai.azure.com"),
            format!(
                "https://acme.openai.azure.com/openai/deployments?api-version={AZURE_DEPLOYMENTS_LIST_API_VERSION}"
            )
        );
    }

    #[test]
    fn azure_base_accepts_both_portal_forms() {
        let d = azure();
        for base in [
            "https://acme.openai.azure.com",
            "https://acme.openai.azure.com/",
            "https://acme.openai.azure.com/openai",
            "https://acme.openai.azure.com/openai/",
        ] {
            assert_eq!(
                d.normalize_base(base),
                "https://acme.openai.azure.com",
                "base {base}"
            );
        }
        // Only Azure strips `/openai` — an OpenAI-compatible endpoint may legitimately
        // be mounted at a path ending in it.
        assert_eq!(
            ProviderDialect::OpenAi.normalize_base("https://gw.internal/openai/"),
            "https://gw.internal/openai"
        );
    }

    #[test]
    fn from_kind_round_trips_and_degrades_to_openai() {
        assert_eq!(
            ProviderDialect::from_kind(KIND_AZURE_OPENAI, Some("2025-01-01-preview")),
            ProviderDialect::AzureOpenAi {
                api_version: "2025-01-01-preview".into()
            }
        );
        assert_eq!(
            ProviderDialect::from_kind(KIND_OPENAI, None),
            ProviderDialect::OpenAi
        );
        assert_eq!(
            ProviderDialect::from_kind(KIND_BEDROCK_CONVERSE, None),
            ProviderDialect::BedrockConverse
        );
        assert_eq!(
            ProviderDialect::from_kind("something-new", None),
            ProviderDialect::OpenAi
        );
        assert_eq!(azure().kind(), KIND_AZURE_OPENAI);
        assert_eq!(ProviderDialect::OpenAi.kind(), KIND_OPENAI);
        assert_eq!(
            ProviderDialect::BedrockConverse.kind(),
            KIND_BEDROCK_CONVERSE
        );
    }

    #[test]
    fn bedrock_converse_urls_put_model_in_path() {
        let d = ProviderDialect::BedrockConverse;
        assert_eq!(
            d.chat_url(
                "https://bedrock-runtime.us-west-2.amazonaws.com",
                "us.openai.gpt-6-astra"
            ),
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/us.openai.gpt-6-astra/converse"
        );
        assert_eq!(
            d.chat_stream_url(
                "https://bedrock-runtime.us-west-2.amazonaws.com",
                "deepseek.v3.2"
            ),
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/deepseek.v3.2/converse-stream"
        );
    }

    #[test]
    fn bedrock_models_url_derives_control_plane_endpoint() {
        let d = ProviderDialect::BedrockConverse;
        assert_eq!(
            d.models_url("https://bedrock-runtime.us-west-2.amazonaws.com"),
            "https://bedrock.us-west-2.amazonaws.com/foundation-models"
        );
    }

    #[test]
    fn bedrock_region_extraction() {
        assert_eq!(
            bedrock_region("https://bedrock-runtime.us-west-2.amazonaws.com"),
            Some("us-west-2")
        );
        assert_eq!(
            bedrock_region("https://bedrock-runtime.eu-west-1.amazonaws.com"),
            Some("eu-west-1")
        );
        assert_eq!(bedrock_region("https://api.openai.com/v1"), None);
    }

    #[test]
    fn bedrock_region_prefix_mapping() {
        assert_eq!(bedrock_region_prefix("us-west-2"), "us");
        assert_eq!(bedrock_region_prefix("eu-west-1"), "eu");
        assert_eq!(bedrock_region_prefix("ap-northeast-1"), "ap");
        assert_eq!(bedrock_region_prefix("unknown"), "us");
    }

    #[test]
    fn azure_unknown_param_is_read_out_of_the_message() {
        let d = azure();
        let body = r#"{"error":{"code":"BadRequest","message":"Unrecognized request argument supplied: max_completion_tokens"}}"#;
        assert_eq!(
            d.extra_droppable_param(body).as_deref(),
            Some("max_completion_tokens")
        );
        // Plural form: one name per round is enough.
        let plural = r#"{"error":{"message":"Unrecognized request arguments supplied: stream_options, max_completion_tokens"}}"#;
        assert_eq!(
            d.extra_droppable_param(plural).as_deref(),
            Some("stream_options")
        );
        // Anything else is not a param rejection.
        assert_eq!(
            d.extra_droppable_param(r#"{"error":{"message":"quota exceeded"}}"#),
            None
        );
        assert_eq!(d.extra_droppable_param("not json"), None);
        // The plain dialect never uses this path.
        assert_eq!(ProviderDialect::OpenAi.extra_droppable_param(body), None);
    }
}
