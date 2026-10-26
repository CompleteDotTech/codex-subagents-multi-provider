//! Provider capability defaults and resolution of custom provider overrides.

use codex_api::is_azure_responses_provider;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::RemoteCompactionSupport;
use codex_model_provider_info::ResponsesCompatibility;

/// Optional provider-backed features that Codex may expose at runtime.
///
/// These capabilities are a provider-owned upper bound. Model and turn settings
/// can further restrict the functionality exposed to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub image_generation: bool,
    pub web_search: bool,
    pub external_web_access: bool,
    pub remote_compaction: RemoteCompactionSupport,
    pub responses: ResponsesCompatibility,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            image_generation: true,
            web_search: true,
            external_web_access: true,
            remote_compaction: RemoteCompactionSupport::Unsupported,
            responses: ResponsesCompatibility::Full,
        }
    }
}

impl ProviderCapabilities {
    pub(crate) fn from_config(info: &ModelProviderInfo) -> Self {
        let defaults = Self {
            remote_compaction: if info.is_openai()
                || is_azure_responses_provider(&info.name, info.base_url.as_deref())
            {
                RemoteCompactionSupport::V2
            } else {
                RemoteCompactionSupport::Unsupported
            },
            ..Self::default()
        };
        let overrides = info.capabilities.unwrap_or_default();
        let responses = overrides.responses.unwrap_or_default();
        let stateless = responses == ResponsesCompatibility::StatelessFunctions;
        Self {
            image_generation: !stateless && defaults.image_generation,
            web_search: !stateless && defaults.web_search,
            external_web_access: !stateless
                && overrides
                    .external_web_access
                    .unwrap_or(defaults.external_web_access),
            remote_compaction: if stateless {
                RemoteCompactionSupport::Unsupported
            } else {
                overrides
                    .remote_compaction
                    .unwrap_or(defaults.remote_compaction)
            },
            responses,
        }
    }
}

#[cfg(test)]
#[path = "capabilities_tests.rs"]
mod tests;
