use super::*;
use crate::create_model_provider;
use codex_model_provider_info::ModelProviderCapabilities;
use pretty_assertions::assert_eq;

#[test]
fn capability_overrides_preserve_unspecified_provider_defaults() {
    for info in [
        ModelProviderInfo::default(),
        ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        ModelProviderInfo {
            name: "Azure".to_string(),
            ..Default::default()
        },
    ] {
        let defaults = create_model_provider(info.clone(), /*auth_manager*/ None).capabilities();
        let provider = create_model_provider(
            ModelProviderInfo {
                capabilities: Some(ModelProviderCapabilities {
                    external_web_access: Some(false),
                    ..Default::default()
                }),
                ..info
            },
            /*auth_manager*/ None,
        );
        assert_eq!(
            provider.capabilities(),
            ProviderCapabilities {
                external_web_access: false,
                ..defaults
            },
        );
    }
}

#[test]
fn stateless_function_profile_is_generic_and_restricts_hosted_features() {
    use codex_model_provider_info::ResponsesCompatibility;
    for name in ["first-compatible", "second-compatible"] {
        let provider = create_model_provider(
            ModelProviderInfo {
                name: name.into(),
                capabilities: Some(ModelProviderCapabilities {
                    responses: Some(ResponsesCompatibility::StatelessFunctions),
                    remote_compaction: Some(RemoteCompactionSupport::V2),
                    external_web_access: Some(true),
                }),
                ..Default::default()
            },
            /*auth_manager*/ None,
        );
        let capabilities = provider.capabilities();
        assert_eq!(
            capabilities.responses,
            ResponsesCompatibility::StatelessFunctions
        );
        assert!(!capabilities.web_search);
        assert!(!capabilities.image_generation);
        assert!(!capabilities.external_web_access);
        assert_eq!(
            capabilities.remote_compaction,
            RemoteCompactionSupport::Unsupported
        );
    }
    let builtins =
        codex_model_provider_info::built_in_model_providers(/*openai_base_url*/ None);
    assert_eq!(
        create_model_provider(builtins["ollama"].clone(), /*auth_manager*/ None)
            .capabilities()
            .responses,
        ResponsesCompatibility::StatelessFunctions
    );
    assert_eq!(
        create_model_provider(builtins["openai"].clone(), /*auth_manager*/ None)
            .capabilities()
            .responses,
        ResponsesCompatibility::Full
    );
}
