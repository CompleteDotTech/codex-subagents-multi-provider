//! Host-only, in-memory origin and single-use native launch authority.
//!
//! Issue15 adds durable records and reload verification. A model-visible config,
//! source or UUID never creates an origin or a launch permit here.

use crate::agent::control::LocalAgentRuntime;
use crate::config::Config;
use crate::session::session::Session;
use codex_config::child_provider::ChildProviderOriginBinding;
use codex_config::child_provider::ChildProviderOriginRecord;
use codex_config::child_provider::ChildProviderPolicy;
use codex_config::child_provider::VerifiedChildProviderOrigin;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSettingsSnapshot;
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

/// Refresh provenance against exactly the effective runtime filesystem scope.
pub(crate) fn current_policy(config: &Config) -> std::io::Result<ChildProviderPolicy> {
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
    let filesystem = config.permissions.file_system_sandbox_policy();
    let writable = filesystem.get_writable_roots_with_cwd(config.cwd.as_path());
    let mut roots = config.workspace_roots.clone();
    roots.extend(writable.iter().map(|root| root.root.clone()));
    roots.sort();
    roots.dedup();
    let protected = writable
        .into_iter()
        .flat_map(|root| root.read_only_subpaths)
        .collect::<Vec<_>>();
    ChildProviderPolicy::from_config_layers(
        &config.config_layer_stack,
        &config.cwd,
        &roots,
        &protected,
        &filesystem,
    )
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
        if !Arc::ptr_eq(
            &self.origins,
            &parent
                .session
                .services
                .local_agent_runtime
                .child_provider_origins,
        ) || parent.thread_settings_snapshot().await != self.parent_settings
            || parent.session.get_config().await.as_ref() != self.parent_base.as_ref()
            || self.origins.verified(caller) != self.origin
        {
            return Err(invalid("parent authority changed after child preparation"));
        }
        let policy = current_policy(config)
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
            caller,
            origins: self.origins,
            origin: self.origin,
            approved: self.approved,
        })
    }
}

/// Non-cloneable proof consumed by the lower native thread creation boundary.
pub(crate) struct ChildLaunchPermit {
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
    ) -> Result<Option<VerifiedChildProviderOrigin>> {
        if parent != Some(self.caller)
            || !Arc::ptr_eq(&self.origins, &runtime.child_provider_origins)
            || self.approved.as_ref() != config
        {
            return Err(invalid(
                "native child launch authority does not match its configuration",
            ));
        }
        Ok(self.origin)
    }
}

pub(crate) fn invalid(message: &str) -> CodexErr {
    CodexErr::InvalidRequest(format!("child_provider_policy_conflict: {message}"))
}

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
