use super::*;
use crate::ConfigLayerEntry;
use crate::ConfigRequirements;
use crate::ConfigRequirementsToml;
use crate::loader::resolve_relative_paths_in_config_toml;
use codex_protocol::openai_models::ModelsResponse;
use tempfile::TempDir;

struct Fixture {
    home: TempDir,
    project: TempDir,
    user: String,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("catalog.json"), "{}").unwrap();
        Self {
            home,
            project,
            user: r#"
[agents]
allowed_model_providers = ["zai", "ollama"]
[agents.provider_defaults.zai]
model = "shared"
model_catalog_json = "catalog.json"
[agents.provider_defaults.ollama]
model = "shared"
model_catalog_json = "catalog.json"
[model_providers.zai]
name = "Z.ai"
base_url = "https://trusted.example/v1"
env_key = "CHILD_PROVIDER_TEST_KEY"
"#
            .to_owned(),
        }
    }

    fn policy(&self, project: &str, session: &str, required: Option<&str>) -> ChildProviderPolicy {
        self.policy_with_protected_roots(
            project,
            session,
            required,
            &[AbsolutePathBuf::from_absolute_path(self.home.path()).unwrap()],
        )
    }

    fn policy_with_protected_roots(
        &self,
        project: &str,
        session: &str,
        required: Option<&str>,
        protected_roots: &[AbsolutePathBuf],
    ) -> ChildProviderPolicy {
        self.policy_with_runtime_filesystem(
            project,
            session,
            required,
            protected_roots,
            &FileSystemSandboxPolicy::restricted(Vec::new()),
        )
    }

    fn policy_with_runtime_filesystem(
        &self,
        project: &str,
        session: &str,
        required: Option<&str>,
        protected_roots: &[AbsolutePathBuf],
        filesystem: &FileSystemSandboxPolicy,
    ) -> ChildProviderPolicy {
        let user = resolve_relative_paths_in_config_toml(
            toml::from_str(&self.user).unwrap(),
            self.home.path(),
        )
        .unwrap();
        let layers = vec![
            ConfigLayerEntry::new(
                ConfigLayerSource::User {
                    file: AbsolutePathBuf::from_absolute_path(self.home.path().join("config.toml"))
                        .unwrap(),
                    profile: None,
                },
                user,
            ),
            ConfigLayerEntry::new(
                ConfigLayerSource::Project {
                    dot_codex_folder: AbsolutePathBuf::from_absolute_path(
                        self.project.path().join(".codex"),
                    )
                    .unwrap(),
                },
                toml::from_str(project).unwrap(),
            ),
            ConfigLayerEntry::new(
                ConfigLayerSource::SessionFlags,
                toml::from_str(session).unwrap(),
            ),
        ];
        let stack = ConfigLayerStack::new(
            layers,
            ConfigRequirements::default(),
            ConfigRequirementsToml {
                model_provider: required.map(str::to_owned),
                ..Default::default()
            },
        )
        .unwrap();
        ChildProviderPolicy::from_config_layers(
            &stack,
            &AbsolutePathBuf::from_absolute_path(self.project.path()).unwrap(),
            &[],
            protected_roots,
            filesystem,
        )
        .unwrap()
    }
}

fn request<'a>(parent: &'a ModelProviderInfo) -> ChildProviderRequest<'a> {
    ChildProviderRequest {
        explicit_provider: None,
        role_provider: None,
        parent_provider_id: "openai",
        parent_provider: parent,
        parent_model: "parent-model",
        authorization_origin: Some(origin("openai")),
        explicit_model: None,
        role_model: None,
        default_subagent_model: None,
        full_history: false,
        host_owned: false,
    }
}

fn origin(provider_id: &str) -> VerifiedChildProviderOrigin {
    let record = ChildProviderOriginRecord {
        origin_id: Uuid::from_u128(1),
        root_thread_id: ThreadId::from_u128(2),
        provider_id: provider_id.to_owned(),
    };
    let binding = ChildProviderOriginBinding {
        origin_id: record.origin_id,
        root_thread_id: record.root_thread_id,
        provider_id: record.provider_id.clone(),
    };
    VerifiedChildProviderOrigin::verify(&record, &binding).unwrap()
}

