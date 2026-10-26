//! Protected host-store effective ceiling; provider origins remain separate.
use super::*;
use crate::windows_sandbox::WindowsSandboxLevelExt;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::openai_models::ModelInfo;
use codex_thread_store::NativeAuthorityEpoch;
use codex_thread_store::NativeAuthorityReceipt;
use codex_thread_store::NativeAuthorityStorage;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

impl RetainedAgentAuthority {
    pub(crate) fn epoch(&self) -> NativeAuthorityEpoch {
        NativeAuthorityEpoch {
            version: codex_thread_store::NATIVE_AUTHORITY_VERSION,
            thread_id: self.thread_id,
            root_thread_id: self.root_session_id.into(),
            parent_thread_id: self.source.parent_thread_id(),
            generation: self.registration_id,
        }
    }
}

fn canonical_existing_or_parent(path: &Path) -> Option<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Some(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Some(path.parent()?.canonicalize().ok()?.join(path.file_name()?))
        }
        Err(_) => None,
    }
}

// SQLite opens the configured lexical path. Reject aliases before resolving it:
// checking only the current destination cannot authenticate an already-open pool.
fn unaliased_storage_path(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let mut prefix = PathBuf::new();
    for component in path.components() {
        if matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::CurDir
        ) {
            return false;
        }
        prefix.push(component.as_os_str());
        match std::fs::symlink_metadata(&prefix) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return false;
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & 0x400 != 0 {
                        return false;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && prefix == path => {}
            Err(_) => return false,
        }
    }
    true
}

/// Check actual store replacement and sidecar paths, never a rollout-supplied path.
pub(crate) fn protected_storage(
    storage: &NativeAuthorityStorage,
    scope: &ChildProviderScope,
) -> bool {
    let NativeAuthorityStorage::Local {
        directory,
        database,
    } = storage
    else {
        return true;
    };
    if !unaliased_storage_path(directory) || !unaliased_storage_path(database) {
        return false;
    }
    let lexical_directory = directory.clone();
    let lexical_database = database.clone();
    let Some(directory) = directory.canonicalize().ok() else {
        return false;
    };
    let Some(database) = canonical_existing_or_parent(database) else {
        return false;
    };
    if database.parent() != Some(directory.as_path()) {
        return false;
    }
    let mut paths = vec![
        lexical_directory,
        lexical_database,
        directory,
        database.clone(),
    ];
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut path = database.as_os_str().to_owned();
        path.push(suffix);
        if !unaliased_storage_path(Path::new(&path)) {
            return false;
        }
        paths.push(PathBuf::from(&path));
        let Some(path) = canonical_existing_or_parent(Path::new(&path)) else {
            return false;
        };
        paths.push(path);
    }
    // Replacing a writable ancestor can redirect a later SQLite open even if
    // the database itself has a read-only carveout.
    let ancestors = paths
        .iter()
        .flat_map(|path| path.ancestors().skip(1))
        .filter(|path| path.parent().is_some())
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    paths.extend(ancestors);
    let denied = |profile: &PermissionProfile, cwd: &Path| {
        paths.iter().all(|path| {
            !profile
                .file_system_sandbox_policy()
                .can_write_local_path_with_cwd(path, cwd)
        })
    };
    if !denied(&scope.profile, scope.cwd.as_path()) {
        return false;
    }
    for (selection, ready) in &scope.environments {
        let (Some(ready), Ok(cwd)) = (ready, selection.cwd.to_abs_path()) else {
            return false;
        };
        let Ok(roots) = selection
            .workspace_roots
            .iter()
            .map(|root| root.to_abs_path())
            .collect::<std::result::Result<Vec<_>, _>>()
        else {
            return false;
        };
        let profile = ready
            .permission_profile
            .permission_profile()
            .clone()
            .materialize_project_roots_with_workspace_roots(&roots);
        if !denied(&profile, cwd.as_path()) {
            return false;
        }
    }
    true
}

