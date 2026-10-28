use super::*;
use crate::config::ConfigBuilder;
use crate::config::ConfigOverrides;
use crate::thread_manager::StartThreadOptions;
use codex_config::child_provider::ChildProviderRequest;
use codex_protocol::openai_models::ModelsResponse;
use pretty_assertions::assert_eq;

fn prepared_source(
    parent: &crate::CodexThread,
    config: &Config,
) -> codex_protocol::protocol::SessionSource {
    codex_protocol::protocol::SessionSource::SubAgent(
        codex_protocol::protocol::SubAgentSource::ThreadSpawn {
            parent_thread_id: parent.session.thread_id,
            depth: crate::agent::next_thread_spawn_depth(&parent.session_source),
            agent_path: None,
            agent_nickname: None,
            agent_role: config.prepared_child_authorization.as_ref().and_then(
                crate::agent::child_provider_authorization::PreparedChildAuthorization::role_name,
            ),
        },
    )
}

async fn start_prepared_child(
    parent: &Arc<crate::CodexThread>,
    mut config: Config,
) -> Arc<crate::CodexThread> {
    let runtime = &parent.session.services.local_agent_runtime;
    let source = prepared_source(parent, &config);
    let authorization = config.prepared_child_authorization.take().unwrap();
    let permit = authorization
        .consume(
            runtime,
            parent.session.thread_id,
            &config,
            /*full_history*/ false,
            &source,
        )
        .await
        .unwrap();
    let child = runtime
        .upgrade()
        .unwrap()
        .spawn_child_thread(
            StartThreadOptions {
                history_mode: Some(codex_protocol::protocol::ThreadHistoryMode::Legacy),
                session_source: Some(source),
                ..StartThreadOptions::new(config)
            },
            runtime.control(parent.session.services.agent_control.identity()),
            Some(parent.session.thread_id),
            /*inherited_exec_policy*/ None,
            permit,
        )
        .await
        .unwrap()
        .thread;
    let step = StepContext::for_test(child.session.new_default_turn().await);
    child
        .session
        .record_context_updates_and_set_reference_context_item(&step)
        .await
        .unwrap();
    child
}

struct Fixture {
    _host: tempfile::TempDir,
    _project: tempfile::TempDir,
    config: Config,
    manager: crate::ThreadManager,
    parent: Arc<crate::CodexThread>,
}

impl Fixture {
    async fn start_native_child(&self, config: Config) -> Arc<crate::CodexThread> {
        start_prepared_child(&self.parent, config).await
    }
    async fn new() -> Self {
        Self::with_fast_mode(/*fast_mode*/ None).await
    }

    async fn with_fast_mode(fast_mode: Option<bool>) -> Self {
        Self::with_binding(fast_mode, /*binding*/ None).await
    }

    async fn with_binding(
        fast_mode: Option<bool>,
        binding: Option<codex_config::CloudConfigBundleBinding>,
    ) -> Self {
        Self::with_binding_and_guardian(fast_mode, binding, /*guardian_threshold*/ None).await
    }

    async fn with_binding_and_guardian(
        fast_mode: Option<bool>,
        binding: Option<codex_config::CloudConfigBundleBinding>,
        guardian_threshold: Option<f64>,
    ) -> Self {
        Self::with_admission_setup(
            fast_mode,
            binding,
            guardian_threshold,
            /*thread_instructions_provider*/ None,
            /*child_base_url*/ None,
        )
        .await
    }

    async fn with_admission_setup(
        fast_mode: Option<bool>,
        binding: Option<codex_config::CloudConfigBundleBinding>,
        guardian_threshold: Option<f64>,
        thread_instructions_provider: Option<
            Arc<dyn codex_extension_api::ThreadInstructionsProvider>,
        >,
        child_base_url: Option<&str>,
    ) -> Self {
        let host = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            host.path().join("config.toml"),
            r#"
approval_policy = "on-request"
[agents]
max_depth = 5
allowed_model_providers = ["zai", "ollama", "openai", "offline"]
[agents.provider_defaults.zai]
model = "shared"
model_catalog_json = "zai.json"
[agents.provider_defaults.offline]
model = "gpt-5.5"
[agents.provider_defaults.ollama]
model = "shared"
model_catalog_json = "ollama.json"
[agents.tier]
description = "Select an explicit service tier"
config_file = "tier.toml"
[agents.reasoning]
description = "Override reasoning effort"
config_file = "reasoning.toml"
[agents.default_tier]
description = "Use the default tier sentinel"
config_file = "default-tier.toml"
[model_providers.zai]
name = "Fixture Z.ai"
base_url = "http://127.0.0.1:1/v1"
[model_providers.offline]
name = "Unavailable authoritative catalog"
base_url = "http://127.0.0.1:1/v1"
model_catalog_url = "http://127.0.0.1:1/catalog"
request_max_retries = 0
"#,
        )
        .unwrap();
        std::fs::write(host.path().join("tier.toml"), "service_tier = 'flex'\n").unwrap();
        std::fs::write(
            host.path().join("default-tier.toml"),
            "service_tier = 'default'\n",
        )
        .unwrap();
        std::fs::write(
            host.path().join("reasoning.toml"),
            "model_reasoning_effort = 'low'\n",
        )
        .unwrap();
        if let Some(threshold) = guardian_threshold {
            let path = host.path().join("config.toml");
            let mut contents = std::fs::read_to_string(&path).unwrap();
            contents.push_str(&format!(
                "\n[features.guardianv2]\nenabled=true\nreview_threshold={threshold}\n"
            ));
            std::fs::write(path, contents).unwrap();
        }
        if let Some(base_url) = child_base_url {
            let script = host.path().join(if cfg!(windows) {
                "admission-helper.cmd"
            } else {
                "admission-helper.sh"
            });
            std::fs::write(&script, if cfg!(windows) {
                "@echo off\r\necho call>>native-admission-helper.calls\r\necho synthetic-token\r\n"
            } else { "printf 'call\\n' >> native-admission-helper.calls\nprintf synthetic-token\n" }).unwrap();
            let auth = if cfg!(windows) {
                serde_json::json!({"command":"cmd.exe","args":["/D","/C",script],"cwd":host.path(),"timeout_ms":5000,"refresh_interval_ms":0})
            } else {
                serde_json::json!({"command":"sh","args":[script],"cwd":host.path(),"timeout_ms":5000,"refresh_interval_ms":0})
            };
            let path = host.path().join("config.toml");
            let mut contents = std::fs::read_to_string(&path).unwrap().replacen(
                "base_url = \"http://127.0.0.1:1/v1\"",
                &format!("base_url = {}", toml::Value::String(base_url.to_owned())),
                1,
            );
            contents.push_str("\n[model_providers.zai.auth]\n");
            contents.push_str(&toml::to_string(&auth).unwrap());
            std::fs::write(path, contents).unwrap();
        }
        let mut config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(host.path().to_path_buf())
            .harness_overrides(ConfigOverrides {
                cwd: Some(project.path().to_path_buf()),
                ..Default::default()
            })
            .build()
            .await
            .unwrap();
        config.config_layer_stack = config
            .config_layer_stack
            .clone()
            .with_cloud_config_binding(binding);
        if let Some(enabled) = fast_mode {
            if enabled {
                config.features.enable(Feature::FastMode).unwrap();
            } else {
                config.features.disable(Feature::FastMode).unwrap();
            }
        }
        for role in ["tier", "reasoning", "default_tier"] {
            assert!(
                config.agent_roles.contains_key(role),
                "fixture role `{role}` was not loaded: {:?}",
                config.startup_warnings
            );
        }
        let manager = crate::ThreadManager::with_models_provider_and_home_for_tests(
            codex_login::CodexAuth::from_api_key("parent-only"),
            config.model_provider.clone(),
            host.path().to_path_buf(),
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        );
        let parent = manager
            .start_thread(StartThreadOptions {
                thread_instructions_provider,
                ..StartThreadOptions::new(config.clone())
            })
            .await
            .unwrap()
            .thread;
        let parent_info = parent
            .session
            .new_default_turn()
            .await
            .model_info()
            .as_ref()
            .clone();
        for (provider, window, effort) in [
            ("zai", 32_000, ReasoningEffort::Low),
            ("ollama", 8_000, ReasoningEffort::Medium),
        ] {
            let mut info = parent_info.clone();
            info.slug = "shared".into();
            info.context_window = Some(window);
            info.max_context_window = Some(window);
            info.default_reasoning_level = Some(effort.clone());
            info.supported_reasoning_levels = vec![ReasoningEffortPreset {
                effort,
                description: "fixture".into(),
            }];
            info.service_tiers.clear();
            info.additional_speed_tiers.clear();
            info.default_service_tier = None;
            info.used_fallback_model_metadata = false;
            std::fs::write(
                host.path().join(format!("{provider}.json")),
                serde_json::to_vec(&ModelsResponse { models: vec![info] }).unwrap(),
            )
            .unwrap();
        }
        Self {
            _host: host,
            _project: project,
            config,
            manager,
            parent,
        }
    }

    async fn resolved(&self, provider: &str) -> ResolvedChildProvider {
        let turn = self.parent.session.new_default_turn().await;
        self.config
            .child_provider_policy
            .resolve(ChildProviderRequest {
                explicit_provider: Some(provider),
                role_provider: None,
                parent_provider_id: &self.config.model_provider_id,
                parent_provider: &self.config.model_provider,
                parent_model: &turn.model_info().slug,
                authorization_origin: None,
                explicit_model: None,
                role_model: None,
                default_subagent_model: None,
                full_history: false,
                host_owned: false,
            })
            .unwrap()
    }

    async fn prepared(
        &self,
        resolved: &ResolvedChildProvider,
        role: Option<&str>,
        effort: Option<ReasoningEffort>,
    ) -> Result<PreparedSpawnConfig, String> {
        let turn = self.parent.session.new_default_turn().await;
        let step = StepContext::for_test(turn);
        prepare_agent_spawn_config(
            &self.parent.session,
            &step,
            SpawnConfigOptions {
                model_provider: None,
                resolved_provider: Some(resolved),
                version: SpawnConfigVersion::V2,
                full_history_fork: false,
                role_name: role,
                model: None,
                reasoning_effort: effort,
            },
        )
        .await
    }
}