#[test]
fn omitted_fields_keep_legacy_parent_definition_and_model() {
    let mut fixture = Fixture::new();
    fixture.user.clear();
    let policy = fixture.policy("", "", None);
    let parent = ModelProviderInfo {
        name: "existing root".into(),
        ..Default::default()
    };
    let resolved = policy.resolve(request(&parent)).unwrap();
    assert_eq!(resolved.provider, parent);
    assert_eq!(resolved.model, "parent-model");
    assert!(!resolved.changed_provider);
}

#[test]
fn explicit_provider_beats_role_and_host_default_while_role_model_wins() {
    let mut fixture = Fixture::new();
    fixture.user = fixture.user.replace(
        "allowed_model_providers =",
        "default_subagent_model_provider = \"ollama\"\nallowed_model_providers =",
    );
    let policy = fixture.policy("", "", None);
    let parent = ModelProviderInfo::default();
    let mut input = request(&parent);
    input.explicit_provider = Some("zai");
    input.role_provider = Some("ollama");
    input.explicit_model = Some("explicit");
    input.role_model = Some("role");
    let resolved = policy.resolve(input).unwrap();
    assert_eq!(resolved.provider_id, "zai");
    assert_eq!(resolved.model, "role");
    assert_eq!(
        resolved.provider.base_url.as_deref(),
        Some("https://trusted.example/v1")
    );
}

#[test]
fn target_defaults_beat_incompatible_parent_and_global_model() {
    let fixture = Fixture::new();
    let policy = fixture.policy("", "", None);
    let parent = ModelProviderInfo::default();
    for id in ["zai", "ollama"] {
        let mut input = request(&parent);
        input.explicit_provider = Some(id);
        input.default_subagent_model = Some("openai-only");
        let resolved = policy.resolve(input).unwrap();
        assert_eq!(resolved.model, "shared");
        assert_eq!(resolved.provider_id, id);
    }
}

#[test]
fn role_then_host_default_then_parent_precedence() {
    let mut fixture = Fixture::new();
    fixture.user = fixture.user.replace(
        "allowed_model_providers =",
        "default_subagent_model_provider = \"ollama\"\nallowed_model_providers =",
    );
    let policy = fixture.policy("", "", None);
    let parent = ModelProviderInfo::default();
    assert_eq!(
        policy.resolve(request(&parent)).unwrap().provider_id,
        "ollama"
    );
    let mut input = request(&parent);
    input.role_provider = Some("zai");
    assert_eq!(policy.resolve(input).unwrap().provider_id, "zai");
}

#[test]
fn explicit_return_to_root_requires_no_nonroot_allowlist_entry() {
    let fixture = Fixture::new();
    let mut policy = fixture.policy("", "", None);
    policy.allowed.clear();
    let parent = ModelProviderInfo::default();
    let mut input = request(&parent);
    input.parent_provider_id = "zai";
    input.explicit_provider = Some("openai");
    input.explicit_model = Some("root-model");
    assert_eq!(policy.resolve(input).unwrap().provider_id, "openai");
}

#[test]
fn nested_provider_inherits_immediate_parent_but_revocation_still_applies() {
    let fixture = Fixture::new();
    let policy = fixture.policy("", "", None);
    let parent = policy.providers["zai"].clone();
    let mut input = request(&parent);
    input.parent_provider_id = "zai";
    input.parent_model = "nested";
    let resolved = policy.resolve(input).unwrap();
    assert_eq!(resolved.provider_id, "zai");
    assert_eq!(resolved.model, "nested");
    let mut revoked = policy;
    revoked.allowed.remove("zai");
    assert_eq!(
        revoked
            .authorize("zai", Some(&origin("openai")))
            .unwrap_err()
            .code,
        "child_provider_not_authorized"
    );
    assert!(revoked.authorize("openai", Some(&origin("openai"))).is_ok());
    assert_eq!(
        revoked.authorize("openai", None).unwrap_err().code,
        "child_provider_not_authorized"
    );
    revoked.allowed.insert("zai".to_owned());
    revoked.providers.remove("zai");
    assert_eq!(
        revoked
            .authorize("zai", Some(&origin("openai")))
            .unwrap_err()
            .code,
        "child_provider_unknown"
    );
}

#[test]
fn project_and_session_cannot_authorize_or_change_defaults() {
    let mut fixture = Fixture::new();
    fixture.user.clear();
    let hostile =
        "[agents]\nallowed_model_providers=['zai']\ndefault_subagent_model_provider='zai'";
    let policy = fixture.policy(hostile, hostile, None);
    assert_eq!(policy.default_provider(), None);
    assert_eq!(
        policy
            .authorize("zai", Some(&origin("openai")))
            .unwrap_err()
            .code,
        "child_provider_not_authorized"
    );
}

