//! Host-only, in-memory origin and single-use native launch authority.
//!
//! Issue15 adds durable records and reload verification. A model-visible config,
//! source or UUID never creates an origin or a launch permit here.

use crate::agent::control::LocalAgentRuntime;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::session::Session;
use codex_config::child_provider::ChildProviderOriginBinding;
use codex_config::child_provider::ChildProviderOriginRecord;
use codex_config::child_provider::ChildProviderPolicy;
use codex_config::child_provider::VerifiedChildProviderOrigin;
use codex_history::InitialHistory;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EnvironmentConfig;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use uuid::Uuid;

#[derive(Default)]
pub(crate) struct ChildProviderOrigins(Mutex<OriginState>);

#[derive(Default)]
struct OriginState {
    records: HashMap<Uuid, ChildProviderOriginRecord>,
    bindings: HashMap<ThreadId, ChildProviderOriginBinding>,
    parents: HashMap<ThreadId, ThreadId>,
}

impl ChildProviderOrigins {
    pub(crate) fn parent(&self, thread: ThreadId) -> Option<ThreadId> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .parents
            .get(&thread)
            .copied()
    }

    pub(crate) fn verified(&self, thread: ThreadId) -> Option<VerifiedChildProviderOrigin> {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let binding = state.bindings.get(&thread)?;
        VerifiedChildProviderOrigin::verify(state.records.get(&binding.origin_id)?, binding).ok()
    }

    /// Called only inside successful host thread registration, never from spawn input.
    pub(crate) fn publish_root(&self, thread: ThreadId, provider: &str) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.parents.contains_key(&thread) {
            return;
        }
        if state
            .bindings
            .get(&thread)
            .is_some_and(|old| old.provider_id == provider)
        {
            return;
        }
        let record = ChildProviderOriginRecord {
            origin_id: Uuid::now_v7(),
            root_thread_id: thread,
            provider_id: provider.to_owned(),
        };
        let binding = ChildProviderOriginBinding {
            origin_id: record.origin_id,
            root_thread_id: thread,
            provider_id: provider.to_owned(),
        };
        state.records.insert(record.origin_id, record);
        state.bindings.insert(thread, binding);
    }

    pub(crate) fn publish_child(
        &self,
        child: ThreadId,
        parent: ThreadId,
        origin: Option<VerifiedChildProviderOrigin>,
    ) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.parents.insert(child, parent);
        if let Some(origin) = origin {
            // The permit captured this immutable record at authorized dispatch.
            // A subsequent root update must not rewrite that accepted subtree.
            state.bindings.insert(
                child,
                ChildProviderOriginBinding {
                    origin_id: origin.origin_id(),
                    root_thread_id: origin.root_thread_id(),
                    provider_id: origin.provider_id().to_owned(),
                },
            );
        }
    }
}

/// Host-captured execution authority, distinct from top-level display/default roots.
/// No Session/Thread is retained and no caller can deserialize or supply this scope.
#[derive(Clone, PartialEq)]
pub(crate) struct ChildProviderScope {
    cwd: AbsolutePathBuf,
    roots: Vec<AbsolutePathBuf>,
    profile: PermissionProfile,
    environments: Vec<(TurnEnvironmentSelection, Option<EnvironmentConfig>)>,
}

impl ChildProviderScope {
    pub(crate) fn from_step(config: &Config, environments: &TurnEnvironmentSnapshot) -> Self {
        Self::capture(config, None, environments)
    }

    pub(crate) fn from_current(
        config: &Config,
        settings: &ThreadSettingsSnapshot,
        environments: &TurnEnvironmentSnapshot,
    ) -> Self {
        Self::capture(config, Some(settings), environments)
    }