#[tokio::test]
async fn selected_same_slug_catalog_reaches_startup_without_changing_parent_or_sibling() {
    let fixture = Fixture::new().await;
    let parent_turn = fixture.parent.session.new_default_turn().await;
    let parent_info = parent_turn.model_info().clone();
    let zai = fixture.resolved("zai").await;
    let ollama = fixture.resolved("ollama").await;
    let (zai, ollama) = tokio::join!(
        fixture.prepared(&zai, /*role*/ None, /*effort*/ None),
        fixture.prepared(&ollama, /*role*/ None, /*effort*/ None)
    );
    let zai = zai.unwrap().config;
    let ollama = ollama.unwrap().config;
    assert_eq!(zai.model_reasoning_effort, Some(ReasoningEffort::Low));
    assert_eq!(ollama.model_reasoning_effort, Some(ReasoningEffort::Medium));
    assert_eq!(zai.model_context_window, None);
    assert_eq!(zai.service_tier, None);
    // Actual thread startup must consume this manager instead of the root's manager.
    let child_a = fixture.start_native_child(zai).await;
    let child_b = fixture.start_native_child(ollama).await;
    let turn_a = child_a.session.new_default_turn().await;
    let turn_b = child_b.session.new_default_turn().await;
    assert_eq!(turn_a.model_info().slug, "shared");
    assert_eq!(turn_a.model_info().context_window, Some(32_000));
    assert_eq!(turn_b.model_info().context_window, Some(8_000));
    assert!(turn_a.config.prepared_child_models.is_none());
    assert!(turn_b.config.prepared_child_models.is_none());
    assert!(
        build_agent_resume_config(&turn_a)
            .unwrap()
            .prepared_child_models
            .is_none()
    );
    let sibling = fixture
        .manager
        .start_thread(StartThreadOptions::new(fixture.config.clone()))
        .await
        .unwrap()
        .thread;
    assert_eq!(
        fixture.parent.session.new_default_turn().await.model_info(),
        &parent_info
    );
    assert_eq!(
        sibling.session.new_default_turn().await.model_info(),
        &parent_info
    );
}

#[tokio::test]
async fn prepared_scope_cannot_survive_provider_catalog_model_or_auth_changes() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let prepared = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await
        .unwrap()
        .config;
    for mutation in 0..5 {
        let mut config = prepared.clone();
        match mutation {
            0 => config.model_provider_id = "ollama".into(),
            1 => config.model_provider.base_url = Some("https://changed.invalid/v1".into()),
            2 => config.model_catalog.as_mut().unwrap().models[0].context_window = Some(1),
            3 => config.model = Some("changed".into()),
            _ => {}
        }
        let auth = if mutation == 4 {
            codex_login::AuthManager::from_auth_for_testing(codex_login::CodexAuth::from_api_key(
                "other",
            ))
        } else {
            Arc::clone(&fixture.parent.session.services.auth_manager)
        };
        assert!(
            take_prepared_child_models(&mut config, &auth)
                .unwrap_err()
                .contains("scope changed")
        );
        assert!(config.prepared_child_models.is_none());
    }
}

#[tokio::test]
async fn unknown_model_and_unsupported_explicit_effort_fail_without_parent_fallback() {
    let fixture = Fixture::new().await;
    let mut resolved = fixture.resolved("zai").await;
    let result = fixture
        .prepared(&resolved, /*role*/ None, Some(ReasoningEffort::High))
        .await;
    assert!(
        result
            .err()
            .unwrap()
            .contains("child_provider_capability_unsupported")
    );
    resolved.model = "gpt-5.5".into();
    let result = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await;
    assert!(
        result
            .err()
            .unwrap()
            .contains("child_provider_model_unavailable")
    );
}

#[tokio::test]
async fn failed_target_catalog_discovery_cannot_validate_a_bundled_openai_slug() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("offline").await;
    let result = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await;
    assert!(
        result
            .err()
            .unwrap()
            .contains("child_provider_model_unavailable")
    );
}

#[tokio::test]
async fn prepared_provider_selection_cannot_be_reused_for_a_full_history_switch() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    std::fs::remove_file(resolved.model_catalog_json.as_ref().unwrap()).unwrap();
    let turn = fixture.parent.session.new_default_turn().await;
    let step = StepContext::for_test(turn);
    let result = prepare_agent_spawn_config(
        &fixture.parent.session,
        &step,
        SpawnConfigOptions {
            model_provider: None,
            resolved_provider: Some(&resolved),
            version: SpawnConfigVersion::V2,
            full_history_fork: true,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await;
    assert!(
        result
            .err()
            .unwrap()
            .contains("child_provider_history_unsupported")
    );
}

#[tokio::test]
async fn explicit_role_tier_is_validated_even_when_equal_to_inherited_value() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let result = fixture
        .prepared(&resolved, Some("tier"), /*effort*/ None)
        .await;
    assert!(
        result
            .err()
            .unwrap()
            .contains("child_provider_capability_unsupported")
    );
    let mut config = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await
        .unwrap()
        .config;
    config.service_tier = Some("flex".into());
    let manager = config
        .prepared_child_models
        .as_ref()
        .unwrap()
        .manager
        .clone();
    let role = prepare_role(&config, Some("tier")).await.unwrap();
    let explicit = apply_spawn_agent_role(&manager, &mut config, &role)
        .await
        .unwrap();
    assert_eq!(explicit, Some("flex".into()));
    let default = fixture
        .prepared(&resolved, Some("default_tier"), /*effort*/ None)
        .await
        .unwrap();
    assert_eq!(
        default.config.service_tier,
        Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.into())
    );
}