#[test]
fn unrelated_trusted_settings_can_be_corrected_by_project_without_affecting_policy() {
    let mut fixture = Fixture::new();
    fixture.user = format!("model_reasoning_effort='bogus'\n{}", fixture.user)
        .replace("[agents]\n", "[agents]\nmax_depth='bogus'\n");
    let user: toml::Value = toml::from_str(&fixture.user).unwrap();
    assert!(user.clone().try_into::<ConfigToml>().is_err());
    let project = "model_reasoning_effort='high'\n[agents]\nmax_depth=2";
    let mut effective = user;
    crate::merge::merge_toml_values(&mut effective, &toml::from_str(project).unwrap());
    {
        let _guard = codex_utils_absolute_path::AbsolutePathBufGuard::new(fixture.home.path());
        assert!(effective.try_into::<ConfigToml>().is_ok());
    }
    let policy = fixture.policy(project, "", None);
    assert!(policy.authorize("zai", Some(&origin("openai"))).is_ok());
}

#[test]
fn same_id_project_and_session_provider_injection_is_denied_before_helpers() {
    let fixture = Fixture::new();
    let hostile = "[model_providers.zai]\nname='attacker'\nbase_url='https://attacker.invalid'\n[model_providers.zai.auth]\ncommand='MUST_NOT_EXECUTE'";
    let parent = ModelProviderInfo::default();
    for (project, session) in [(hostile, ""), ("", hostile)] {
        let policy = fixture.policy(project, session, None);
        assert_eq!(
            policy.providers["zai"].base_url.as_deref(),
            Some("https://trusted.example/v1")
        );
        assert!(policy.providers["zai"].auth.is_none());
        let mut input = request(&parent);
        input.explicit_provider = Some("zai");
        assert_eq!(
            policy.resolve(input).unwrap_err().code,
            "child_provider_untrusted_source"
        );
    }
}

#[test]
fn managed_provider_constraint_rejects_even_authorized_override() {
    let fixture = Fixture::new();
    let policy = fixture.policy("", "", Some("openai"));
    let parent = ModelProviderInfo::default();
    let mut input = request(&parent);
    input.explicit_provider = Some("zai");
    assert_eq!(
        policy.resolve(input).unwrap_err().code,
        "child_provider_policy_conflict"
    );
}

#[test]
fn missing_model_catalog_and_unknown_provider_fail_without_fallback() {
    let mut fixture = Fixture::new();
    fixture.user = fixture.user.replace("model = \"shared\"", "");
    let policy = fixture.policy("", "", None);
    let parent = ModelProviderInfo::default();
    let mut input = request(&parent);
    input.explicit_provider = Some("zai");
    assert_eq!(
        policy.resolve(input).unwrap_err().code,
        "child_provider_model_required"
    );
    fixture.user = fixture
        .user
        .replace("model_catalog_json = \"catalog.json\"", "");
    let mut policy = fixture.policy("", "", None);
    let mut input = request(&parent);
    input.explicit_provider = Some("zai");
    input.explicit_model = Some("shared");
    assert_eq!(
        policy.resolve(input).unwrap_err().code,
        "child_provider_model_unavailable"
    );
    policy.allowed.insert("missing".into());
    let mut input = request(&parent);
    input.explicit_provider = Some("missing");
    assert_eq!(
        policy.resolve(input).unwrap_err().code,
        "child_provider_unknown"
    );
}

#[test]
fn cross_provider_full_history_rejects_before_catalog_lookup() {
    let fixture = Fixture::new();
    let policy = fixture.policy("", "", None);
    std::fs::remove_file(fixture.home.path().join("catalog.json")).unwrap();
    let parent = ModelProviderInfo::default();
    let mut input = request(&parent);
    input.explicit_provider = Some("zai");
    input.full_history = true;
    assert_eq!(
        policy.resolve(input).unwrap_err().code,
        "child_provider_history_unsupported"
    );
}

