//! Serializable capability overrides for custom Responses-compatible providers.
//!
//! Unspecified capabilities inherit the provider's existing defaults. Model and
//! turn settings may further restrict the capabilities declared here.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Overrides for the optional API features supported by a custom provider.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelProviderCapabilities {
    /// Whether hosted web search may access the live web.
    pub external_web_access: Option<bool>,
    /// Remote context-compaction protocol; omission preserves provider defaults.
    pub remote_compaction: Option<RemoteCompactionSupport>,
    /// Responses request/history dialect. This cannot grant tool or filesystem authority.
    pub responses: Option<ResponsesCompatibility>,
}

/// Wire compatibility for Responses endpoints, independent of provider or model names.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesCompatibility {
    /// Full Responses protocol, preserving the existing OpenAI-compatible behavior.
    #[default]
    Full,
    /// Stateless HTTP requests with portable history and function tools only.
    StatelessFunctions,
}

/// Remote context-compaction protocols supported by a model provider.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCompactionSupport {
    /// Use local compaction instead of a provider compaction protocol.
    Unsupported,
    /// Send `compaction_trigger` items over the Responses endpoint.
    V2,
}