#[tokio::test]
async fn advertised_custom_role_tier_requires_the_same_feature_gate_as_runtime() {
    for enabled in [false, true] {
        let fixture = Fixture::with_fast_mode(Some(enabled)).await;
        std::fs::write(
            fixture._host.path().join("tier.toml"),
            "service_tier = 'custom'\n",
        )
        .unwrap();
        let resolved = fixture.resolved("zai").await;
        let path = resolved.model_catalog_json.as_ref().unwrap();
        let mut catalog: ModelsResponse =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        catalog.models[0]
            .service_tiers
            .push(codex_protocol::openai_models::ModelServiceTier {
                id: "custom".into(),
                name: "Custom".into(),
                description: "fixture".into(),
            });
        std::fs::write(path, serde_json::to_vec(&catalog).unwrap()).unwrap();
        let result = fixture
            .prepared(&resolved, Some("tier"), /*effort*/ None)
            .await;
        if !enabled {
            assert!(
                result
                    .err()
                    .unwrap()
                    .contains("child_provider_capability_unsupported")
            );
        } else {
            let prepared = result.unwrap();
            assert_eq!(prepared.config.service_tier.as_deref(), Some("custom"));
            let child = fixture.start_native_child(prepared.config).await;
            let turn = child.session.new_default_turn().await;
            let step = child
                .session
                .capture_step_context(turn, &tokio_util::sync::CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(step.settings.service_tier.as_deref(), Some("custom"));
        }
    }
}

#[tokio::test]
async fn unsupported_inherited_tier_is_omitted_and_default_sentinel_is_preserved() {
    let fixture = Fixture::new().await;
    fixture
        .parent
        .session
        .services
        .agent_control
        .propagate_config_update(crate::agent::api::AgentConfigUpdate::ServiceTier(Some(
            "flex".into(),
        )));
    let resolved = fixture.resolved("zai").await;
    assert_eq!(
        fixture
            .prepared(&resolved, /*role*/ None, /*effort*/ None)
            .await
            .unwrap()
            .config
            .service_tier,
        None
    );
    fixture
        .parent
        .session
        .services
        .agent_control
        .propagate_config_update(crate::agent::api::AgentConfigUpdate::ServiceTier(Some(
            SERVICE_TIER_DEFAULT_REQUEST_VALUE.into(),
        )));
    assert_eq!(
        fixture
            .prepared(&resolved, /*role*/ None, /*effort*/ None)
            .await
            .unwrap()
            .config
            .service_tier,
        Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.into())
    );
}

#[tokio::test]
async fn native_step_does_not_reintroduce_an_unsupported_root_flex_tier() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let prepared = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await
        .unwrap();
    let child = fixture.start_native_child(prepared.config).await;
    child
        .session
        .services
        .agent_control
        .propagate_config_update(crate::agent::api::AgentConfigUpdate::ServiceTier(Some(
            "flex".into(),
        )));
    let turn = child.session.new_default_turn().await;
    let step = child
        .session
        .capture_step_context(turn, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(step.settings.service_tier, None);
}

#[tokio::test]
async fn native_explicit_role_tier_survives_root_updates_and_is_not_inherited_as_role_authority() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let path = resolved.model_catalog_json.as_ref().unwrap();
    let mut catalog: ModelsResponse =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    catalog.models[0]
        .service_tiers
        .push(codex_protocol::openai_models::ModelServiceTier {
            id: "flex".into(),
            name: "Flex".into(),
            description: "fixture".into(),
        });
    std::fs::write(path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let prepared = fixture
        .prepared(&resolved, Some("tier"), /*effort*/ None)
        .await
        .unwrap();
    assert!(prepared.config.child_role_service_tier_override);
    let child = fixture.start_native_child(prepared.config).await;
    for root_tier in [None, Some("default".into()), Some("unsupported".into())] {
        child
            .session
            .services
            .agent_control
            .propagate_config_update(crate::agent::api::AgentConfigUpdate::ServiceTier(root_tier));
        let turn = child.session.new_default_turn().await;
        assert!(
            !build_agent_resume_config(&turn)
                .unwrap()
                .child_role_service_tier_override
        );
        let step = child
            .session
            .capture_step_context(turn, &tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(step.settings.service_tier.as_deref(), Some("flex"));
    }
}

#[tokio::test]
async fn changed_provider_uses_target_reasoning_default_and_preserves_role_precedence() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let mut config = fixture.config.clone();
    config.agent_default_subagent_reasoning_effort = Some(ReasoningEffort::High);
    prepare_resolved_child_models(
        &fixture.parent.session,
        &mut config,
        &resolved,
        /*requested_model*/ None,
        /*requested_effort*/ None,
    )
    .await
    .unwrap();
    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::Low));
    let prepared = fixture
        .prepared(&resolved, Some("reasoning"), Some(ReasoningEffort::High))
        .await
        .unwrap();
    assert_eq!(
        prepared.config.model_reasoning_effort,
        Some(ReasoningEffort::Low)
    );
}

#[tokio::test]
async fn fresh_same_provider_reload_uses_changed_catalog_prompts_and_limits() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let prepared = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await
        .unwrap();
    let first = fixture.start_native_child(prepared.config).await;
    let turn = first.session.new_default_turn().await;
    let mut info = turn.model_info().as_ref().clone();
    info.context_window = Some(4_000);
    info.max_context_window = Some(4_000);
    info.model_messages.as_mut().unwrap().instructions_template =
        Some("fresh target model instructions".into());
    std::fs::write(
        resolved.model_catalog_json.as_ref().unwrap(),
        serde_json::to_vec(&ModelsResponse { models: vec![info] }).unwrap(),
    )
    .unwrap();
    // Resolve again using the switched immediate parent's actual identity. No
    // authorization origin is invented; current allowlist membership authorizes it.
    let next = fixture
        .config
        .child_provider_policy
        .resolve(ChildProviderRequest {
            explicit_provider: None,
            role_provider: None,
            parent_provider_id: "zai",
            parent_provider: &resolved.provider,
            parent_model: "shared",
            authorization_origin: None,
            explicit_model: None,
            role_model: None,
            default_subagent_model: None,
            full_history: false,
            host_owned: false,
        })
        .unwrap();
    assert!(!next.changed_provider);
    let step = StepContext::for_test(turn);
    let prepared = prepare_agent_spawn_config(
        &first.session,
        &step,
        SpawnConfigOptions {
            model_provider: None,
            resolved_provider: Some(&next),
            version: SpawnConfigVersion::V2,
            full_history_fork: false,
            role_name: None,
            model: None,
            reasoning_effort: None,
        },
    )
    .await
    .unwrap();
    let second = start_prepared_child(&first, prepared.config).await;
    assert_eq!(
        second
            .session
            .new_default_turn()
            .await
            .model_info()
            .context_window,
        Some(4_000)
    );
    assert!(
        second
            .session
            .get_base_instructions()
            .await
            .text
            .contains("fresh target model instructions")
    );
    assert_eq!(
        first
            .session
            .new_default_turn()
            .await
            .model_info()
            .context_window,
        Some(32_000)
    );
}

#[tokio::test]
async fn prepared_launch_rejects_whole_config_mutation_and_clone_replay() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let prepared = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await
        .unwrap()
        .config;
    let runtime = &fixture.parent.session.services.local_agent_runtime;
    for mutation in 0..6 {
        let mut changed = prepared.clone();
        let authorization = changed.prepared_child_authorization.take().unwrap();
        match mutation {
            0 => changed.model_provider.base_url = Some("https://forged.invalid/v1".into()),
            1 => changed.model_catalog.as_mut().unwrap().models[0].context_window = Some(1),
            2 => changed.developer_instructions = Some("forged authority".into()),
            3 => changed.cwd = changed.codex_home.clone(),
            4 => {
                changed
                    .set_legacy_sandbox_policy(
                        codex_protocol::protocol::SandboxPolicy::DangerFullAccess,
                    )
                    .unwrap();
            }
            _ => changed.model_context_window = Some(1),
        }
        assert!(
            authorization
                .consume(
                    runtime,
                    fixture.parent.session.thread_id,
                    &changed,
                    /*full_history*/ false,
                    &prepared_source(&fixture.parent, &fixture.config)
                )
                .await
                .is_err()
        );
    }
    let mut first = prepared.clone();
    let mut second = prepared;
    let permit = first
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            runtime,
            fixture.parent.session.thread_id,
            &first,
            /*full_history*/ false,
            &prepared_source(&fixture.parent, &fixture.config),
        )
        .await
        .unwrap();
    let replay = second
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            runtime,
            fixture.parent.session.thread_id,
            &second,
            /*full_history*/ false,
            &prepared_source(&fixture.parent, &fixture.config),
        )
        .await;
    assert!(replay.is_err_and(|error| error.to_string().contains("already consumed")));
    drop(permit);
}