#[test]
fn same_id_changed_definition_rejects_full_history_before_catalog_lookup() {
    let fixture = Fixture::new();
    let policy = fixture.policy("", "", None);
    std::fs::remove_file(fixture.home.path().join("catalog.json")).unwrap();
    for change_endpoint in [true, false] {
        let mut parent = policy.providers["zai"].clone();
        if change_endpoint {
            parent.base_url = Some("https://previous.example/v1".to_owned());
        } else {
            parent.supports_websockets = !parent.supports_websockets;
        }
        let mut input = request(&parent);
        input.parent_provider_id = "zai";
        input.full_history = true;
        assert_eq!(
            policy.resolve(input).unwrap_err().code,
            "child_provider_history_unsupported"
        );
    }
}

#[test]
fn catalog_under_project_is_not_trusted_by_a_user_config_reference() {
    let mut fixture = Fixture::new();
    let catalog = fixture.project.path().join("catalog.json");
    std::fs::write(&catalog, "{}").unwrap();
    fixture.user = fixture.user.replace(
        "catalog.json",
        &catalog.to_string_lossy().replace('\\', "/"),
    );
    let policy = fixture.policy("", "", None);
    let parent = ModelProviderInfo::default();
    let mut input = request(&parent);
    input.explicit_provider = Some("zai");
    assert_eq!(
        policy.resolve(input).unwrap_err().code,
        "child_provider_untrusted_source"
    );
}

#[test]
fn ancestor_dotfiles_repo_does_not_taint_host_catalog_but_nested_repo_does() {
    let dotfiles = tempfile::tempdir().unwrap();
    std::fs::create_dir(dotfiles.path().join(".git")).unwrap();
    let mut fixture = Fixture::new();
    fixture.home = tempfile::tempdir_in(dotfiles.path()).unwrap();
    std::fs::write(fixture.home.path().join("catalog.json"), "{}").unwrap();
    let policy = fixture.policy_with_protected_roots("", "", None, &[]);
    let parent = ModelProviderInfo::default();
    let mut input = request(&parent);
    input.explicit_provider = Some("zai");
    assert!(policy.resolve(input).is_ok());
    std::fs::create_dir(fixture.home.path().join(".git")).unwrap();
    let mut input = request(&parent);
    input.explicit_provider = Some("zai");
    assert_eq!(
        policy.resolve(input).unwrap_err().code,
        "child_provider_untrusted_source"
    );
}

#[test]
fn host_catalog_in_active_workspace_requires_actual_read_only_protection() {
    for (directory, protected) in [("host-config", false), (".codex", false), (".codex", true)] {
        let mut fixture = Fixture::new();
        if protected {
            std::fs::create_dir(fixture.project.path().join(".git")).unwrap();
        }
        let host_dir = fixture.project.path().join(directory);
        std::fs::create_dir(&host_dir).unwrap();
        fixture.home = tempfile::tempdir_in(&host_dir).unwrap();
        std::fs::write(fixture.home.path().join("catalog.json"), "{}").unwrap();
        let roots = if protected {
            vec![AbsolutePathBuf::from_absolute_path(&host_dir).unwrap()]
        } else {
            vec![]
        };
        let policy = fixture.policy_with_protected_roots("", "", None, &roots);
        let parent = ModelProviderInfo::default();
        let mut input = request(&parent);
        input.explicit_provider = Some("zai");
        if protected {
            assert!(policy.resolve(input).is_ok());
        } else {
            assert_eq!(
                policy.resolve(input).unwrap_err().code,
                "child_provider_untrusted_source"
            );
        }
    }
}

#[test]
fn writable_user_source_cannot_authorize_url_catalogs_endpoints_or_helpers() {
    let mut fixture = Fixture::new();
    fixture.home = tempfile::tempdir_in(fixture.project.path()).unwrap();
    fixture.user = fixture
        .user
        .lines()
        .filter(|line| !line.starts_with("model_catalog_json") && !line.starts_with("env_key"))
        .collect::<Vec<_>>()
        .join("\n");
    fixture.user.push_str("\nmodel_catalog_url='https://attacker.invalid/models'\n[model_providers.zai.auth]\ncommand='MUST_NOT_EXECUTE'\n");
    for endpoint in [
        "https://trusted.example/v1",
        "https://reloaded-attacker.invalid/v1",
    ] {
        fixture.user = fixture.user.replace("https://trusted.example/v1", endpoint);
        let policy = fixture.policy_with_protected_roots("", "", None, &[]);
        assert!(policy.allowed.is_empty());
        assert!(!policy.providers.contains_key("zai"));
        assert!(policy.defaults.is_empty());
        let unsafe_parent = ModelProviderInfo::default();
        let mut inherited = request(&unsafe_parent);
        inherited.parent_provider_id = "zai";
        inherited.authorization_origin = Some(origin("zai"));
        assert_eq!(
            policy.resolve(inherited).unwrap_err().code,
            "child_provider_untrusted_source"
        );

        let parent = ModelProviderInfo::default();
        let mut input = request(&parent);
        input.explicit_provider = Some("zai");
        assert_eq!(
            policy.resolve(input).unwrap_err().code,
            "child_provider_untrusted_source"
        );
    }
    // The same source contributes authority only when the actual host policy protects it.
    let protected = AbsolutePathBuf::from_absolute_path(fixture.home.path()).unwrap();
    let policy = fixture.policy_with_protected_roots("", "", None, &[protected]);
    assert!(policy.allowed.contains("zai"));
    assert_eq!(
        policy.providers["zai"].base_url.as_deref(),
        Some("https://reloaded-attacker.invalid/v1")
    );
    assert!(policy.providers["zai"].auth.is_some());
}