fn canonical_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => {
            let ordered = object.into_iter().collect::<BTreeMap<_, _>>();
            let mut output = serde_json::Map::new();
            for (key, value) in ordered {
                output.insert(key, canonical_json(value));
            }
            serde_json::Value::Object(output)
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_json).collect())
        }
        other => other,
    }
}

// Bump this semantic revision whenever the compiled classifier policy changes.
// Text is compared only in memory, never serialized or hashed into a receipt.
const BUILTIN_GUARDIAN_POLICY: &str = "codex-builtin-guardian-policy-20261026-v1";

fn model_grants(model: &ModelInfo) -> Option<serde_json::Value> {
    let guardian = model
        .model_messages
        .as_ref()
        .and_then(|m| m.guardian_v2.as_ref());
    let policy_id = match guardian.and_then(|g| g.classifier_instructions.as_deref()) {
        None => None,
        Some(instructions) => {
            let bundled = codex_models_manager::bundled_models_response().ok()?;
            let owner = if instructions
                == codex_prompts::ResolvedModelMessages::bundled()
                    .guardian_classifier_instructions()
            {
                "default".to_owned()
            } else {
                bundled
                    .models
                    .iter()
                    .find(|m| {
                        m.model_messages
                            .as_ref()
                            .and_then(|m| m.guardian_v2.as_ref())
                            .and_then(|g| g.classifier_instructions.as_deref())
                            == Some(instructions)
                    })?
                    .slug
                    .clone()
            };
            Some(format!("{BUILTIN_GUARDIAN_POLICY}:{owner}"))
        }
    };
    let guardian = guardian.map(|g| json!({
        "policy":policy_id,
        "mode":g.async_classifier_mode,"conversation_limit":g.async_classifier_conversation_token_limit,
        "threshold":g.review_threshold_basis_points,"lag":g.max_tool_call_lag,
        "effort":g.reasoning_effort,"transcript":g.transcript,"action_limit":g.max_action_tokens,
        "instruction_limit":g.max_classifier_instruction_tokens,"reuse_compaction":g.reuse_parent_compaction,
        "compaction_limit":g.max_parent_compaction_tokens,
    }));
    Some(json!({
        "shell":model.shell_type,"patch":model.apply_patch_tool_type,"search":model.web_search_tool_type,
        "experimental_tools":model.experimental_supported_tools,"supports_search":model.supports_search_tool,
        "node_disabled":model.node_repl_disabled,"node_review":model.node_repl_auto_review_required,
        "tool_mode":model.tool_mode,"multi_agent":model.multi_agent_version,"guardian":model.guardian,
        "review_model":model.auto_review_model_override,"guardian_v2":guardian,
    }))
}

fn local_guardian_grants(config: &Config, model: &ModelInfo) -> Option<serde_json::Value> {
    if config.guardian_policy_config.is_some() || config.guardian_extra_policy.is_some() {
        return None;
    }
    let effective = config.config_layer_stack.effective_config();
    let mut local = match effective
        .get("features")
        .and_then(|f| f.get("guardianv2"))
        .cloned()
    {
        Some(value) => {
            let parsed: codex_features::FeatureToml<codex_features::GuardianV2ConfigToml> =
                value.try_into().ok()?;
            match parsed {
                codex_features::FeatureToml::Enabled(_) => {
                    codex_features::GuardianV2ConfigToml::default()
                }
                codex_features::FeatureToml::Config(value) => value,
            }
        }
        None => codex_features::GuardianV2ConfigToml::default(),
    };
    if local.classifier_instructions.is_some() {
        return None;
    }
    local.enabled = Some(config.features.enabled(codex_features::Feature::GuardianV2));
    let legacy = codex_features::FeatureToml::Config(local.clone());
    let requirements = config.config_layer_stack.requirements();
    let mut policy =
        codex_config::GuardianPolicyLoader::new(Some(&legacy), requirements).resolve(Some(model));
    requirements.constrain_guardian_policy(&mut policy, &model.slug);
    Some(json!({
        "policy_revision":BUILTIN_GUARDIAN_POLICY,"policy":policy,"transcript_mode":config.guardian_transcript_mode,
        "mode":local.async_classifier_mode,"conversation_limit":local.async_classifier_conversation_token_limit,
        "threshold":local.review_threshold,"lag":local.max_tool_call_lag,"effort":local.reasoning_effort,
        "action_limit":local.max_action_tokens,"instruction_limit":local.max_classifier_instruction_tokens,
        "reuse_compaction":local.reuse_parent_compaction,"compaction_limit":local.max_parent_compaction_tokens,
        "scope":local.review_scope,"transcript":local.transcript,
    }))
}