#[tokio::test]
async fn prepared_launch_rejects_wrong_parent_tree_history_and_stale_host_settings() {
    let fixture = Fixture::new().await;
    let other = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let prepared = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await
        .unwrap()
        .config;
    let runtime = &fixture.parent.session.services.local_agent_runtime;
    for (caller, tree, full_history) in [
        (codex_protocol::ThreadId::new(), runtime, false),
        (
            fixture.parent.session.thread_id,
            &other.parent.session.services.local_agent_runtime,
            false,
        ),
        (fixture.parent.session.thread_id, runtime, true),
    ] {
        let mut changed = prepared.clone();
        assert!(
            changed
                .prepared_child_authorization
                .take()
                .unwrap()
                .consume(
                    tree,
                    caller,
                    &changed,
                    full_history,
                    &prepared_source(&fixture.parent, &fixture.config)
                )
                .await
                .is_err()
        );
    }
    let current = fixture
        .parent
        .thread_settings_snapshot()
        .await
        .approval_policy;
    let replacement = if current == codex_protocol::protocol::AskForApproval::Never {
        codex_protocol::protocol::AskForApproval::OnRequest
    } else {
        codex_protocol::protocol::AskForApproval::Never
    };
    fixture
        .parent
        .update_thread_settings(codex_protocol::protocol::ThreadSettingsOverrides {
            approval_policy: Some(replacement),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut changed = prepared;
    assert!(
        changed
            .prepared_child_authorization
            .take()
            .unwrap()
            .consume(
                runtime,
                fixture.parent.session.thread_id,
                &changed,
                /*full_history*/ false,
                &prepared_source(&fixture.parent, &fixture.config)
            )
            .await
            .is_err_and(|error| error.to_string().contains("authority changed"))
    );
}

#[tokio::test]
async fn public_root_start_cannot_forge_a_delegated_source_or_reuse_prepared_config() {
    let fixture = Fixture::new().await;
    let source = codex_protocol::protocol::SessionSource::SubAgent(
        codex_protocol::protocol::SubAgentSource::ThreadSpawn {
            parent_thread_id: fixture.parent.session.thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        },
    );
    let forged = fixture
        .manager
        .start_thread(StartThreadOptions {
            session_source: Some(source),
            ..StartThreadOptions::new(fixture.config.clone())
        })
        .await;
    assert!(forged.is_err_and(|error| error.to_string().contains("private native launch path")));
    let resolved = fixture.resolved("zai").await;
    let prepared = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await
        .unwrap()
        .config;
    assert!(
        fixture
            .manager
            .start_thread(StartThreadOptions::new(prepared))
            .await
            .is_err_and(|error| error.to_string().contains("independent root"))
    );
}

#[tokio::test]
async fn prepared_role_reads_once_and_ignores_later_role_replacement() {
    let fixture = Fixture::new().await;
    let role = crate::agent::role::prepare_role(&fixture.config, Some("reasoning"))
        .await
        .unwrap();
    std::fs::write(
        fixture._host.path().join("reasoning.toml"),
        "model_provider = 'ollama'\nmodel = 'hostile-new-model'\nmodel_reasoning_effort = 'high'\n",
    )
    .unwrap();
    let mut config = fixture.config.clone();
    let provider = config.model_provider.clone();
    role.apply(&mut config).unwrap();
    assert_eq!(config.model_provider, provider);
    assert_eq!(config.agent_role_model_provider, None);
    assert_eq!(config.model_reasoning_effort, Some(ReasoningEffort::Low));
    assert_ne!(config.model.as_deref(), Some("hostile-new-model"));
}

#[tokio::test]
async fn direct_controller_cannot_start_forged_config_or_run_its_auth_helper() {
    let fixture = Fixture::new().await;
    let mut config = fixture.config.clone();
    let script = fixture._host.path().join(if cfg!(windows) {
        "forged.cmd"
    } else {
        "forged.sh"
    });
    std::fs::write(
        &script,
        if cfg!(windows) {
            "@echo off\r\necho invoked>forged-helper-marker\r\necho synthetic-forged\r\n"
        } else {
            "touch forged-helper-marker\nprintf synthetic-forged\n"
        },
    )
    .unwrap();
    let auth = if cfg!(windows) {
        serde_json::json!({"command":"cmd.exe","args":["/D","/C",script],"cwd":fixture._host.path(),"timeout_ms":5000,"refresh_interval_ms":0})
    } else {
        serde_json::json!({"command":"sh","args":[script],"cwd":fixture._host.path(),"timeout_ms":5000,"refresh_interval_ms":0})
    };
    config.model_provider.auth = Some(serde_json::from_value(auth).unwrap());
    let caller = fixture.parent.session.thread_id;
    let result = fixture
        .parent
        .session
        .services
        .agent_control
        .spawn(crate::agent::api::SpawnRequest {
            caller,
            config,
            input: crate::agent::api::AgentInput::UserInput(vec![
                codex_protocol::user_input::UserInput::Text {
                    text: "forged".into(),
                    text_elements: vec![],
                },
            ]),
            source: codex_protocol::protocol::SessionSource::SubAgent(
                codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                    parent_thread_id: caller,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                },
            ),
            options: crate::agent::types::SpawnAgentOptions {
                parent_thread_id: Some(caller),
                ..Default::default()
            },
        })
        .await;
    assert!(result.is_err_and(|error| error.to_string().contains("host-prepared authorization")));
    assert!(!fixture._host.path().join("forged-helper-marker").exists());
    assert_eq!(fixture.manager.list_thread_ids().await.len(), 1);
}

#[tokio::test]
async fn runtime_permission_change_rechecks_host_catalog_and_definition_provenance() {
    let fixture = Fixture::new().await;
    let environments = fixture
        .parent
        .session
        .inherited_environment_snapshot()
        .await;
    let ready = environments.primary().expect("actual local executor");
    let mut owner = ready.config().clone();
    owner.permission_profile = codex_protocol::models::PermissionProfileSnapshot::legacy(
        codex_protocol::models::PermissionProfile::Disabled,
    );
    let mut selection = ready.selection();
    selection.config = codex_protocol::protocol::EnvironmentConfigState::Ready(owner);
    fixture
        .parent
        .update_thread_settings(codex_protocol::protocol::ThreadSettingsOverrides {
            permission_profile: Some(codex_protocol::models::PermissionProfile::Disabled),
            environments: Some(codex_protocol::protocol::TurnEnvironmentRequests::new(
                fixture.parent.thread_settings_snapshot().await.cwd,
                vec![selection.into_request()],
            )),
            ..Default::default()
        })
        .await
        .unwrap();
    let _current_turn = fixture
        .parent
        .session
        .new_turn_with_default_settings("authority-runtime-update".into(), Default::default())
        .await;
    let actual = fixture
        .parent
        .session
        .inherited_environment_snapshot()
        .await;
    let ready = actual.primary().expect("updated actual executor");
    for path in [
        fixture._host.path().join("config.toml"),
        fixture._host.path().join("zai.json"),
    ] {
        assert!(
            ready
                .config()
                .permission_profile
                .permission_profile()
                .file_system_sandbox_policy()
                .can_write_local_path_with_cwd(&path, fixture._project.path()),
            "actual executor must exercise writable host authority"
        );
    }
    let resolved = fixture.resolved("zai").await;
    let result = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await;
    let error = result.err();
    let turn = fixture.parent.session.new_default_turn().await;
    let step = StepContext::for_test(turn);
    let runtime =
        build_agent_spawn_config(&fixture.parent.session.get_base_instructions().await, &step)
            .unwrap();
    let current = fixture.parent.thread_settings_snapshot().await;
    let filesystem = runtime.permissions.file_system_sandbox_policy();
    let host_config = fixture._host.path().join("config.toml");
    let host_catalog = fixture._host.path().join("zai.json");
    assert!(filesystem.can_write_local_path_with_cwd(&host_config, runtime.cwd.as_path()));
    assert!(filesystem.can_write_local_path_with_cwd(&host_catalog, runtime.cwd.as_path()));
    assert!(
        error
            .as_ref()
            .is_some_and(|error| error.contains("child_provider_untrusted_source")
                || error.contains("child_provider_not_authorized")),
        "writable host authority must be rejected: error={error:?}, config_writable={}, catalog_writable={}, cwd_matches={}, profile_matches={}, roots_match={}, layers_match={}",
        filesystem.can_write_local_path_with_cwd(&host_config, runtime.cwd.as_path()),
        filesystem.can_write_local_path_with_cwd(&host_catalog, runtime.cwd.as_path()),
        current.cwd == runtime.cwd,
        current.permission_profile == runtime.permissions.effective_permission_profile(),
        current
            .runtime_workspace_roots
            .as_ref()
            .is_none_or(|roots| roots == &runtime.workspace_roots),
        fixture.parent.session.get_config().await.config_layer_stack == runtime.config_layer_stack,
    );
    assert_eq!(fixture.manager.list_thread_ids().await.len(), 1);
}

#[tokio::test]
async fn native_preparation_rejects_stale_step_approval_and_reviewer_before_catalog_io() {
    use codex_protocol::config_types::ApprovalsReviewer;
    use codex_protocol::protocol::AskForApproval;
    for reviewer_change in [false, true] {
        let fixture = Fixture::new().await;
        let resolved = fixture.resolved("zai").await;
        let turn = fixture.parent.session.new_default_turn().await;
        let stale_step = StepContext::for_test(Arc::clone(&turn));
        fixture
            .parent
            .update_thread_settings(codex_protocol::protocol::ThreadSettingsOverrides {
                approval_policy: (!reviewer_change).then_some(AskForApproval::Never),
                approvals_reviewer: reviewer_change.then_some(ApprovalsReviewer::AutoReview),
                ..Default::default()
            })
            .await
            .unwrap();
        let catalog = resolved.model_catalog_json.as_ref().unwrap();
        let contents = std::fs::read(catalog).unwrap();
        std::fs::remove_file(catalog).unwrap();
        let rejected = prepare_agent_spawn_config(
            &fixture.parent.session,
            &stale_step,
            SpawnConfigOptions {
                model_provider: Some("zai"),
                resolved_provider: None,
                version: SpawnConfigVersion::V2,
                full_history_fork: false,
                role_name: None,
                model: None,
                reasoning_effort: None,
            },
        )
        .await;
        assert!(
            rejected.is_err_and(|error| error.contains("runtime scope changed before preparation"))
        );
        assert_eq!(fixture.manager.list_thread_ids().await.len(), 1);
        std::fs::write(catalog, contents).unwrap();
        // An idle test turn is not installed as the running turn. Pair a real
        // current host settings snapshot with the retained initial TurnContext,
        // exactly the split a later step has after an active-turn update.
        let mut current = StepContext::for_test(fixture.parent.session.new_default_turn().await);
        Arc::get_mut(&mut current).unwrap().turn = turn;
        let mut prepared = prepare_agent_spawn_config(
            &fixture.parent.session,
            &current,
            SpawnConfigOptions {
                model_provider: Some("zai"),
                resolved_provider: None,
                version: SpawnConfigVersion::V2,
                full_history_fork: false,
                role_name: None,
                model: None,
                reasoning_effort: None,
            },
        )
        .await
        .unwrap()
        .config;
        let settings = fixture.parent.thread_settings_snapshot().await;
        assert_eq!(
            prepared.permissions.approval_policy.value(),
            settings.approval_policy
        );
        assert_eq!(prepared.approvals_reviewer, settings.approvals_reviewer);
        // Changing either field again after preparation revokes that proof.
        fixture
            .parent
            .update_thread_settings(codex_protocol::protocol::ThreadSettingsOverrides {
                approval_policy: (!reviewer_change).then_some(AskForApproval::OnRequest),
                approvals_reviewer: reviewer_change.then_some(ApprovalsReviewer::User),
                ..Default::default()
            })
            .await
            .unwrap();
        let source = prepared_source(&fixture.parent, &prepared);
        let result = prepared
            .prepared_child_authorization
            .take()
            .unwrap()
            .consume(
                &fixture.parent.session.services.local_agent_runtime,
                fixture.parent.session.thread_id,
                &prepared,
                /*full_history*/ false,
                &source,
            )
            .await;
        assert!(result.is_err_and(|error| error.to_string().contains("authority changed")));
        assert_eq!(fixture.manager.list_thread_ids().await.len(), 1);
    }
}

#[tokio::test]
async fn native_legacy_depth_limit_is_enforced_by_actual_parent_authority() {
    async fn prepare(parent: &Arc<crate::CodexThread>) -> Config {
        let step = StepContext::for_test(parent.session.new_default_turn().await);
        prepare_agent_spawn_config(
            &parent.session,
            &step,
            SpawnConfigOptions {
                resolved_provider: None,
                model_provider: None,
                version: SpawnConfigVersion::V1,
                full_history_fork: false,
                role_name: None,
                model: None,
                reasoning_effort: None,
            },
        )
        .await
        .expect("real host preparation before the legacy depth check")
        .config
    }

    let fixture = Fixture::new().await;
    let mut config = fixture.config.clone();
    config.agent_max_depth = 1;
    // This bundled model has no model-owned V2 override; Collab selects V1.
    config.model = Some("gpt-5.5".into());
    config.features.disable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::Collab).unwrap();
    let manager = crate::ThreadManager::with_models_provider_and_home_for_tests(
        codex_login::CodexAuth::from_api_key("parent-only"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config))
        .await
        .unwrap()
        .thread;
    let child = start_prepared_child(&root, prepare(&root).await).await;
    assert_eq!(
        child.multi_agent_version(),
        Some(codex_protocol::protocol::MultiAgentVersion::V1)
    );
    assert_eq!(
        crate::agent::next_thread_spawn_depth(&child.session_source),
        2
    );
    let mut candidate = prepare(&child).await;
    let source = prepared_source(&child, &candidate);
    let error = candidate
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            &child.session.services.local_agent_runtime,
            child.session.thread_id,
            &candidate,
            /*full_history*/ false,
            &source,
        )
        .await
        .err()
        .expect("a correctly parented legacy child cannot exceed the real parent's depth limit");
    assert!(
        error
            .to_string()
            .contains("child source changed its prepared parent, role, or depth"),
        "{error}"
    );
    assert_eq!(manager.list_thread_ids().await.len(), 2);
}