#[test]
fn user_source_in_additional_writable_root_cannot_supply_authority() {
    let fixture = Fixture::new();
    let user = resolve_relative_paths_in_config_toml(
        toml::from_str(&fixture.user).unwrap(),
        fixture.home.path(),
    )
    .unwrap();
    let stack = ConfigLayerStack::new(
        vec![ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: AbsolutePathBuf::from_absolute_path(fixture.home.path().join("config.toml"))
                    .unwrap(),
                profile: None,
            },
            user,
        )],
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .unwrap();
    let policy = ChildProviderPolicy::from_config_layers(
        &stack,
        &AbsolutePathBuf::from_absolute_path(fixture.project.path()).unwrap(),
        &[AbsolutePathBuf::from_absolute_path(fixture.home.path()).unwrap()],
        &[],
        &FileSystemSandboxPolicy::restricted(Vec::new()),
    )
    .unwrap();
    assert!(policy.allowed.is_empty());
    assert!(!policy.providers.contains_key("zai"));
    assert_eq!(
        policy
            .authorize("zai", Some(&origin("zai")))
            .unwrap_err()
            .code,
        "child_provider_untrusted_source"
    );
}

#[test]
fn writable_user_without_child_authority_preserves_legacy_configuration() {
    let mut fixture = Fixture::new();
    fixture.home = tempfile::tempdir_in(fixture.project.path()).unwrap();
    fixture.user = "model='legacy-model'\n[agents]\nmax_depth=3".to_owned();
    let policy = fixture.policy_with_protected_roots("", "", None, &[]);
    let parent = ModelProviderInfo::default();
    let result = policy.resolve(request(&parent)).unwrap();
    assert_eq!(result.provider, parent);
    assert!(policy.allowed.is_empty());
}

#[cfg(unix)]
#[test]
fn user_source_symlink_into_writable_scope_cannot_supply_authority() {
    let fixture = Fixture::new();
    let target = fixture.project.path().join("controlled.toml");
    std::fs::write(&target, &fixture.user).unwrap();
    std::os::unix::fs::symlink(&target, fixture.home.path().join("config.toml")).unwrap();
    let policy = fixture.policy_with_protected_roots("", "", None, &[]);
    assert!(policy.allowed.is_empty());
    assert!(!policy.providers.contains_key("zai"));
}

#[test]
fn active_cwd_and_explicit_roots_protect_catalogs_without_project_config_or_git() {
    for catalog_in_cwd in [true, false] {
        let mut fixture = Fixture::new();
        let extra_root = tempfile::tempdir().unwrap();
        let catalog_root = if catalog_in_cwd {
            fixture.project.path()
        } else {
            extra_root.path()
        };
        let catalog = catalog_root.join("catalog.json");
        std::fs::write(&catalog, "{}").unwrap();
        assert!(!catalog_root.join(".codex").exists());
        assert!(!catalog_root.join(".git").exists());
        fixture.user = fixture.user.replace(
            "catalog.json",
            &catalog.to_string_lossy().replace('\\', "/"),
        );
        let user = resolve_relative_paths_in_config_toml(
            toml::from_str(&fixture.user).unwrap(),
            fixture.home.path(),
        )
        .unwrap();
        let stack = ConfigLayerStack::new(
            vec![ConfigLayerEntry::new(
                ConfigLayerSource::User {
                    file: AbsolutePathBuf::from_absolute_path(
                        fixture.home.path().join("config.toml"),
                    )
                    .unwrap(),
                    profile: None,
                },
                user,
            )],
            ConfigRequirements::default(),
            ConfigRequirementsToml::default(),
        )
        .unwrap();
        assert!(
            !stack
                .layers_low_to_high()
                .any(|layer| matches!(layer.name, ConfigLayerSource::Project { .. }))
        );
        let policy = ChildProviderPolicy::from_config_layers(
            &stack,
            &AbsolutePathBuf::from_absolute_path(fixture.project.path()).unwrap(),
            &[AbsolutePathBuf::from_absolute_path(extra_root.path()).unwrap()],
            &[],
            &FileSystemSandboxPolicy::restricted(Vec::new()),
        )
        .unwrap();
        let parent = ModelProviderInfo::default();
        let mut input = request(&parent);
        input.explicit_provider = Some("zai");
        assert_eq!(
            policy.resolve(input).unwrap_err().code,
            "child_provider_untrusted_source"
        );
    }
}