pub(crate) fn model_fingerprint(config: &Config, model: &ModelInfo) -> Option<String> {
    if model.used_fallback_model_metadata {
        return None;
    }
    let bytes = serde_json::to_vec(&canonical_json(
        json!({"model":model_grants(model)?,"guardian":local_guardian_grants(config, model)?}),
    ))
    .ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

/// Grant-only projection. No provider/auth definition, transport, environment
/// value, prompt text or credential-derived hash enters this value.
pub(crate) fn fingerprint(
    config: &Config,
    scope: &ChildProviderScope,
    model: &ModelInfo,
    roots: &[SelectedCapabilityRoot],
    disabled_plugins: &[String],
) -> Option<String> {
    if model.used_fallback_model_metadata {
        return None;
    }
    let features = codex_features::FEATURES
        .iter()
        .map(|spec| (spec.key, config.features.enabled(spec.id)))
        .collect::<BTreeMap<_, _>>();
    let skills = codex_config::skill_config_rules_from_stack(&config.config_layer_stack);
    let skill_rules = skills
        .entries
        .iter()
        .map(|rule| match &rule.selector {
            codex_config::SkillConfigRuleSelector::Name(name) => {
                json!({"name":name,"enabled":rule.enabled})
            }
            codex_config::SkillConfigRuleSelector::Path(path) => {
                json!({"path":path,"enabled":rule.enabled})
            }
        })
        .collect::<Vec<_>>();
    let mcp = config
        .mcp_servers
        .get()
        .iter()
        .map(|(name, server)| {
            (
                name,
                json!({
                    "environment":server.environment_id,"enabled":server.enabled,
                    "enabled_tools":server.enabled_tools,"disabled_tools":server.disabled_tools,
                    "approval":server.default_tools_approval_mode,"tools":server.tools,
                    "omit":server.omit_tools_from,"scopes":server.scopes,
                }),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let plugins = config.plugins.plugins.iter().map(|(name, plugin)| (name,json!({
        "enabled":plugin.enabled,"remote":plugin.remote_plugin_id,"release":plugin.release_id,
        "servers":plugin.mcp_servers,
    }))).collect::<BTreeMap<_,_>>();
    let mut grants = Vec::new();
    if scope.environments.is_empty() {
        let profile = codex_protocol::intersect_effective_permission_profiles(
            &scope.profile,
            &scope.profile,
            scope.cwd.as_path(),
        )
        .ok()?;
        grants.push(
            json!({"cwd":scope.cwd,"roots":scope.roots,"profile":profile,
            "login_shell":config.permissions.allow_login_shell}),
        );
    } else {
        for (selection, ready) in &scope.environments {
            let ready = ready.as_ref()?;
            let cwd = selection.cwd.to_abs_path().ok()?;
            let paths = selection
                .workspace_roots
                .iter()
                .map(|root| root.to_abs_path())
                .collect::<std::result::Result<Vec<_>, _>>()
                .ok()?;
            let profile = ready
                .permission_profile
                .permission_profile()
                .clone()
                .materialize_project_roots_with_workspace_roots(&paths);
            let profile = codex_protocol::intersect_effective_permission_profiles(
                &profile,
                &profile,
                cwd.as_path(),
            )
            .ok()?;
            grants.push(json!({"cwd":cwd,"roots":paths,"profile":profile,"login_shell":ready.allow_login_shell}));
        }
    }
    let mut grants = grants
        .into_iter()
        .map(|value| serde_json::to_string(&canonical_json(value)).ok())
        .collect::<Option<Vec<_>>>()?;
    grants.sort();
    grants.dedup();
    let projection = json!({
        "version":codex_thread_store::NATIVE_AUTHORITY_VERSION,
        "features":features,"skills":skill_rules,"bundled_skills":config.bundled_skills_enabled(),
        "skill_instructions":config.include_skill_instructions,"plugins":plugins,
        "plugin_default":config.plugins.default,"disabled_plugins":disabled_plugins,
        "mcp":mcp,"mcp_prefix":config.non_prefixed_mcp_tool_servers,
        "approval":config.permissions.approval_policy.value(),"reviewer":config.approvals_reviewer,
        "web_search":config.web_search_mode.value(),"request_user_input":config.experimental_request_user_input_enabled,
        "update_plan":config.update_plan_enabled,"tool_registry":config.tool_registry,"code_mode":config.code_mode,
        "capability_roots":roots,"executors":grants,
        "provider_tool_capabilities":config.model_provider.capabilities,
        "model_capabilities":model_grants(model)?,"guardian_config":local_guardian_grants(config, model)?,
    });
    let bytes = serde_json::to_vec(&canonical_json(projection)).ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

/// Reconstruct exactly the executable grants that startup will receive. An
/// omitted selection uses Config defaults, not the sender's unrelated bindings.
pub(crate) fn candidate_scope(
    config: &Config,
    selections: Option<&[TurnEnvironmentSelection]>,
) -> ChildProviderScope {
    let environments = selections
        .unwrap_or_default()
        .iter()
        .map(|selection| {
            let ready = match &selection.config {
                EnvironmentConfigState::Ready(ready) => Some(ready.clone()),
                EnvironmentConfigState::FromThread
                    if selection.environment_id == codex_exec_server::LOCAL_ENVIRONMENT_ID =>
                {
                    // Match SessionConfiguration::inferred_environment_config and
                    // ThreadEnvironmentDefaults::for_selection for a local startup.
                    Some(EnvironmentConfig {
                        allow_login_shell: config.permissions.allow_login_shell,
                        workspace_roots: selection.workspace_roots.clone(),
                        permission_profile: config
                            .permissions
                            .permission_profile_state()
                            .snapshot(),
                        shell_environment_policy: config
                            .permissions
                            .shell_environment_policy
                            .clone(),
                        windows_sandbox_level:
                            codex_protocol::config_types::WindowsSandboxLevel::from_config(config),
                        windows_sandbox_type: config.effective_local_windows_sandbox_type(),
                        use_legacy_landlock: config.features.use_legacy_landlock(),
                        exec_policy: None,
                        mcp_policy: None,
                        network_policy: None,
                        selected_capability_roots: Vec::new(),
                    })
                }
                _ => None,
            };
            (selection.clone(), ready)
        })
        .collect();
    ChildProviderScope {
        cwd: config.cwd.clone(),
        roots: config.workspace_roots.clone(),
        profile: config.permissions.effective_permission_profile(),
        environments,
    }
}

pub(crate) async fn verify_cold_lineage(
    manager: &crate::thread_manager::ThreadManagerState,
    receipt: &NativeAuthorityReceipt,
    root: ThreadId,
    parent: Option<ThreadId>,
) -> Result<()> {
    receipt
        .validate()
        .map_err(|_| invalid("invalid saved authority receipt"))?;
    if receipt.epoch.root_thread_id != root || receipt.epoch.parent_thread_id != parent {
        return Err(invalid("saved authority belongs to another host tree"));
    }
    let mut cursor = Some(receipt.epoch.thread_id);
    let mut seen = HashSet::new();
    while let Some(thread) = cursor {
        if !seen.insert(thread) || seen.len() > 1024 {
            return Err(invalid("saved authority ownership cycle"));
        }
        let epoch = manager
            .read_native_authority_epoch(thread)
            .await?
            .ok_or_else(|| invalid("saved host authority ownership is unavailable"))?;
        if epoch.root_thread_id != root || (epoch.parent_thread_id.is_none() && thread != root) {
            return Err(invalid("saved host authority ownership is inconsistent"));
        }
        cursor = epoch.parent_thread_id;
    }
    if !seen.contains(&root) {
        return Err(invalid("saved host authority has no root"));
    }
    Ok(())
}

/// Seal only a stopped actual host Session; the live registry stores no strong Session.
pub(crate) async fn seal_stopped(
    manager: &crate::thread_manager::ThreadManagerState,
    thread: &CodexThread,
    authority: &Arc<RetainedAgentAuthority>,
) -> Result<()> {
    if thread.is_running()
        || (authority.source.is_non_root_agent()
            && !matches!(
                authority.source,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
            ))
    {
        return Ok(());
    }
    let Some(storage) = manager.native_authority_storage() else {
        return Ok(());
    };
    if !protected_storage(&storage, &authority.scope) {
        return Ok(());
    }
    let runtime = &thread.session.services.local_agent_runtime;
    let mut parent = authority.source.parent_thread_id();
    let mut visited = HashSet::new();
    while let Some(id) = parent {
        if !visited.insert(id) {
            return Ok(());
        }
        let Some(ancestor) = runtime.retained_authority(id) else {
            return Ok(());
        };
        if !protected_storage(&storage, &ancestor.scope) {
            return Ok(());
        }
        parent = ancestor.source.parent_thread_id();
    }
    // Unknown dynamic tool/extension/exec-policy ceilings are deliberately not
    // serialized or approximated by a role name. Issue15 can add typed support.
    if !thread.session.dynamic_tools().await.is_empty()
        || thread
            .session
            .services
            .client_mcp_extensions
            .iter()
            .next()
            .is_some()
    {
        return Ok(());
    }
    let policy = thread.session.services.exec_policy.current();
    if !policy.rules().is_empty()
        || !policy.network_rules().is_empty()
        || !policy.host_executables().is_empty()
    {
        return Ok(());
    }
    let config = &authority.config;
    let manager_config = config.to_models_manager_config();
    let Some(model) = thread
        .session
        .services
        .models_manager
        .try_pinned_authority_model(&authority.settings.model, &manager_config)
    else {
        return Ok(());
    };
    let Some(fingerprint) = fingerprint(
        config,
        &authority.scope,
        &model,
        &thread.session.services.selected_capability_roots,
        &authority.settings.disabled_plugin_ids,
    ) else {
        return Ok(());
    };
    let current = RetainedAgentAuthority::capture(&thread.session, &thread.session_source).await;
    if current.config.as_ref() != config.as_ref()
        || current.settings != authority.settings
        || current.scope != authority.scope
        || current.registration_id != authority.registration_id
        || thread
            .session
            .services
            .models_manager
            .try_pinned_authority_model(&authority.settings.model, &manager_config)
            .as_ref()
            != Some(&model)
    {
        return Ok(());
    }
    manager
        .seal_native_authority_receipt(NativeAuthorityReceipt {
            epoch: authority.epoch(),
            fingerprint,
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigBuilder;

    #[tokio::test]
    async fn grant_fingerprint_tracks_effective_feature_and_model_limits_but_excludes_credentials()
    {
        let home = tempfile::tempdir().unwrap();
        let mut config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .unwrap();
        config
            .features
            .enable(codex_features::Feature::ShellTool)
            .unwrap();
        let scope = candidate_scope(&config, None);
        let mut model = codex_models_manager::model_info::model_info_from_slug("gpt-5.6-sol");
        model.used_fallback_model_metadata = false;
        let baseline = fingerprint(&config, &scope, &model, &[], &[]).unwrap();
        let mut secret_changed = config.clone();
        secret_changed.model_provider.base_url =
            Some("https://example.invalid/private-token-in-routing".into());
        secret_changed.model_provider.auth=Some(serde_json::from_value(json!({"command":"synthetic-secret-command","args":["synthetic-secret-value"],"timeout_ms":5000,"refresh_interval_ms":0})).unwrap());
        assert_eq!(
            fingerprint(&secret_changed, &scope, &model, &[], &[]).unwrap(),
            baseline,
            "provider routing/auth material must not be serialized or hashed as authority"
        );
        config
            .features
            .disable(codex_features::Feature::ShellTool)
            .unwrap();
        assert_ne!(
            fingerprint(&config, &scope, &model, &[], &[]).unwrap(),
            baseline
        );
        let before = fingerprint(&config, &scope, &model, &[], &[]).unwrap();
        model.node_repl_disabled = !model.node_repl_disabled;
        assert_ne!(
            fingerprint(&config, &scope, &model, &[], &[]).unwrap(),
            before
        );
        let before = fingerprint(&config, &scope, &model, &[], &[]).unwrap();
        model
            .experimental_supported_tools
            .push("new-experimental-grant".into());
        assert_ne!(
            fingerprint(&config, &scope, &model, &[], &[]).unwrap(),
            before
        );
        model.used_fallback_model_metadata = true;
        assert!(fingerprint(&config, &scope, &model, &[], &[]).is_none());
    }

    #[tokio::test]
    async fn guardian_grants_exclude_nonpolicy_prompts_and_reject_opaque_policy() {
        let home = tempfile::tempdir().unwrap();
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .unwrap();
        let scope = candidate_scope(&config, None);
        let mut model = codex_models_manager::model_info::model_info_from_slug("gpt-5.6-sol");
        model.used_fallback_model_metadata = false;
        let before = fingerprint(&config, &scope, &model, &[], &[]).unwrap();
        model
            .model_messages
            .get_or_insert_with(Default::default)
            .instructions_template = Some("nonpolicy developer text".into());
        assert_eq!(
            fingerprint(&config, &scope, &model, &[], &[]).unwrap(),
            before
        );
        let guardian = model
            .model_messages
            .as_mut()
            .unwrap()
            .guardian_v2
            .get_or_insert_with(Default::default);
        guardian.review_threshold_basis_points = Some(1234);
        assert_ne!(
            fingerprint(&config, &scope, &model, &[], &[]).unwrap(),
            before
        );
        model
            .model_messages
            .as_mut()
            .unwrap()
            .guardian_v2
            .as_mut()
            .unwrap()
            .classifier_instructions = Some("custom safety classifier".into());
        assert!(model_fingerprint(&config, &model).is_none());
        assert!(fingerprint(&config, &scope, &model, &[], &[]).is_none());
        model.model_messages.as_mut().unwrap().guardian_v2 = None;
        let overridden = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .cli_overrides(vec![(
                "features.guardianv2.review_threshold".into(),
                toml::Value::Float(0.25),
            )])
            .build()
            .await
            .unwrap();
        assert_ne!(
            fingerprint(&config, &scope, &model, &[], &[]),
            fingerprint(&overridden, &scope, &model, &[], &[])
        );
        let opaque = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .cli_overrides(vec![(
                "features.guardianv2.classifier_instructions".into(),
                toml::Value::String("opaque local policy".into()),
            )])
            .build()
            .await
            .unwrap();
        assert!(fingerprint(&opaque, &scope, &model, &[], &[]).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sqlite_pool_path_alias_cannot_become_protected_authority_after_replacement() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        let original = tempfile::tempdir().unwrap();
        let replacement = tempfile::tempdir().unwrap();
        let alias = home.path().join("sqlite-alias");
        symlink(original.path(), &alias).unwrap();
        let sqlite = codex_state::SqliteConfig::new_for_testing(
            AbsolutePathBuf::try_from(alias.clone()).unwrap(),
        );
        let database = sqlite.state_db_path().to_path_buf();
        let runtime = codex_state::StateRuntime::init(sqlite, "test".into())
            .await
            .unwrap();
        let root = ThreadId::new();
        let epoch = NativeAuthorityEpoch {
            version: codex_thread_store::NATIVE_AUTHORITY_VERSION,
            thread_id: root,
            root_thread_id: root,
            parent_thread_id: None,
            generation: uuid::Uuid::now_v7(),
        };
        runtime.begin_native_authority_epoch(&epoch).await.unwrap();
        std::fs::remove_file(&alias).unwrap();
        symlink(replacement.path(), &alias).unwrap();
        assert_eq!(
            runtime.read_native_authority_epoch(root).await.unwrap(),
            Some(epoch)
        );
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .unwrap();
        let mut scope = candidate_scope(&config, None);
        scope.profile = PermissionProfile::read_only();
        assert!(!protected_storage(
            &NativeAuthorityStorage::Local {
                directory: alias,
                database
            },
            &scope
        ));
    }

    #[tokio::test]
    async fn local_from_thread_projection_matches_actual_startup_but_unknown_bindings_have_no_proof()
     {
        let home = tempfile::tempdir().unwrap();
        let mut config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .unwrap();
        config
            .permissions
            .set_permission_profile(PermissionProfile::read_only())
            .unwrap();
        let manager = crate::ThreadManager::with_models_provider_and_home_for_tests(
            codex_login::CodexAuth::from_api_key("projection-only"),
            config.model_provider.clone(),
            home.path().to_path_buf(),
            Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        );
        let root = manager
            .start_thread(crate::thread_manager::StartThreadOptions::new(config))
            .await
            .unwrap()
            .thread;
        let (config, settings, environments) = root.session.native_agent_authority_snapshot().await;
        let actual = ChildProviderScope::from_current(&config, &settings, &environments);
        let ready = environments
            .primary()
            .expect("actual local startup attachment");
        let mut selection = ready.selection();
        selection.config = EnvironmentConfigState::FromThread;
        let projected = candidate_scope(&config, Some(std::slice::from_ref(&selection)));
        assert!(
            projected.environments[0].1.as_ref() == Some(ready.config()),
            "pure FromThread projection must equal actual startup authority"
        );
        let model = root
            .session
            .new_default_turn()
            .await
            .model_info()
            .as_ref()
            .clone();
        assert_eq!(
            fingerprint(&config, &projected, &model, &[], &[]),
            fingerprint(&config, &actual, &model, &[], &[])
        );
        let storage = NativeAuthorityStorage::Local {
            directory: home.path().to_path_buf(),
            database: home.path().join("authority.sqlite"),
        };
        assert!(protected_storage(&storage, &projected));
        assert!(protected_storage(&storage, &actual));
        selection.environment_id = "unknown-remote".into();
        let unknown = candidate_scope(&config, Some(std::slice::from_ref(&selection)));
        assert!(fingerprint(&config, &unknown, &model, &[], &[]).is_none());
        assert!(!protected_storage(&storage, &unknown));
        selection.environment_id = codex_exec_server::LOCAL_ENVIRONMENT_ID.into();
        selection.config = EnvironmentConfigState::Pending;
        let pending = candidate_scope(&config, Some(&[selection]));
        assert!(fingerprint(&config, &pending, &model, &[], &[]).is_none());
        assert!(!protected_storage(&storage, &pending));
    }

    #[tokio::test]
    async fn writable_actual_database_directory_or_sidecars_are_never_authority() {
        let home = tempfile::tempdir().unwrap();
        let config = ConfigBuilder::without_managed_config_for_tests()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .unwrap();
        let mut scope = candidate_scope(&config, None);
        scope.profile = PermissionProfile::read_only();
        let storage = NativeAuthorityStorage::Local {
            directory: home.path().to_path_buf(),
            database: home.path().join("state.sqlite"),
        };
        assert!(protected_storage(&storage, &scope));
        scope.profile = PermissionProfile::Disabled;
        assert!(!protected_storage(&storage, &scope));
        let unrelated = tempfile::tempdir().unwrap();
        let foreign = NativeAuthorityStorage::Local {
            directory: home.path().to_path_buf(),
            database: unrelated.path().join("state.sqlite"),
        };
        scope.profile = PermissionProfile::read_only();
        assert!(
            !protected_storage(&foreign, &scope),
            "store ownership/path mismatch cannot qualify"
        );
    }
}