struct RevokeDuringInheritedInstructions {
    policy: codex_config::CloudConfigBundlePolicy,
    sharing: std::sync::atomic::AtomicBool,
    armed: std::sync::atomic::AtomicBool,
    revocations: std::sync::atomic::AtomicUsize,
}

fn revoke_native_child_access(policy: &codex_config::CloudConfigBundlePolicy) {
    let mut revoked = codex_config::CloudConfigBundle::default();
    revoked
        .config_toml
        .enterprise_managed
        .push(codex_config::CloudConfigFragment {
            id: "post-mint-child-revocation".into(),
            name: "revoked native child access".into(),
            contents: "[agents]\nallowed_model_providers=[]".into(),
        });
    policy.observe_remote_bundle(&revoked).unwrap();
}

impl codex_extension_api::ThreadInstructionsProvider for RevokeDuringInheritedInstructions {
    fn share_with_subagents(&self) -> bool {
        if !self.sharing.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.revocations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            revoke_native_child_access(&self.policy);
        }
        true
    }

    fn load_thread_instructions(&self) -> codex_extension_api::LoadInstructionsFuture<'_> {
        Box::pin(async { codex_extension_api::LoadedUserInstructions::default() })
    }
}

#[test_case::test_case(0; "revoked after mint before lower start")]
#[test_case::test_case(1; "revoked during real manager instruction await")]
#[test_case::test_case(2; "unchanged managed admission succeeds")]
#[tokio::test]
async fn native_managed_binding_is_checked_at_actual_startup_admission(stage: u8) {
    let server = wiremock::MockServer::start().await;
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    let provider = Arc::new(RevokeDuringInheritedInstructions {
        policy: policy.clone(),
        sharing: std::sync::atomic::AtomicBool::new(false),
        armed: std::sync::atomic::AtomicBool::new(false),
        revocations: std::sync::atomic::AtomicUsize::new(0),
    });
    let base_url = format!("{}/v1", server.uri());
    let fixture = Fixture::with_admission_setup(
        /*fast_mode*/ None,
        snapshot.binding,
        /*guardian_threshold*/ None,
        Some(provider.clone()),
        Some(&base_url),
    )
    .await;
    // Opt in after actual root registration so its manager retains the direct
    // host provider, rather than the shared wrapper's unconditional opt-in.
    provider
        .sharing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_bytes(std::fs::read(fixture._host.path().join("zai.json")).unwrap()),
        )
        .mount(&server)
        .await;
    let resolved = fixture.resolved("zai").await;
    let mut prepared = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await
        .unwrap()
        .config;
    assert!(
        prepared.model_provider.auth.is_some(),
        "real configured auth-helper path must be present"
    );
    let source = prepared_source(&fixture.parent, &prepared);
    let runtime = &fixture.parent.session.services.local_agent_runtime;
    let permit = prepared
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            runtime,
            fixture.parent.session.thread_id,
            &prepared,
            /*full_history*/ false,
            &source,
        )
        .await
        .expect("permit must mint successfully before the revocation under test");
    let marker = fixture._host.path().join("native-admission-helper.calls");
    let read_marker = || match std::fs::read(&marker) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("cannot read synthetic helper receipt: {error}"),
    };
    let helper_before = read_marker();
    let requests_before = server
        .received_requests()
        .await
        .expect("request recording is enabled")
        .len();
    if stage == 0 {
        revoke_native_child_access(&policy);
    }
    if stage == 1 {
        provider
            .armed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let result = runtime
        .upgrade()
        .unwrap()
        .spawn_child_thread(
            StartThreadOptions {
                session_source: Some(source),
                ..StartThreadOptions::new(prepared)
            },
            runtime.control(fixture.parent.session.session_id()),
            Some(fixture.parent.session.thread_id),
            /*inherited_exec_policy*/ None,
            permit,
        )
        .await;
    assert_eq!(
        provider
            .revocations
            .load(std::sync::atomic::Ordering::SeqCst),
        usize::from(stage == 1),
        "the real manager inherited-instruction await must be crossed in the delayed case"
    );
    if stage == 2 {
        let child = result.expect("unchanged live managed policy remains authorized");
        assert_eq!(fixture.manager.list_thread_ids().await.len(), 2);
        assert_eq!(
            child.thread.session.session_id(),
            fixture.parent.session.session_id()
        );
        child.thread.shutdown_and_wait().await.unwrap();
    } else {
        let error = result
            .err()
            .expect("post-mint revoked policy must not start a child");
        assert!(
            error
                .to_string()
                .contains("revoked before startup admission"),
            "{error}"
        );
        assert_eq!(
            read_marker(),
            helper_before,
            "revoked startup must not invoke its auth helper"
        );
        assert_eq!(
            server
                .received_requests()
                .await
                .expect("request recording is enabled")
                .len(),
            requests_before,
            "revoked startup must not make a provider request"
        );
        assert_eq!(
            fixture.manager.list_thread_ids().await,
            vec![fixture.parent.session.thread_id]
        );
    }
}