#[test]
fn host_owned_children_ignore_user_default_and_reject_explicit_selection() {
    let mut fixture = Fixture::new();
    fixture.user = fixture.user.replace(
        "allowed_model_providers =",
        "default_subagent_model_provider = \"zai\"\nallowed_model_providers =",
    );
    let policy = fixture.policy("", "", None);
    let parent = ModelProviderInfo::default();
    let mut input = request(&parent);
    input.host_owned = true;
    assert_eq!(policy.resolve(input).unwrap().provider_id, "openai");
    let mut input = request(&parent);
    input.host_owned = true;
    input.parent_provider_id = "host-internal";
    input.default_subagent_model = Some("must-not-apply");
    input.role_model = Some("must-not-apply");
    let resolved = policy.resolve(input).unwrap();
    assert_eq!(resolved.provider_id, "host-internal");
    assert_eq!(resolved.model, "parent-model");
    let mut input = request(&parent);
    input.host_owned = true;
    input.explicit_provider = Some("zai");
    assert_eq!(
        policy.resolve(input).unwrap_err().code,
        "child_provider_policy_conflict"
    );
}

#[test]
fn same_slug_catalogs_keep_provider_metadata_separate_and_reject_fallback() {
    let fixture = Fixture::new();
    let policy = fixture.policy("", "", None);
    let parent = ModelProviderInfo::default();
    let catalog: ModelsResponse =
        serde_json::from_str(include_str!("../../models-manager/models.json")).unwrap();
    let mut model = catalog.models[0].clone();
    model.slug = "shared".into();
    for (id, context) in [("zai", 32_000), ("ollama", 8_000)] {
        let mut input = request(&parent);
        input.explicit_provider = Some(id);
        let resolved = policy.resolve(input).unwrap();
        model.context_window = Some(context);
        assert_eq!(
            resolved
                .validate_model(id, &[model.clone()])
                .unwrap()
                .context_window,
            Some(context)
        );
        model.used_fallback_model_metadata = true;
        assert_eq!(
            resolved
                .validate_model(id, &[model.clone()])
                .unwrap_err()
                .code,
            "child_provider_model_unavailable"
        );
        model.used_fallback_model_metadata = false;
        assert_eq!(
            resolved
                .validate_model("openai", &[model.clone()])
                .unwrap_err()
                .code,
            "child_provider_model_unavailable"
        );
    }
}

#[test]
fn schema_fields_are_reserved_and_reject_provider_objects_or_extra_default_authority() {
    for invalid in [
        "[agents]\nallowed_model_providers='zai'",
        "[agents]\ndefault_subagent_model_provider={name='zai'}",
        "[agents.provider_defaults.zai]\nbase_url='https://attacker.invalid'",
    ] {
        assert!(toml::from_str::<ConfigToml>(invalid).is_err(), "{invalid}");
    }
    let config: ConfigToml = toml::from_str(
        "[agents]\nallowed_model_providers=['zai']\n[agents.reviewer]\ndescription='review'",
    )
    .unwrap();
    let agents = config.agents.unwrap();
    assert!(agents.roles.contains_key("reviewer"));
    assert!(!agents.roles.contains_key("allowed_model_providers"));
}

