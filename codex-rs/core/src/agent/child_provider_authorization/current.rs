use super::*;

/// Private, one-operation fence. It cannot select a provider or refresh metadata.
/// Each synchronous decision admits exactly one queue/helper/transport stage.
/// That stage may finish after publication; later stages must be admitted again.
#[derive(Clone)]
pub(crate) struct CurrentChildOperation {
    root: Arc<CodexThread>,
    target: Arc<CodexThread>,
    owners: Vec<(Arc<CodexThread>, u64)>,
    policies: Vec<(Arc<Config>, ChildProviderScope, bool)>,
    ceiling: Arc<RetainedAgentAuthority>,
    selected: Arc<RetainedAgentAuthority>,
    origin: VerifiedChildProviderOrigin,
    #[cfg(test)]
    after_policy_validation: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl CurrentChildOperation {
    pub(crate) async fn prepare(
        target: Arc<CodexThread>,
        caller: Option<Arc<CodexThread>>,
        scope: Option<ChildProviderScope>,
    ) -> Result<Option<Self>> {
        let runtime = &target.session.services.local_agent_runtime;
        let manager = runtime.upgrade()?;
        let ceiling = runtime
            .retained_authority(target.session.thread_id)
            .filter(|authority| authority.matches_session(&target.session))
            .ok_or_else(|| invalid("current target registration is unavailable"))?;
        // Host-owned root/reviewer/Guardian paths are authenticated by the exact
        // retained registration and its actual source, never by a role string.
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) = &target.session_source
        else {
            return Ok(None);
        };
        if ceiling.source != target.session_source
            || runtime
                .child_provider_origins
                .parent(target.session.thread_id)
                != Some(*parent_thread_id)
        {
            return Err(invalid("current native child ownership is unavailable"));
        }
        let origin = runtime
            .child_provider_origins
            .verified(target.session.thread_id)
            .ok_or_else(|| invalid("current native child origin is unavailable"))?;
        let root = manager
            .get_thread(origin.root_thread_id())
            .await
            .map_err(|_| invalid("current host policy owner is unavailable"))?;
        if !Arc::ptr_eq(
            &runtime.registry,
            &root.session.services.local_agent_runtime.registry,
        ) || root.session_source.parent_thread_id().is_some()
            || root.session.session_id() != ceiling.root_session_id
        {
            return Err(invalid("current host policy owner was replaced"));
        }
        let mut threads = vec![Arc::clone(&root), Arc::clone(&target)];
        if let Some(caller) = &caller {
            if !Arc::ptr_eq(
                &runtime.registry,
                &caller.session.services.local_agent_runtime.registry,
            ) {
                return Err(invalid("operation caller belongs to another runtime"));
            }
            threads.push(Arc::clone(caller));
        }
        threads.sort_by_key(|thread| {
            (
                thread.session.thread_id.to_string(),
                thread.session.native_registration_id,
            )
        });
        threads.dedup_by(|left, right| Arc::ptr_eq(left, right));
        let mut owners = Vec::new();
        let mut snapshots = Vec::new();
        for thread in threads {
            let before = *thread
                .session
                .child_policy_revision
                .lock()
                .map_err(|_| invalid("current policy publication fence is unavailable"))?;
            let snapshot =
                RetainedAgentAuthority::capture(&thread.session, &thread.session_source).await;
            if before
                != *thread
                    .session
                    .child_policy_revision
                    .lock()
                    .map_err(|_| invalid("current policy publication fence is unavailable"))?
            {
                return Err(invalid(
                    "current policy changed during operation preparation",
                ));
            }
            if !manager
                .get_thread(thread.session.thread_id)
                .await
                .is_ok_and(|registered| Arc::ptr_eq(&registered, &thread))
            {
                return Err(invalid("operation allocation was replaced"));
            }
            if snapshot
                .scope
                .environments
                .iter()
                .any(|(_, config)| config.is_none())
            {
                return Err(invalid(
                    "current executor authority is pending or unavailable",
                ));
            }
            snapshots.push((Arc::clone(&thread), snapshot));
            owners.push((thread, before));
        }
        let selected = snapshots
            .iter()
            .find(|(thread, _)| Arc::ptr_eq(thread, &target))
            .map(|(_, snapshot)| Arc::clone(snapshot))
            .expect("target captured");
        let root_snapshot = snapshots
            .iter()
            .find(|(thread, _)| Arc::ptr_eq(thread, &root))
            .map(|(_, snapshot)| snapshot)
            .expect("root captured");
        let target_scope = scope.unwrap_or_else(|| selected.scope.clone());
        if target_scope
            .environments
            .iter()
            .any(|(_, config)| config.is_none())
        {
            return Err(invalid(
                "inference executor authority is pending or unavailable",
            ));
        }
        let mut policies = vec![
            (
                Arc::clone(&root_snapshot.config),
                root_snapshot.scope.clone(),
                true,
            ),
            (Arc::clone(&root_snapshot.config), target_scope, true),
        ];
        if let Some(caller) = caller {
            let caller_snapshot = snapshots
                .iter()
                .find(|(thread, _)| Arc::ptr_eq(thread, &caller))
                .map(|(_, snapshot)| snapshot)
                .expect("caller captured");
            policies.push((
                Arc::clone(&caller_snapshot.config),
                caller_snapshot.scope.clone(),
                runtime
                    .child_provider_origins
                    .verified(caller.session.thread_id)
                    .is_some_and(|caller_origin| {
                        caller_origin.origin_id() == origin.origin_id()
                            && caller_origin.root_thread_id() == origin.root_thread_id()
                    }),
            ));
        }
        let operation = Self {
            root,
            target,
            owners,
            policies,
            ceiling,
            selected,
            origin,
            #[cfg(test)]
            after_policy_validation: None,
        };
        operation.with_current(|| Ok(()))?;
        Ok(Some(operation))
    }