#[test_case::test_case(true; "revoked during resumed manager instruction await")]
#[test_case::test_case(false; "unchanged managed resume succeeds")]
#[tokio::test]
async fn native_managed_resume_keeps_live_binding_through_final_admission(revoke: bool) {
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    let provider = Arc::new(RevokeDuringInheritedInstructions {
        policy: policy.clone(),
        sharing: std::sync::atomic::AtomicBool::new(false),
        armed: std::sync::atomic::AtomicBool::new(false),
        revocations: std::sync::atomic::AtomicUsize::new(0),
    });
    let fixture = Fixture::with_admission_setup(
        /*fast_mode*/ None,
        snapshot.binding,
        /*guardian_threshold*/ None,
        Some(provider.clone()),
        /*child_base_url*/ None,
    )
    .await;
    provider
        .sharing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let config = fixture
        .prepared(
            &fixture.resolved("zai").await,
            /*role*/ None,
            /*effort*/ None,
        )
        .await
        .unwrap()
        .config;
    let child = fixture.start_native_child(config).await;
    child.ensure_rollout_materialized().await;
    child.session.flush_rollout().await.unwrap();
    child.shutdown_and_wait().await.unwrap();
    fixture
        .manager
        .remove_thread(&child.session.thread_id)
        .await
        .unwrap();
    provider
        .armed
        .store(revoke, std::sync::atomic::Ordering::SeqCst);
    let runtime = &fixture.parent.session.services.local_agent_runtime;
    let result = runtime
        .control(fixture.parent.session.session_id())
        .resume_agent_from_rollout(
            fixture.config.clone(),
            child.session.thread_id,
            child.session_source.clone(),
        )
        .await;
    assert_eq!(
        provider
            .revocations
            .load(std::sync::atomic::Ordering::SeqCst),
        usize::from(revoke),
        "the real resumed manager instruction await must execute before interpreting its result"
    );
    if revoke {
        let error = match result {
            Ok(_) => panic!("revoked resumed startup must not publish the target"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("revoked before startup admission"),
            "{error}"
        );
        assert_eq!(
            fixture.manager.list_thread_ids().await,
            vec![fixture.parent.session.thread_id]
        );
    } else {
        assert_eq!(result.unwrap(), child.session.thread_id);
        let restored = fixture
            .manager
            .get_thread(child.session.thread_id)
            .await
            .unwrap();
        assert_eq!(restored.thread_settings_snapshot().await.model, "shared");
        assert_eq!(
            restored.session.session_id(),
            fixture.parent.session.session_id()
        );
        restored.shutdown_and_wait().await.unwrap();
    }
}

#[tokio::test]
async fn managed_bundle_revocation_invalidates_retained_preparation_before_admission() {
    let policy = codex_config::CloudConfigBundlePolicy::default();
    let mut snapshot = codex_config::CloudConfigBundleSnapshot {
        bundle: Ok(None),
        binding: None,
        config_server: None,
    };
    policy.publish_snapshot(&mut snapshot);
    let fixture = Fixture::with_binding(/*fast_mode*/ None, snapshot.binding).await;
    let resolved = fixture.resolved("zai").await;
    let mut prepared = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await
        .unwrap()
        .config;
    let source = prepared_source(&fixture.parent, &prepared);
    let mut revoked = codex_config::CloudConfigBundle::default();
    revoked
        .config_toml
        .enterprise_managed
        .push(codex_config::CloudConfigFragment {
            id: "child-provider-revocation".into(),
            name: "revoked child access".into(),
            contents: "[agents]\nallowed_model_providers=[]".into(),
        });
    policy.observe_remote_bundle(&revoked).unwrap();
    assert!(current_policy(&prepared).is_err());
    let result = prepared
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            &fixture.parent.session.services.local_agent_runtime,
            fixture.parent.session.thread_id,
            &prepared,
            /*full_history*/ false,
            &source,
        )
        .await;
    assert!(result.is_err_and(|error| {
        error
            .to_string()
            .contains("current child policy cannot be established")
    }));
    assert_eq!(fixture.manager.list_thread_ids().await.len(), 1);
}

#[tokio::test]
async fn controller_rejects_transplanted_mutated_and_relabelled_prepared_launches() {
    let fixture = Fixture::new().await;
    let other = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let prepared = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await
        .unwrap()
        .config;
    let caller = fixture.parent.session.thread_id;
    for mutation in 0..6 {
        let mut config = prepared.clone();
        let mut source = prepared_source(&fixture.parent, &config);
        let mut options = crate::agent::types::SpawnAgentOptions {
            parent_thread_id: Some(caller),
            ..Default::default()
        };
        let mut request_caller = caller;
        match mutation {
            0 => config.model_provider.base_url = Some("http://127.0.0.1:9/forged".into()),
            1 => config.cwd = fixture._host.path().to_path_buf().try_into().unwrap(),
            2 => {
                source = codex_protocol::protocol::SessionSource::Internal(
                    codex_protocol::protocol::InternalSessionSource::Guardian,
                );
            }
            3 => options.fork_mode = Some(crate::agent::types::SpawnAgentForkMode::FullHistory),
            4 => request_caller = other.parent.session.thread_id,
            _ => {
                source = codex_protocol::protocol::SessionSource::SubAgent(
                    codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                        parent_thread_id: caller,
                        depth: 3,
                        agent_path: None,
                        agent_nickname: None,
                        agent_role: None,
                    },
                );
            }
        }
        let result = fixture
            .parent
            .session
            .services
            .agent_control
            .spawn(crate::agent::api::SpawnRequest {
                caller: request_caller,
                config,
                input: crate::agent::api::AgentInput::UserInput(vec![
                    codex_protocol::user_input::UserInput::Text {
                        text: "forged prepared child".into(),
                        text_elements: vec![],
                    },
                ]),
                source,
                options,
            })
            .await;
        assert!(
            result.is_err_and(|error| error.to_string().contains("child_provider_policy_conflict"))
        );
        assert_eq!(fixture.manager.list_thread_ids().await.len(), 1);
    }
    // All negative branches return before the shared single-use nonce is spent.
    // A real registered parent can still consume its original proof.
    let mut valid = prepared;
    let source = prepared_source(&fixture.parent, &valid);
    let permit = valid
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            &fixture.parent.session.services.local_agent_runtime,
            caller,
            &valid,
            /*full_history*/ false,
            &source,
        )
        .await
        .unwrap();
    drop(permit);
}