#[test]
fn managed_allowlists_intersect_and_cannot_be_broadened_by_user_config() {
    let fixture = Fixture::new();
    for (mdm, enterprise, user_allow, expected) in [
        (
            "['zai']",
            "['zai','ollama']",
            "['zai','ollama']",
            vec!["zai"],
        ),
        (
            "['zai','ollama']",
            "['ollama']",
            "['zai','ollama']",
            vec!["ollama"],
        ),
        ("[]", "['zai','ollama']", "['zai','ollama']", vec![]),
        ("['zai','ollama']", "['zai','ollama']", "[]", vec![]),
        ("['zai']", "['zai','ollama']", "", vec!["zai"]),
    ] {
        let user = if user_allow.is_empty() {
            fixture
                .user
                .replace("allowed_model_providers = [\"zai\", \"ollama\"]", "")
        } else {
            fixture.user.replace("[\"zai\", \"ollama\"]", user_allow)
        };
        let user = resolve_relative_paths_in_config_toml(
            toml::from_str(&user).unwrap(),
            fixture.home.path(),
        )
        .unwrap();
        let layers = vec![
            ConfigLayerEntry::new(
                ConfigLayerSource::Mdm {
                    domain: "test".into(),
                    key: "test".into(),
                },
                toml::from_str(&format!("[agents]\nallowed_model_providers={mdm}")).unwrap(),
            ),
            ConfigLayerEntry::new(
                ConfigLayerSource::EnterpriseManaged {
                    id: "test".into(),
                    name: "test".into(),
                },
                toml::from_str(&format!("[agents]\nallowed_model_providers={enterprise}")).unwrap(),
            ),
            ConfigLayerEntry::new(
                ConfigLayerSource::User {
                    file: AbsolutePathBuf::from_absolute_path(
                        fixture.home.path().join("config.toml"),
                    )
                    .unwrap(),
                    profile: None,
                },
                user,
            ),
        ];
        let stack = ConfigLayerStack::new(
            layers,
            ConfigRequirements::default(),
            ConfigRequirementsToml::default(),
        )
        .unwrap();
        let policy = ChildProviderPolicy::from_config_layers(
            &stack,
            &AbsolutePathBuf::from_absolute_path(fixture.project.path()).unwrap(),
            &[],
            &[],
            &FileSystemSandboxPolicy::restricted(Vec::new()),
        )
        .unwrap();
        assert_eq!(
            policy.allowed,
            expected
                .into_iter()
                .map(str::to_owned)
                .collect::<BTreeSet<_>>()
        );
        // A managed allowlist is a constraint on switching, not removal of the
        // already-authorized root's legacy exemption.
        assert!(policy.authorize("openai", Some(&origin("openai"))).is_ok());
    }
}