    fn validate_policy(&self) -> Result<()> {
        if !self.ceiling.same_registration(&self.selected)
            || self.ceiling.config.model_provider_id != self.selected.config.model_provider_id
            || self.ceiling.config.model_provider != self.selected.config.model_provider
            || self.ceiling.model_authority.is_none()
            || self.ceiling.model_authority != self.selected.model_authority
            || self.ceiling.source != self.selected.source
            || self.ceiling.scope != self.selected.scope
            || self.ceiling.settings.approval_policy != self.selected.settings.approval_policy
            || self.ceiling.settings.approvals_reviewer != self.selected.settings.approvals_reviewer
        {
            return Err(invalid(
                "selected provider or pinned model authority changed or is unavailable",
            ));
        }
        let model = &self.selected.settings.model;
        for (config, scope, same_origin) in &self.policies {
            let policy = current_policy_in_scope(config, scope)
                .map_err(|_| invalid("current operation policy cannot be established"))?;
            let legacy =
                *same_origin && self.origin.provider_id() == self.selected.config.model_provider_id;
            let resolved = policy
                .resolve(codex_config::child_provider::ChildProviderRequest {
                    explicit_provider: Some(&self.selected.config.model_provider_id),
                    role_provider: None,
                    parent_provider_id: if legacy {
                        &self.ceiling.config.model_provider_id
                    } else {
                        &config.model_provider_id
                    },
                    parent_provider: if legacy {
                        &self.ceiling.config.model_provider
                    } else {
                        &config.model_provider
                    },
                    parent_model: model,
                    authorization_origin: same_origin.then(|| self.origin.clone()),
                    explicit_model: Some(model),
                    role_model: None,
                    default_subagent_model: None,
                    full_history: false,
                    host_owned: false,
                })
                .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
            if resolved.provider != self.selected.config.model_provider || resolved.model != *model
            {
                return Err(invalid(
                    "current policy changed the selected provider or model",
                ));
            }
            // Resolve the retained catalog binding without loading a catalog or
            // invoking a credential helper. Metadata stays pinned to this allocation.
            let retained = self
                .ceiling
                .config
                .child_provider_policy
                .resolve(codex_config::child_provider::ChildProviderRequest {
                    explicit_provider: Some(&self.selected.config.model_provider_id),
                    role_provider: None,
                    parent_provider_id: &self.ceiling.config.model_provider_id,
                    parent_provider: &self.ceiling.config.model_provider,
                    parent_model: model,
                    authorization_origin: Some(self.origin.clone()),
                    explicit_model: Some(model),
                    role_model: None,
                    default_subagent_model: None,
                    full_history: false,
                    host_owned: false,
                })
                .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
            if resolved.model_catalog_json != retained.model_catalog_json {
                return Err(invalid("current selected catalog binding changed"));
            }
            if let Some(path) = resolved.model_catalog_json.as_ref() {
                // Match model-provider/models_endpoint.rs's existing 1 MiB
                // catalog bound. This is a host file comparison, not a cache
                // refresh; perform it before acquiring admission locks.
                use std::io::Read;
                const MAX_MODEL_CATALOG_BYTES: u64 = 1024 * 1024;
                let file = std::fs::File::open(path)
                    .map_err(|_| invalid("selected host catalog is unavailable"))?;
                let mut bytes = Vec::new();
                file.take(MAX_MODEL_CATALOG_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| invalid("selected host catalog cannot be checked"))?;
                if bytes.len() as u64 > MAX_MODEL_CATALOG_BYTES {
                    return Err(invalid(
                        "selected host catalog exceeds the catalog size limit",
                    ));
                }
                let catalog: codex_protocol::openai_models::ModelsResponse =
                    serde_json::from_slice(&bytes)
                        .map_err(|_| invalid("selected host catalog is invalid"))?;
                if self.ceiling.config.model_catalog.as_ref() != Some(&catalog) {
                    return Err(invalid("selected host catalog metadata changed"));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn with_current<T>(&self, admit: impl FnOnce() -> Result<T>) -> Result<T> {
        // File trust and resolution run outside all admission locks.
        self.validate_policy()?;
        #[cfg(test)]
        if let Some(barrier) = &self.after_policy_validation {
            barrier();
        }
        if self
            .target
            .session
            .services
            .local_agent_runtime
            .shutdown
            .is_cancelled()
        {
            return Err(invalid("operation runtime is shutting down"));
        }
        let mut revisions = Vec::new();
        for (owner, expected) in &self.owners {
            let revision = owner
                .session
                .child_policy_revision
                .lock()
                .map_err(|_| invalid("current policy publication fence is unavailable"))?;
            if *revision != *expected {
                return Err(invalid("current policy changed before operation admission"));
            }
            revisions.push(revision);
        }
        let runtime = &self.target.session.services.local_agent_runtime;
        let callers = self
            .owners
            .iter()
            .map(|(owner, _)| owner.session.as_ref())
            .collect::<Vec<_>>();
        runtime.with_current_native_operation(
            &self.root.session,
            &self.target.session,
            &callers,
            || {
                let bindings = self
                    .policies
                    .iter()
                    .filter_map(|(config, _, _)| config.config_layer_stack.cloud_config_binding())
                    .collect::<Vec<_>>();
                codex_config::CloudConfigBundleBinding::with_current_bindings(&bindings, admit)
                    .ok_or_else(|| invalid("managed operation policy changed before admission"))?
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn set_after_policy_validation(&mut self, barrier: Arc<dyn Fn() + Send + Sync>) {
        self.after_policy_validation = Some(barrier);
    }

    pub(crate) fn matches_target(&self, thread: &Arc<CodexThread>) -> bool {
        Arc::ptr_eq(thread, &self.target)
    }

    pub(crate) fn target(&self) -> &Arc<CodexThread> {
        &self.target
    }

    pub(crate) fn check(&self) -> Result<()> {
        self.with_current(|| Ok(()))
    }
}

pub(crate) async fn prepare_inference(
    session: &Arc<Session>,
    step: &crate::session::step_context::StepContext,
) -> Result<Option<CurrentChildOperation>> {
    let runtime = &session.services.local_agent_runtime;
    // A retained registration supplies the source classification. Synthetic
    // sessions with no runtime registration keep their existing root behavior.
    if !runtime
        .retained_authority(session.thread_id)
        .is_some_and(|authority| {
            authority.matches_session(session)
                && matches!(
                    authority.source,
                    SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
                )
        })
    {
        if matches!(
            step.turn.session_source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        ) {
            return Err(invalid("native inference registration is unavailable"));
        }
        return Ok(None);
    }
    let manager = runtime.upgrade()?;
    let target = manager.get_thread(session.thread_id).await?;
    if !Arc::ptr_eq(&target.session, session) {
        return Err(invalid(
            "inference session is no longer the registered allocation",
        ));
    }
    if step.settings.model_info.slug
        != target
            .config_snapshot()
            .await
            .model
            .as_deref()
            .unwrap_or_default()
    {
        return Err(invalid(
            "inference model differs from the selected runtime model",
        ));
    }
    CurrentChildOperation::prepare(
        target,
        None,
        Some(ChildProviderScope::from_step(
            &step.turn.config,
            &step.environments,
        )),
    )
    .await
}

pub(crate) async fn prepare_loaded(
    runtime: &LocalAgentRuntime,
    target: ThreadId,
    caller: ThreadId,
) -> Result<Option<CurrentChildOperation>> {
    let manager = runtime.upgrade()?;
    CurrentChildOperation::prepare(
        manager.get_thread(target).await?,
        Some(manager.get_thread(caller).await?),
        None,
    )
    .await
}