#[tokio::test]
async fn prepared_launch_rejects_changed_executor_authority_with_unchanged_display_roots() {
    let fixture = Fixture::new().await;
    let resolved = fixture.resolved("zai").await;
    let mut prepared = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await
        .unwrap()
        .config;
    let source = prepared_source(&fixture.parent, &prepared);
    let before = fixture.parent.thread_settings_snapshot().await;
    let environments = fixture
        .parent
        .session
        .inherited_environment_snapshot()
        .await;
    let ready = environments.primary().expect("fixture local executor");
    let mut owner = ready.config().clone();
    owner.allow_login_shell = !owner.allow_login_shell;
    let mut selection = ready.selection();
    selection.config = codex_protocol::protocol::EnvironmentConfigState::Ready(owner);
    fixture
        .parent
        .update_thread_settings(codex_protocol::protocol::ThreadSettingsOverrides {
            environments: Some(codex_protocol::protocol::TurnEnvironmentRequests::new(
                before.cwd.clone(),
                vec![selection.into_request()],
            )),
            ..Default::default()
        })
        .await
        .unwrap();
    let _current_turn = fixture
        .parent
        .session
        .new_turn_with_default_settings("authority-runtime-update".into(), Default::default())
        .await;
    let updated = fixture
        .parent
        .session
        .inherited_environment_snapshot()
        .await;
    assert_ne!(
        ready.config().allow_login_shell,
        updated.primary().unwrap().config().allow_login_shell
    );
    let after = fixture.parent.thread_settings_snapshot().await;
    assert_eq!(before.cwd, after.cwd);
    assert_eq!(
        before.runtime_workspace_roots,
        after.runtime_workspace_roots
    );
    let result = prepared
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            &fixture.parent.session.services.local_agent_runtime,
            fixture.parent.session.thread_id,
            &prepared,
            /*full_history*/ false,
            &source,
        )
        .await;
    let error = result
        .err()
        .expect("changed executor authority must revoke prepared proof");
    assert!(
        error.to_string().contains("environment authority changed"),
        "{error}"
    );
    assert_eq!(fixture.manager.list_thread_ids().await.len(), 1);
}

#[tokio::test]
async fn private_native_permit_prevents_forged_controller_root_identity_from_entering_child_metadata()
 {
    let fixture = Fixture::new().await;
    let config = fixture
        .prepared(
            &fixture.resolved("zai").await,
            /*role*/ None,
            /*effort*/ None,
        )
        .await
        .unwrap()
        .config;
    let source = prepared_source(&fixture.parent, &config);
    let mut config = config;
    let permit = config
        .prepared_child_authorization
        .take()
        .unwrap()
        .consume(
            &fixture.parent.session.services.local_agent_runtime,
            fixture.parent.session.thread_id,
            &config,
            /*full_history*/ false,
            &source,
        )
        .await
        .unwrap();
    let forged = codex_protocol::SessionId::new();
    assert_ne!(forged, fixture.parent.session.session_id());
    let runtime = &fixture.parent.session.services.local_agent_runtime;
    let child = runtime
        .upgrade()
        .unwrap()
        .spawn_child_thread(
            StartThreadOptions {
                session_source: Some(source),
                ..StartThreadOptions::new(config)
            },
            runtime.control(forged),
            Some(fixture.parent.session.thread_id),
            /*inherited_exec_policy*/ None,
            permit,
        )
        .await
        .unwrap();
    assert_eq!(
        child.thread.session.session_id(),
        fixture.parent.session.session_id()
    );
    assert_ne!(child.thread.session.session_id(), forged);
    child.thread.ensure_rollout_materialized().await;
    child.thread.session.flush_rollout().await.unwrap();
    let meta = codex_rollout::read_session_meta_line(&child.thread.rollout_path().unwrap())
        .await
        .unwrap()
        .meta;
    assert_eq!(meta.session_id, fixture.parent.session.session_id());
    assert_eq!(
        meta.parent_thread_id,
        Some(fixture.parent.session.thread_id)
    );
}

#[tokio::test]
async fn sqlite_less_warm_reload_uses_only_the_actual_retained_target_model() {
    let fixture = Fixture::new().await;
    let config = fixture
        .prepared(
            &fixture.resolved("zai").await,
            /*role*/ None,
            /*effort*/ None,
        )
        .await
        .unwrap()
        .config;
    let child = fixture.start_native_child(config).await;
    child.ensure_rollout_materialized().await;
    child.session.flush_rollout().await.unwrap();
    let stored = child
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .unwrap();
    assert!(
        stored.model.is_none(),
        "SQLite-less legacy metadata does not claim a selected model"
    );
    assert_eq!(child.thread_settings_snapshot().await.model, "shared");
    child.shutdown_and_wait().await.unwrap();
    fixture
        .manager
        .remove_thread(&child.session.thread_id)
        .await
        .unwrap();
    let runtime = &fixture.parent.session.services.local_agent_runtime;
    let turn = fixture
        .parent
        .session
        .new_turn_with_default_settings("warm-model-proof".into(), Default::default())
        .await;
    let mut candidate = build_agent_resume_config(&turn).unwrap();
    candidate.model_provider_id = stored.model_provider;
    candidate.model = Some("forged-saved-model".into());
    let history = codex_history::InitialHistory::Resumed(codex_history::ResumedHistory {
        history_revision: None,
        conversation_id: child.session.thread_id,
        history: Arc::new(stored.history.unwrap().items),
        rollout_path: stored.rollout_path,
    });
    let epoch = crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
        &fixture.parent.session,
        &fixture.parent.session_source,
    )
    .await;
    let mut selections = None;
    let rejected = crate::agent::child_provider_authorization::ChildResumePermit::prepare(
        runtime,
        fixture.parent.clone(),
        epoch,
        &mut candidate,
        &history,
        &child.session_source,
        Some(fixture.parent.session.thread_id),
        &stored.permission_profile,
        stored.approval_mode,
        MultiAgentVersion::V1,
        &mut selections,
        /*inherited*/ None,
    )
    .await;
    let error = rejected
        .err()
        .expect("raw saved model cannot replace actual target selection");
    assert!(
        error
            .to_string()
            .contains("saved model differs from its retained actual selection"),
        "{error}"
    );
    assert_eq!(
        fixture.manager.list_thread_ids().await,
        vec![fixture.parent.session.thread_id]
    );
    runtime
        .control(fixture.parent.session.session_id())
        .resume_agent_from_rollout(
            fixture.config.clone(),
            child.session.thread_id,
            child.session_source.clone(),
        )
        .await
        .expect("real same-host target model remains reloadable without SQLite");
    let resumed = fixture
        .manager
        .get_thread(child.session.thread_id)
        .await
        .unwrap();
    assert_eq!(resumed.thread_settings_snapshot().await.model, "shared");
    assert_eq!(
        resumed.thread_settings_snapshot().await.model_provider_id,
        "zai"
    );
    let root_step = StepContext::for_test(fixture.parent.session.new_default_turn().await);
    fixture
        .parent
        .session
        .record_context_updates_and_set_reference_context_item(&root_step)
        .await
        .unwrap();
    fixture.parent.ensure_rollout_materialized().await;
    fixture.parent.session.flush_rollout().await.unwrap();
    let root_context = runtime
        .upgrade()
        .unwrap()
        .load_latest_model_context(codex_thread_store::LoadThreadHistoryParams {
            thread_id: fixture.parent.session.thread_id,
            include_archived: true,
        })
        .await
        .unwrap();
    let root_path = fixture.parent.rollout_path();
    fixture
        .manager
        .shutdown_all_threads_bounded(std::time::Duration::from_secs(5))
        .await;
    let cold_manager = crate::ThreadManager::with_models_provider_and_home_for_tests(
        codex_login::CodexAuth::from_api_key("parent-only"),
        fixture.config.model_provider.clone(),
        fixture._host.path().to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let cold_root = cold_manager
        .start_thread(StartThreadOptions {
            initial_history: codex_history::InitialHistory::Resumed(
                codex_history::ResumedHistory {
                    conversation_id: fixture.parent.session.thread_id,
                    history: Arc::new(root_context.items),
                    history_revision: root_context.revision,
                    rollout_path: root_path,
                },
            ),
            ..StartThreadOptions::new(fixture.config.clone())
        })
        .await
        .unwrap()
        .thread;
    assert_eq!(
        cold_root.session.session_id(),
        fixture.parent.session.session_id()
    );
    let cold_runtime = &cold_root.session.services.local_agent_runtime;
    let cold_turn = cold_root
        .session
        .new_turn_with_default_settings("cold-model-no-proof".into(), Default::default())
        .await;
    let mut cold_candidate = build_agent_resume_config(&cold_turn).unwrap();
    cold_candidate.model_provider_id = "zai".into();
    cold_candidate.model = Some("forged-saved-model".into());
    let cold_epoch = crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
        &cold_root.session,
        &cold_root.session_source,
    )
    .await;
    let mut selections = None;
    let rejected = crate::agent::child_provider_authorization::ChildResumePermit::prepare(
        cold_runtime,
        cold_root.clone(),
        cold_epoch,
        &mut cold_candidate,
        &history,
        &child.session_source,
        Some(cold_root.session.thread_id),
        &stored.permission_profile,
        stored.approval_mode,
        MultiAgentVersion::V1,
        &mut selections,
        /*inherited*/ None,
    )
    .await;
    let error = rejected
        .err()
        .expect("raw model with no same-host ceiling cannot create cold authority");
    assert!(error.to_string().contains("authority"), "{error}");
    assert_eq!(
        cold_manager.list_thread_ids().await,
        vec![cold_root.session.thread_id]
    );
}