#[test]
fn verified_origin_requires_matching_uuid_root_and_provider_binding() {
    let record = ChildProviderOriginRecord {
        origin_id: Uuid::from_u128(1),
        root_thread_id: ThreadId::from_u128(2),
        provider_id: "openai".into(),
    };
    let binding = ChildProviderOriginBinding {
        origin_id: record.origin_id,
        root_thread_id: record.root_thread_id,
        provider_id: record.provider_id.clone(),
    };
    let verified = VerifiedChildProviderOrigin::verify(&record, &binding).unwrap();
    assert_eq!(verified.provider_id(), "openai");
    assert_eq!(verified.origin_id(), record.origin_id);
    assert_eq!(verified.root_thread_id(), record.root_thread_id);
    let mut mismatches = vec![binding.clone(); 3];
    mismatches[0].origin_id = Uuid::from_u128(3);
    mismatches[1].root_thread_id = ThreadId::from_u128(3);
    mismatches[2].provider_id = "zai".into();
    for mismatch in mismatches {
        assert_eq!(
            VerifiedChildProviderOrigin::verify(&record, &mismatch)
                .unwrap_err()
                .code,
            "child_provider_policy_conflict"
        );
    }
    let mut new_root_origin = record.clone();
    new_root_origin.origin_id = Uuid::from_u128(4);
    new_root_origin.provider_id = "zai".into();
    // A later valid root origin cannot silently rebind an existing child.
    assert!(VerifiedChildProviderOrigin::verify(&new_root_origin, &binding).is_err());
    assert!(serde_json::from_str::<ChildProviderOriginRecord>(r#"{"origin_id":"not-a-uuid","root_thread_id":"00000000-0000-0000-0000-000000000002","provider_id":"openai"}"#).is_err());
}

#[test]
fn native_guidance_filters_authority_managed_constraints_and_definition_shadowing() {
    let fixture = Fixture::new();
    let policy = fixture.policy("", "", None);
    assert_eq!(
        policy.selectable_provider_ids(Some(&origin("openai")), "openai"),
        vec!["ollama", "openai", "zai"]
    );
    assert_eq!(
        fixture
            .policy("", "", Some("zai"))
            .selectable_provider_ids(Some(&origin("openai")), "openai"),
        vec!["zai"]
    );
    let shadowed = fixture.policy(
        "[model_providers.zai]\nname='forged'\nbase_url='https://forged.invalid'",
        "",
        None,
    );
    assert!(
        !shadowed
            .selectable_provider_ids(Some(&origin("openai")), "openai")
            .contains(&"zai".to_owned())
    );
    assert!(
        !policy
            .selectable_provider_ids(None, "openai")
            .contains(&"openai".to_owned())
    );
}

#[test]
fn unrestricted_filesystem_rejects_file_authority_outside_all_workspace_roots() {
    let fixture = Fixture::new();
    let cwd = AbsolutePathBuf::from_absolute_path(fixture.project.path()).unwrap();
    let filesystem = FileSystemSandboxPolicy::unrestricted();
    assert!(
        filesystem
            .get_writable_roots_with_cwd(cwd.as_path())
            .is_empty()
    );
    assert!(
        filesystem
            .can_write_local_path_with_cwd(&fixture.home.path().join("config.toml"), cwd.as_path())
    );
    let policy = fixture.policy_with_runtime_filesystem("", "", None, &[], &filesystem);
    let parent = ModelProviderInfo::default();
    let mut input = request(&parent);
    input.explicit_provider = Some("zai");
    assert_eq!(
        policy.resolve(input).unwrap_err().code,
        "child_provider_untrusted_source"
    );
    assert!(
        !policy
            .selectable_provider_ids(None, "openai")
            .contains(&"zai".into())
    );
}

#[test]
fn non_file_managed_authority_cannot_promote_a_writable_local_catalog() {
    let fixture = Fixture::new();
    let cwd = AbsolutePathBuf::from_absolute_path(fixture.project.path()).unwrap();
    let config = resolve_relative_paths_in_config_toml(
        toml::from_str(&fixture.user).unwrap(),
        fixture.home.path(),
    )
    .unwrap();
    let stack = ConfigLayerStack::new(
        vec![ConfigLayerEntry::new(
            ConfigLayerSource::Mdm {
                domain: "child-provider-test".into(),
                key: "managed".into(),
            },
            config,
        )],
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .unwrap();
    let filesystem = FileSystemSandboxPolicy::unrestricted();
    let policy =
        ChildProviderPolicy::from_config_layers(&stack, &cwd, &[], &[], &filesystem).unwrap();
    // The delivered MDM definition is still trusted; its writable file payload is not.
    assert!(policy.authorize("zai", None).is_ok());
    let parent = ModelProviderInfo::default();
    let mut input = request(&parent);
    input.explicit_provider = Some("zai");
    let error = policy.resolve(input).unwrap_err();
    assert_eq!(error.code, "child_provider_untrusted_source");
    assert!(error.to_string().contains("catalog is writable"));
}

#[test]
fn reopened_read_only_carveout_cannot_retain_file_authority_or_root_exemption() {
    let mut fixture = Fixture::new();
    fixture.home = tempfile::tempdir_in(fixture.project.path()).unwrap();
    std::fs::write(fixture.home.path().join("catalog.json"), "{}").unwrap();
    let protected = [AbsolutePathBuf::from_absolute_path(fixture.home.path()).unwrap()];
    let readonly = fixture.policy_with_runtime_filesystem(
        "",
        "",
        None,
        &protected,
        &FileSystemSandboxPolicy::restricted(Vec::new()),
    );
    let original = readonly.providers["zai"].clone();
    let mut input = request(&original);
    input.explicit_provider = Some("zai");
    assert!(readonly.resolve(input).is_ok());
    // A stale list of old carveouts is insufficient once the actual runtime
    // grants write access. Even the original provider's origin cannot bypass it.
    let reopened = fixture.policy_with_runtime_filesystem(
        "",
        "",
        None,
        &protected,
        &FileSystemSandboxPolicy::unrestricted(),
    );
    let mut input = request(&original);
    input.parent_provider_id = "zai";
    input.authorization_origin = Some(origin("zai"));
    assert_eq!(
        reopened.resolve(input).unwrap_err().code,
        "child_provider_untrusted_source"
    );
}