    fn capture(
        config: &Config,
        settings: Option<&ThreadSettingsSnapshot>,
        environments: &TurnEnvironmentSnapshot,
    ) -> Self {
        let mut cwd = settings
            .map(|settings| settings.cwd.clone())
            .unwrap_or_else(|| config.cwd.clone());
        let mut roots = settings
            .and_then(|settings| settings.runtime_workspace_roots.clone())
            .unwrap_or_else(|| config.workspace_roots.clone());
        let mut profile = config.permissions.permission_profile().clone();
        if let Some(local) = environments.local_selection() {
            if let Ok(local_cwd) = local.cwd.to_abs_path() {
                cwd = local_cwd;
            }
            roots = local
                .workspace_roots
                .iter()
                .filter_map(|root| root.to_abs_path().ok())
                .collect();
            profile = if let Some(ready) = environments
                .turn_environments()
                .find(|environment| environment.selection.environment_id == local.environment_id)
            {
                ready.permission_profile_with_workspace_roots()
            } else if let EnvironmentConfigState::Ready(owner) = &local.config {
                owner
                    .permission_profile
                    .permission_profile()
                    .clone()
                    .materialize_project_roots_with_workspace_roots(&roots)
            } else {
                profile.materialize_project_roots_with_workspace_roots(&roots)
            };
        } else {
            profile = profile.materialize_project_roots_with_workspace_roots(&roots);
        }
        let bindings = environments
            .all_selections()
            .into_iter()
            .map(|selection| {
                let ready = environments
                    .turn_environments()
                    .find(|environment| {
                        environment.selection.environment_id == selection.environment_id
                    })
                    .map(|environment| environment.config().clone());
                (selection, ready)
            })
            .collect();
        Self {
            cwd,
            roots,
            profile,
            environments: bindings,
        }
    }
}

/// Config-only policy probe for managed-binding tests; executing paths use captured scopes.
#[cfg(test)]
pub(crate) fn current_policy(config: &Config) -> std::io::Result<ChildProviderPolicy> {
    current_policy_in_scope(
        config,
        &ChildProviderScope {
            cwd: config.cwd.clone(),
            roots: config.workspace_roots.clone(),
            profile: config.permissions.effective_permission_profile(),
            environments: Vec::new(),
        },
    )
}

/// Re-establish trust using the actual captured executor's filesystem authority.
pub(crate) fn current_policy_in_scope(
    config: &Config,
    scope: &ChildProviderScope,
) -> std::io::Result<ChildProviderPolicy> {
    // Retained layer snapshots cannot authorize new launches after the host
    // observes a changed or suspended managed bundle.
    let managed = config
        .config_layer_stack
        .cloud_config_binding()
        .map(|binding| binding.read());
    if managed
        .as_ref()
        .is_some_and(|guard| guard.status != codex_config::CloudConfigBundleBindingStatus::Current)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed child-provider policy is no longer current",
        ));
    }
    let filesystem = scope.profile.file_system_sandbox_policy();
    let writable = filesystem.get_writable_roots_with_cwd(scope.cwd.as_path());
    let mut roots = scope.roots.clone();
    roots.extend(writable.iter().map(|root| root.root.clone()));
    roots.sort();
    roots.dedup();
    let protected = writable
        .into_iter()
        .flat_map(|root| root.read_only_subpaths)
        .collect::<Vec<_>>();
    ChildProviderPolicy::from_config_layers(
        &config.config_layer_stack,
        &scope.cwd,
        &roots,
        &protected,
        &filesystem,
    )
}

/// A host-captured in-memory ceiling, never a launch credential or durable record.
/// Deliberately has no Debug/Serialize and holds no Session/Thread reference.
#[derive(Clone)]
pub(crate) struct RetainedAgentAuthority {
    pub(crate) thread_id: ThreadId,
    pub(crate) config: Arc<Config>,
    pub(crate) settings: ThreadSettingsSnapshot,
    pub(crate) scope: ChildProviderScope,
    pub(crate) source: SessionSource,
    root_session_id: codex_protocol::SessionId,
    model_authority: Option<String>,
    registration_id: Uuid,
    session_allocation: usize,
}