#[tokio::test]
async fn warm_saved_child_cannot_gain_changed_catalog_tool_authority() {
    let fixture = Fixture::new().await;
    let catalog_path = fixture._host.path().join("zai.json");
    let mut original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&catalog_path).unwrap()).unwrap();
    original["models"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|model| model["slug"] == "shared")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("node_repl_disabled".into(), serde_json::Value::Bool(true));
    std::fs::write(&catalog_path, serde_json::to_vec(&original).unwrap()).unwrap();
    let resolved = fixture.resolved("zai").await;
    let config = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await
        .unwrap()
        .config;
    let child = fixture.start_native_child(config).await;
    child.inject_response_items(vec![serde_json::from_value(serde_json::json!({
        "type":"message","role":"user","content":[{"type":"input_text","text":"persist warm authority"}]
    })).unwrap()]).await.unwrap();
    child.ensure_rollout_materialized().await;
    child.session.flush_rollout().await.unwrap();
    child.shutdown_and_wait().await.unwrap();
    fixture
        .manager
        .remove_thread(&child.session.thread_id)
        .await
        .unwrap();
    let path = fixture._host.path().join("zai.json");
    let mut catalog: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let model = catalog["models"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|model| model["slug"] == "shared")
        .unwrap()
        .as_object_mut()
        .unwrap();
    assert_eq!(
        model
            .get("node_repl_disabled")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    // Removing an actual old model restriction widens the proposed runtime.
    model.insert("node_repl_disabled".into(), serde_json::Value::Bool(false));
    std::fs::write(path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    let runtime = &fixture.parent.session.services.local_agent_runtime;
    let error = match runtime
        .control(fixture.parent.session.session_id())
        .resume_agent_from_rollout(
            fixture.config.clone(),
            child.session.thread_id,
            child.session_source.clone(),
        )
        .await
    {
        Ok(_) => panic!("changed selected model grants must not bypass the warm ceiling"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("selected model tool authority changed"),
        "{error}"
    );
    assert_eq!(
        fixture.manager.list_thread_ids().await,
        vec![fixture.parent.session.thread_id]
    );
}

#[tokio::test]
async fn warm_saved_child_cannot_weaken_its_local_guardian_threshold() {
    let fixture = Fixture::with_binding_and_guardian(
        /*fast_mode*/ None,
        /*binding*/ None,
        Some(0.25),
    )
    .await;
    let resolved = fixture.resolved("zai").await;
    let config = fixture
        .prepared(&resolved, /*role*/ None, /*effort*/ None)
        .await
        .unwrap()
        .config;
    let child = fixture.start_native_child(config).await;
    child.inject_response_items(vec![serde_json::from_value(serde_json::json!({
        "type":"message","role":"user","content":[{"type":"input_text","text":"persist typed Guardian ceiling"}]
    })).unwrap()]).await.unwrap();
    let actual = child.session.get_config().await;
    assert!(actual.features.enabled(Feature::GuardianV2));
    assert_eq!(
        actual.config_layer_stack.effective_config()["features"]["guardianv2"]["review_threshold"]
            .as_float(),
        Some(0.25)
    );
    child.ensure_rollout_materialized().await;
    child.session.flush_rollout().await.unwrap();
    child.shutdown_and_wait().await.unwrap();
    fixture
        .manager
        .remove_thread(&child.session.thread_id)
        .await
        .unwrap();
    let host_path = fixture._host.path().join("config.toml");
    let contents = std::fs::read_to_string(&host_path)
        .unwrap()
        .replace("review_threshold=0.25", "review_threshold=0.75");
    std::fs::write(&host_path, contents).unwrap();
    let next = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(fixture._host.path().to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(fixture._project.path().to_path_buf()),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let current = fixture.parent.session.get_config().await;
    assert!(matches!(
        fixture
            .parent
            .session
            .refresh_runtime_config(current, next)
            .await,
        crate::ConfigRefreshOutcome::Published
    ));
    assert_eq!(
        fixture
            .parent
            .session
            .get_config()
            .await
            .config_layer_stack
            .effective_config()["features"]["guardianv2"]["review_threshold"]
            .as_float(),
        Some(0.75)
    );
    assert!(
        fixture
            .parent
            .session
            .get_config()
            .await
            .features
            .enabled(Feature::GuardianV2)
    );
    let runtime = &fixture.parent.session.services.local_agent_runtime;
    let error = match runtime
        .control(fixture.parent.session.session_id())
        .resume_agent_from_rollout(
            fixture.config.clone(),
            child.session.thread_id,
            child.session_source.clone(),
        )
        .await
    {
        Ok(_) => panic!("warm reload must retain actual local Guardian safety ceiling"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("selected model tool authority changed"),
        "{error}"
    );
    assert_eq!(
        fixture.manager.list_thread_ids().await,
        vec![fixture.parent.session.thread_id]
    );
}

#[tokio::test]
async fn warm_saved_metadata_cannot_transplant_a_child_between_real_roots_in_one_runtime() {
    let fixture = Fixture::new().await;
    let config = fixture
        .prepared(
            &fixture.resolved("zai").await,
            /*role*/ None,
            /*effort*/ None,
        )
        .await
        .unwrap()
        .config;
    let child = fixture.start_native_child(config).await;
    child.ensure_rollout_materialized().await;
    child.session.flush_rollout().await.unwrap();
    let saved = child
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .unwrap();
    child.shutdown_and_wait().await.unwrap();
    fixture
        .manager
        .remove_thread(&child.session.thread_id)
        .await
        .unwrap();
    let runtime = &fixture.parent.session.services.local_agent_runtime;
    let other = fixture
        .manager
        .start_thread_with_control_for_tests(
            StartThreadOptions::new(fixture.config.clone()),
            runtime.control(fixture.parent.session.session_id()),
        )
        .await
        .unwrap();
    assert_ne!(
        other.thread.session.session_id(),
        fixture.parent.session.session_id()
    );
    let mut items = saved.history.unwrap().items;
    for item in &mut items {
        if let codex_history::RolloutItem::SessionMeta(meta) = item {
            meta.meta.session_id = other.thread.session.session_id();
        }
    }
    let history = codex_history::InitialHistory::Resumed(codex_history::ResumedHistory {
        history_revision: None,
        conversation_id: child.session.thread_id,
        history: Arc::new(items),
        rollout_path: saved.rollout_path,
    });
    let turn = other.thread.session.new_default_turn().await;
    let mut candidate = build_agent_resume_config(&turn).unwrap();
    candidate.model_provider_id = saved.model_provider;
    candidate.model = saved.model;
    candidate.model_reasoning_effort = saved.reasoning_effort;
    let caller = crate::agent::child_provider_authorization::RetainedAgentAuthority::capture(
        &other.thread.session,
        &other.thread.session_source,
    )
    .await;
    let error = crate::agent::child_provider_authorization::ChildResumePermit::prepare(
        runtime,
        Arc::clone(&other.thread),
        caller,
        &mut candidate,
        &history,
        &child.session_source,
        Some(fixture.parent.session.thread_id),
        &saved.permission_profile,
        saved.approval_mode,
        codex_protocol::protocol::MultiAgentVersion::V1,
        &mut None,
        /*inherited*/ None,
    )
    .await
    .err()
    .expect("mutable saved metadata cannot replace actual host root ownership");
    assert!(
        error.to_string().contains("another actual host root"),
        "{error}"
    );
    assert!(
        fixture
            .manager
            .get_thread(child.session.thread_id)
            .await
            .is_err()
    );
    assert_eq!(fixture.manager.list_thread_ids().await.len(), 2);
}