impl RetainedAgentAuthority {
    pub(crate) async fn capture(session: &Session, source: &SessionSource) -> Arc<Self> {
        let (config, settings, environments) = session.native_agent_authority_snapshot().await;
        let scope = ChildProviderScope::from_current(&config, &settings, &environments);
        let session_allocation = session as *const Session as usize;
        let registration_id = session
            .services
            .local_agent_runtime
            .retained_authority(session.thread_id)
            .filter(|old| old.session_allocation == session_allocation)
            .map(|old| old.registration_id)
            .unwrap_or_else(Uuid::now_v7);
        let model_authority = session
            .services
            .models_manager
            .try_pinned_authority_model(&settings.model, &config.to_models_manager_config())
            .as_ref()
            .and_then(durable::model_fingerprint);
        Arc::new(Self {
            root_session_id: session.session_id(),
            model_authority,
            registration_id,
            session_allocation,
            thread_id: session.thread_id,
            config,
            settings,
            scope,
            source: source.clone(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct PreparedChildAuthorization {
    caller: ThreadId,
    origins: Arc<ChildProviderOrigins>,
    origin: Option<VerifiedChildProviderOrigin>,
    full_history: bool,
    role_name: Option<String>,
    parent_settings: ThreadSettingsSnapshot,
    parent_base: Arc<Config>,
    execution_scope: ChildProviderScope,
    approved: Arc<Config>,
    consumed: Arc<AtomicBool>,
}

impl fmt::Debug for PreparedChildAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedChildAuthorization")
            .field("caller", &self.caller)
            .field("full_history", &self.full_history)
            .finish_non_exhaustive()
    }
}

impl PartialEq for PreparedChildAuthorization {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.consumed, &other.consumed)
    }
}

impl PreparedChildAuthorization {
    pub(crate) fn capture(
        session: &Session,
        config: &Config,
        full_history: bool,
        parent_settings: ThreadSettingsSnapshot,
        parent_base: Arc<Config>,
        role_name: Option<String>,
        execution_scope: ChildProviderScope,
    ) -> Self {
        debug_assert!(config.prepared_child_authorization.is_none());
        Self {
            caller: session.thread_id,
            origins: Arc::clone(&session.services.local_agent_runtime.child_provider_origins),
            origin: session
                .services
                .local_agent_runtime
                .child_provider_origins
                .verified(session.thread_id),
            full_history,
            role_name,
            parent_settings,
            parent_base,
            execution_scope,
            approved: Arc::new(config.clone()),
            consumed: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    pub(crate) fn role_name(&self) -> Option<String> {
        self.role_name.clone()
    }

    pub(crate) async fn consume(
        self,
        runtime: &LocalAgentRuntime,
        caller: ThreadId,
        config: &Config,
        full_history: bool,
        source: &SessionSource,
    ) -> Result<ChildLaunchPermit> {
        if self.caller != caller
            || self.full_history != full_history
            || !Arc::ptr_eq(&self.origins, &runtime.child_provider_origins)
            || self.approved.as_ref() != config
        {
            return Err(invalid("prepared child configuration or caller changed"));
        }
        let manager = runtime.upgrade()?;
        let parent = manager.get_thread(caller).await?;
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth,
            agent_role,
            agent_path,
            ..
        }) = source
        else {
            return Err(invalid("delegated source is not a native child"));
        };
        if *parent_thread_id != caller
            || *depth != crate::agent::next_thread_spawn_depth(&parent.session_source)
            || crate::agent::exceeds_thread_spawn_depth_limit(*depth, config.agent_max_depth)
            || agent_role != &self.role_name
        {
            return Err(invalid(
                "child source changed its prepared parent, role, or depth",
            ));
        }
        if let Some(path) = agent_path {
            let expected_parent = parent
                .session_source
                .get_agent_path()
                .unwrap_or_else(codex_protocol::AgentPath::root);
            if path.as_str().rsplit_once('/').map(|(prefix, _)| prefix)
                != Some(expected_parent.as_str())
            {
                return Err(invalid("child task path is outside its actual parent"));
            }
        }
        let (current, settings, environments) =
            parent.session.native_agent_authority_snapshot().await;
        if !Arc::ptr_eq(
            &self.origins,
            &parent
                .session
                .services
                .local_agent_runtime
                .child_provider_origins,
        ) || settings != self.parent_settings
            || current.as_ref() != self.parent_base.as_ref()
            || self.origins.verified(caller) != self.origin
        {
            return Err(invalid("parent authority changed after child preparation"));
        }
        let scope = ChildProviderScope::from_current(&current, &settings, &environments);
        if scope != self.execution_scope {
            return Err(invalid(
                "execution environment authority changed after child preparation",
            ));
        }
        let policy = current_policy_in_scope(config, &scope)
            .map_err(|_| invalid("current child policy cannot be established"))?;
        policy
            .authorize(&config.model_provider_id, self.origin.as_ref())
            .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
        if self
            .consumed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(invalid("prepared child authorization was already consumed"));
        }
        Ok(ChildLaunchPermit {
            session_id: parent.session.session_id(),
            caller,
            origins: self.origins,
            origin: self.origin,
            approved: self.approved,
        })
    }
}

/// Non-cloneable proof consumed by the lower native thread creation boundary.
pub(crate) struct ChildLaunchPermit {
    session_id: codex_protocol::SessionId,
    caller: ThreadId,
    origins: Arc<ChildProviderOrigins>,
    origin: Option<VerifiedChildProviderOrigin>,
    approved: Arc<Config>,
}

impl ChildLaunchPermit {
    pub(crate) fn consume(
        self,
        runtime: &LocalAgentRuntime,
        parent: Option<ThreadId>,
        config: &Config,
    ) -> Result<(
        Option<VerifiedChildProviderOrigin>,
        codex_protocol::SessionId,
    )> {
        if parent != Some(self.caller)
            || !Arc::ptr_eq(&self.origins, &runtime.child_provider_origins)
            || self.approved.as_ref() != config
        {
            return Err(invalid(
                "native child launch authority does not match its configuration",
            ));
        }
        Ok((self.origin, self.session_id))
    }
}

/// Non-cloneable one-operation authorization for a host-read saved runtime.
/// The caller Arc exists only while this operation is in flight, never in the registry.
pub(crate) struct ChildResumePermit {
    caller: Arc<CodexThread>,
    caller_epoch: Arc<RetainedAgentAuthority>,
    origins: Arc<ChildProviderOrigins>,
    target: ThreadId,
    parent: Option<ThreadId>,
    source: SessionSource,
    history: Arc<Vec<RolloutItem>>,
    history_revision: Option<String>,
    rollout_path: Option<std::path::PathBuf>,
    target_ceiling: Option<Arc<RetainedAgentAuthority>>,
    origin: Option<VerifiedChildProviderOrigin>,
    authorization_origin: Option<VerifiedChildProviderOrigin>,
    cold_receipt: Option<(
        codex_thread_store::NativeAuthorityStorage,
        codex_thread_store::NativeAuthorityReceipt,
    )>,
    environment_selections: Option<Vec<TurnEnvironmentSelection>>,
    inherited_scope: Option<ChildProviderScope>,
    approved: Arc<Config>,
}

impl ChildResumePermit {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn prepare(
        runtime: &LocalAgentRuntime,
        caller: Arc<CodexThread>,
        caller_epoch: Arc<RetainedAgentAuthority>,
        config: &mut Config,
        history: &InitialHistory,
        source: &SessionSource,
        parent: Option<ThreadId>,
        stored_permissions: &PermissionProfile,
        stored_approval: AskForApproval,
        version: MultiAgentVersion,
        selections: &mut Option<Vec<TurnEnvironmentSelection>>,
        inherited: Option<&TurnEnvironmentSnapshot>,
    ) -> Result<Self> {
        let manager = runtime.upgrade()?;
        let registered = manager.get_thread(caller.session.thread_id).await?;
        if !Arc::ptr_eq(&registered, &caller)
            || !caller.is_running()
            || !Arc::ptr_eq(
                &runtime.child_provider_origins,
                &caller
                    .session
                    .services
                    .local_agent_runtime
                    .child_provider_origins,
            )
        {
            return Err(invalid(
                "resume caller is not the actual loaded host runtime",
            ));
        }
        let current =
            RetainedAgentAuthority::capture(&caller.session, &caller.session_source).await;
        if current.thread_id != caller_epoch.thread_id
            || current.config.as_ref() != caller_epoch.config.as_ref()
            || current.settings != caller_epoch.settings
            || current.scope != caller_epoch.scope
            || current.model_authority != caller_epoch.model_authority
            || current.root_session_id != caller_epoch.root_session_id
        {
            return Err(invalid(
                "resume caller authority changed during preparation",
            ));
        }
        let InitialHistory::Resumed(saved) = history else {
            return Err(invalid("private reload requires host-read resumed history"));
        };
        let meta = saved
            .history
            .iter()
            .find_map(|item| match item {
                RolloutItem::SessionMeta(meta) => Some(&meta.meta),
                _ => None,
            })
            .ok_or_else(|| invalid("saved runtime has no host identity metadata"))?;
        if meta.id != saved.conversation_id
            || meta.session_id != caller.session.session_id()
            || meta.parent_thread_id != parent
            || source.parent_thread_id() != parent
            || meta.model_provider.as_deref() != Some(config.model_provider_id.as_str())
        {
            return Err(invalid(&format!(
                "saved runtime identity, provider or ownership is inconsistent (target={}, session={}, parent={}, source_parent={}, provider={})",
                meta.id == saved.conversation_id,
                meta.session_id == caller.session.session_id(),
                meta.parent_thread_id == parent,
                source.parent_thread_id() == parent,
                meta.model_provider.as_deref() == Some(config.model_provider_id.as_str())
            )));
        }
        let target = saved.conversation_id;
        let ceiling = runtime.retained_authority(target);
        if let Some(ceiling) = &ceiling {
            if ceiling.root_session_id != caller.session.session_id()
                || ceiling.root_session_id != meta.session_id
            {
                return Err(invalid("saved runtime belongs to another actual host root"));
            }
            if ceiling.thread_id != target
                || ceiling.source != *source
                || ceiling.config.model_provider_id != config.model_provider_id
                || runtime.child_provider_origins.parent(target) != parent
            {
                return Err(invalid(
                    "saved runtime differs from retained host ownership",
                ));
            }
            if ceiling.settings.approval_policy != current.settings.approval_policy
                || ceiling.settings.approvals_reviewer != current.settings.approvals_reviewer
            {
                return Err(invalid(
                    "saved approval authority requires explicit host reconfiguration",
                ));
            }
            crate::agent::role::preserve_retained_role_restrictions(config, &ceiling.config)
                .map_err(CodexErr::InvalidRequest)?;
            config.permissions.allow_login_shell &= ceiling.config.permissions.allow_login_shell;
            for feature in [
                codex_features::Feature::ShellTool,
                codex_features::Feature::Apps,
                codex_features::Feature::Plugins,
                codex_features::Feature::MemoryTool,
                codex_features::Feature::RequestPermissionsTool,
            ] {
                if !ceiling.config.features.enabled(feature) {
                    config.features.disable(feature).map_err(|_| {
                        invalid("retained tool restriction conflicts with current policy")
                    })?;
                }
            }
        }

        if stored_approval != current.settings.approval_policy {
            return Err(invalid(
                "stored approval mode differs from the current caller",
            ));
        }
        let mut bounded = codex_protocol::intersect_effective_permission_profiles(
            &current.scope.profile,
            stored_permissions,
            &config.cwd,
        )
        .map_err(|_| invalid("stored runtime permissions cannot be bounded safely"))?;
        if let Some(ceiling) = &ceiling {
            bounded = codex_protocol::intersect_effective_permission_profiles(
                &ceiling.scope.profile,
                &bounded,
                &config.cwd,
            )
            .map_err(|_| invalid("retained runtime permissions cannot be bounded safely"))?;
        }
        config
            .permissions
            .set_permission_profile(bounded)
            .map_err(|_| invalid("bounded runtime permissions conflict with host policy"))?;
        config
            .permissions
            .approval_policy
            .set(current.settings.approval_policy)
            .map_err(|_| invalid("bounded approval mode conflicts with host policy"))?;
        config.approvals_reviewer = current.settings.approvals_reviewer;
        if let Some(selections) = selections.as_mut() {
            for selection in selections {
                let EnvironmentConfigState::Ready(environment) = &mut selection.config else {
                    // Pending and failed selections preserve their startup failure;
                    // FromThread is bounded by the Config permission profile above.
                    continue;
                };
                let cwd = selection.cwd.to_abs_path().map_err(|_| {
                    invalid("saved executor permissions cannot be bounded on a non-local workspace")
                })?;
                let roots = selection
                    .workspace_roots
                    .iter()
                    .map(|root| root.to_abs_path())
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|_| invalid("saved executor roots are not local absolute paths"))?;
                let requested = environment
                    .permission_profile
                    .permission_profile()
                    .clone()
                    .materialize_project_roots_with_workspace_roots(&roots);
                let bounded = codex_protocol::intersect_effective_permission_profiles(
                    config.permissions.permission_profile(),
                    &requested,
                    &cwd,
                )
                .map_err(|_| invalid("saved executor permissions cannot be bounded safely"))?;
                if inherited.is_some_and(|snapshot| {
                    snapshot.turn_environments().any(|ready| {
                        ready.selection.environment_id == selection.environment_id
                            && ready.environment.is_remote()
                    })
                }) && bounded != requested
                {
                    return Err(invalid("saved remote executor authority changed"));
                }
                environment.permission_profile =
                    crate::config::PermissionProfileSnapshot::legacy(bounded);
                environment.allow_login_shell &= config.permissions.allow_login_shell;
            }
        }
        // Current trusted host inputs authorize this operation. A retained Config
        // is only a ceiling and never bypasses current managed/file/allowlist checks.
        let policy = current_policy_in_scope(&current.config, &current.scope)
            .map_err(|_| invalid("current resume policy cannot be established"))?;
        let cold_receipt = if ceiling.is_none() {
            let storage = manager
                .native_authority_storage()
                .ok_or_else(|| invalid("cold effective authority has no trusted host store"))?;
            if !durable::protected_storage(&storage, &current.scope) {
                return Err(invalid(
                    "cold authority metadata is writable by the current executor",
                ));
            }
            let receipt = manager
                .read_native_authority_receipt(target)
                .await?
                .ok_or_else(|| invalid("cold effective authority is unsealed or unavailable"))?;
            if receipt.epoch.thread_id != target {
                return Err(invalid("cold authority receipt names another target"));
            }
            durable::verify_cold_lineage(
                &manager,
                &receipt,
                caller.session.session_id().into(),
                parent,
            )
            .await?;
            if history
                .get_dynamic_tools()
                .is_some_and(|tools| !tools.is_empty())
                || caller
                    .session
                    .services
                    .client_mcp_extensions
                    .iter()
                    .next()
                    .is_some()
            {
                return Err(invalid(
                    "cold dynamic tool or extension authority is not supported",
                ));
            }
            let exec = caller.session.services.exec_policy.current();
            if !exec.rules().is_empty()
                || !exec.network_rules().is_empty()
                || !exec.host_executables().is_empty()
            {
                return Err(invalid(
                    "cold custom execution-policy authority is not supported",
                ));
            }
            Some((storage, receipt))
        } else {
            None
        };
        let origin = runtime.child_provider_origins.verified(target);
        let caller_origin = runtime
            .child_provider_origins
            .verified(caller.session.thread_id);
        let authorization_origin = origin
            .as_ref()
            .filter(|target_origin| {
                caller_origin.as_ref().is_some_and(|caller_origin| {
                    caller_origin.origin_id() == target_origin.origin_id()
                        && caller_origin.root_thread_id() == target_origin.root_thread_id()
                })
            })
            .cloned();
        // Only a real same-host retained target may preserve its original-root
        // legacy definition. A cold snapshot never manufactures this exemption.
        let previous = ceiling.as_ref().filter(|ceiling| {
            authorization_origin
                .as_ref()
                .is_some_and(|origin| origin.provider_id() == ceiling.config.model_provider_id)
        });
        let previous_id = previous
            .map(|ceiling| ceiling.config.model_provider_id.as_str())
            .unwrap_or(current.config.model_provider_id.as_str());
        let previous_definition = previous
            .map(|ceiling| &ceiling.config.model_provider)
            .unwrap_or(&current.config.model_provider);
        let model = config
            .model
            .as_deref()
            .ok_or_else(|| invalid("saved runtime has no model"))?;
        let resolved = policy
            .resolve(codex_config::child_provider::ChildProviderRequest {
                explicit_provider: Some(&config.model_provider_id),
                role_provider: None,
                parent_provider_id: previous_id,
                parent_provider: previous_definition,
                parent_model: model,
                authorization_origin: authorization_origin.clone(),
                explicit_model: Some(model),
                role_model: None,
                default_subagent_model: None,
                full_history: false,
                host_owned: false,
            })
            .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
        if ceiling
            .as_ref()
            .is_some_and(|ceiling| ceiling.config.model_provider != resolved.provider)
        {
            return Err(invalid("retained provider definition changed"));
        }
        config.model_provider = resolved.provider.clone();
        config.model_provider_id = resolved.provider_id.clone();
        config.model_catalog = if resolved.model_catalog_json.is_none() {
            ceiling
                .as_ref()
                .and_then(|ceiling| ceiling.config.model_catalog.clone())
        } else {
            None
        };
        config.child_provider_policy = policy;
        let effort = config.model_reasoning_effort.clone();
        let manager = crate::agent::child_config::prepare_resolved_child_models(
            &caller.session,
            config,
            &resolved,
            None,
            effort.clone(),
        )
        .await
        .map_err(CodexErr::InvalidRequest)?;
        // A saved runtime restores its selected effort, including None meaning
        // model default. Fresh-child provider defaults must not materialize it.
        config.model_reasoning_effort = effort;
        crate::agent::child_config::validate_resolved_child_model(
            &manager, config, &resolved, version,
        )
        .await
        .map_err(CodexErr::InvalidRequest)?;
        if let Some(ceiling) = &ceiling {
            let selected = manager
                .try_pinned_authority_model(&resolved.model, &config.to_models_manager_config())
                .as_ref()
                .and_then(durable::model_fingerprint);
            if selected.is_none() || selected != ceiling.model_authority {
                return Err(invalid(
                    "saved selected model tool authority changed or is unavailable",
                ));
            }
        }
        if let Some((storage, receipt)) = &cold_receipt {
            let model = manager
                .try_pinned_authority_model(&resolved.model, &config.to_models_manager_config())
                .ok_or_else(|| invalid("cold selected model authority is not pinned"))?;
            let candidate_scope = durable::candidate_scope(config, selections.as_deref());
            if !durable::protected_storage(storage, &candidate_scope) {
                return Err(invalid(
                    "cold restored executor can write its authority metadata",
                ));
            }
            let disabled = saved
                .history
                .iter()
                .rev()
                .find_map(|item| match item {
                    RolloutItem::TurnContext(context) => context.disabled_plugin_ids.clone(),
                    _ => None,
                })
                .unwrap_or_default();
            let fingerprint = durable::fingerprint(
                config,
                &candidate_scope,
                &model,
                &meta.selected_capability_roots,
                &disabled,
            )
            .ok_or_else(|| invalid("cold effective authority cannot be reconstructed"))?;
            if fingerprint != receipt.fingerprint {
                return Err(invalid(
                    "cold effective authority differs from its sealed host ceiling",
                ));
            }
        }
        Ok(Self {
            caller,
            caller_epoch,
            origins: Arc::clone(&runtime.child_provider_origins),
            target,
            parent,
            source: source.clone(),
            history: Arc::clone(&saved.history),
            history_revision: saved.history_revision.clone(),
            rollout_path: saved.rollout_path.clone(),
            target_ceiling: ceiling,
            cold_receipt,
            origin,
            authorization_origin,
            environment_selections: selections.clone(),
            inherited_scope: inherited
                .map(|snapshot| ChildProviderScope::from_step(config, snapshot)),
            approved: Arc::new(config.clone()),
        })
    }

    pub(crate) async fn consume(
        self,
        runtime: &LocalAgentRuntime,
        config: &Config,
        history: &InitialHistory,
        source: &SessionSource,
        parent: Option<ThreadId>,
        selections: &Option<Vec<TurnEnvironmentSelection>>,
        inherited: Option<&TurnEnvironmentSnapshot>,
    ) -> Result<Option<VerifiedChildProviderOrigin>> {
        let InitialHistory::Resumed(saved) = history else {
            return Err(invalid("reload history changed"));
        };
        if !Arc::ptr_eq(&self.origins, &runtime.child_provider_origins)
            || self.target != saved.conversation_id
            || self.parent != parent
            || self.source != *source
            || !Arc::ptr_eq(&self.history, &saved.history)
            || self.history_revision != saved.history_revision
            || self.rollout_path != saved.rollout_path
            || self.approved.as_ref() != config
            || self.origins.verified(self.target) != self.origin
            || self.environment_selections != *selections
            || self.inherited_scope
                != inherited.map(|snapshot| ChildProviderScope::from_step(config, snapshot))
        {
            return Err(invalid(
                "private resume permit does not match this runtime/history/config",
            ));
        }
        let retained = runtime.retained_authority(self.target);
        if match (&retained, &self.target_ceiling) {
            (Some(current), Some(captured)) => !Arc::ptr_eq(current, captured),
            (None, None) => false,
            _ => true,
        } {
            return Err(invalid("target authority changed after resume preparation"));
        }
        if runtime
            .retained_authority(self.caller.session.thread_id)
            .is_none_or(|registered| {
                registered.registration_id != self.caller_epoch.registration_id
            })
        {
            return Err(invalid(
                "actual caller runtime was replaced before admission",
            ));
        }
        if let Some((storage, receipt)) = &self.cold_receipt {
            let manager = runtime.upgrade()?;
            if manager.native_authority_storage().as_ref() != Some(storage)
                || !durable::protected_storage(storage, &self.caller_epoch.scope)
                || manager
                    .read_native_authority_receipt(self.target)
                    .await?
                    .as_ref()
                    != Some(receipt)
            {
                return Err(invalid("sealed cold authority changed before admission"));
            }
            durable::verify_cold_lineage(
                &manager,
                receipt,
                self.caller.session.session_id().into(),
                self.parent,
            )
            .await?;
        }
        // This actual caller may have been evicted by our own residency reservation.
        // Validate the held host Session epoch, never re-fetch an absent ancestor.
        let current =
            RetainedAgentAuthority::capture(&self.caller.session, &self.caller.session_source)
                .await;
        if current.config.as_ref() != self.caller_epoch.config.as_ref()
            || current.settings != self.caller_epoch.settings
            || current.scope != self.caller_epoch.scope
            || current.model_authority != self.caller_epoch.model_authority
            || current.root_session_id != self.caller_epoch.root_session_id
        {
            return Err(invalid("resume caller authority changed before admission"));
        }
        current_policy_in_scope(&current.config, &current.scope)
            .map_err(|_| invalid("current resume policy was revoked before admission"))?
            .authorize(
                &config.model_provider_id,
                self.authorization_origin.as_ref(),
            )
            .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
        Ok(self.origin)
    }
}

pub(crate) fn invalid(message: &str) -> CodexErr {
    CodexErr::InvalidRequest(format!("child_provider_policy_conflict: {message}"))
}

pub(crate) mod durable;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_reconfiguration_retains_old_descendant_origins() {
        let store = ChildProviderOrigins::default();
        let root = ThreadId::new();
        let child = ThreadId::new();
        let grandchild = ThreadId::new();
        assert!(store.verified(root).is_none());
        store.publish_root(root, "openai");
        let original = store.verified(root).unwrap();
        store.publish_root(root, "openai");
        assert_eq!(store.verified(root), Some(original.clone()));
        store.publish_child(child, root, Some(original.clone()));
        store.publish_root(root, "zai");
        assert_ne!(
            store.verified(root).unwrap().origin_id(),
            original.origin_id()
        );
        assert_eq!(store.verified(child), Some(original.clone()));
        store.publish_child(grandchild, child, store.verified(child));
        assert_eq!(store.verified(grandchild), Some(original));
        // A child can never be promoted to a newly exempt root.
        store.publish_root(child, "ollama");
        assert_eq!(store.verified(child).unwrap().provider_id(), "openai");
    }

    #[test]
    fn missing_or_mismatched_host_record_never_verifies_a_binding() {
        let store = ChildProviderOrigins::default();
        let root = ThreadId::new();
        store.publish_root(root, "openai");
        {
            let mut state = store.0.lock().unwrap();
            state.bindings.get_mut(&root).unwrap().provider_id = "zai".into();
        }
        assert!(store.verified(root).is_none());
        let unbound = ThreadId::new();
        store.publish_child(unbound, root, None);
        assert!(store.verified(unbound).is_none());
    }
}
