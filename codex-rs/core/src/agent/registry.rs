use crate::agent::child_provider_authorization::RetainedAgentAuthority;
use crate::agent::types::AgentMetadata;
use crate::environment_selection::TurnEnvironmentSnapshot;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::AgentErrorContext;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnEnvironmentSelection;
use rand::prelude::IndexedRandom;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// This structure is used to add some limits on the multi-agent capabilities for Codex. In
/// the current implementation, it limits:
/// * Total number of sub-agents (i.e. threads) per user session
///
/// This structure is shared by all agents in the same user session (because the `LocalAgentControl`
/// is).
#[derive(Default)]
pub(crate) struct AgentRegistry {
    active_agents: Mutex<ActiveAgents>,
    total_count: AtomicUsize,
    #[cfg(test)]
    graph_test_pauses: Mutex<[Option<Arc<GraphLifecyclePause>>; 3]>,
}

#[derive(Default)]
struct ActiveAgents {
    agent_tree: HashMap<String, AgentMetadata>,
    thread_paths: HashMap<ThreadId, RegisteredAgent>,
    // Actual finalized/unloaded Session ceilings only; never serialized, printed,
    // or sufficient by themselves to authorize provider use.
    retained_authority: HashMap<ThreadId, RetainedAuthorityEntry>,
    // Same canonical tombstone map: revocations advance revision; only a NEW
    // actual registration changes generation. Neither disappears with residency.
    lifecycles: HashMap<ThreadId, CanonicalRegistrationLifecycle>,
    used_agent_nicknames: HashSet<String>,
    nickname_reset_count: usize,
}

#[derive(Default)]
struct CanonicalRegistrationLifecycle {
    teardown_revision: u64,
    registration_generation: Option<uuid::Uuid>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CallerPhase {
    Current,
    ResidencyEvictionInProgress,
    CompletedResidencyEviction,
    Revoked,
}

struct RetainedAuthorityEntry {
    authority: Arc<RetainedAgentAuthority>,
    // Captured only on NEW registration; same-registration refresh/eviction
    // cannot attach a descendant to a replacement owner with the same ID.
    parent_registration: Option<uuid::Uuid>,
    phase: CallerPhase,
}

/// Thread defaults and executor bindings retained while a child's runtime is unloaded.
#[derive(Clone)]
pub(crate) struct EvictedEnvironments {
    pub(crate) selections: Vec<TurnEnvironmentSelection>,
    pub(crate) bindings: TurnEnvironmentSnapshot,
}

struct RegisteredAgent {
    path: String,
    evicted_environments: Option<EvictedEnvironments>,
}

impl RegisteredAgent {
    fn new(path: String) -> Self {
        Self {
            path,
            evicted_environments: None,
        }
    }
}

fn format_agent_nickname(name: &str, nickname_reset_count: usize) -> String {
    match nickname_reset_count {
        0 => name.to_string(),
        reset_count => {
            let value = reset_count + 1;
            let suffix = match value % 100 {
                11..=13 => "th",
                _ => match value % 10 {
                    1 => "st", // codespell:ignore
                    2 => "nd", // codespell:ignore
                    3 => "rd", // codespell:ignore
                    _ => "th", // codespell:ignore
                },
            };
            format!("{name} the {value}{suffix}")
        }
    }
}

fn session_depth(session_source: &SessionSource) -> i32 {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) => *depth,
        SessionSource::SubAgent(_) => 0,
        _ => 0,
    }
}

pub(crate) fn next_thread_spawn_depth(session_source: &SessionSource) -> i32 {
    session_depth(session_source).saturating_add(1)
}

pub(crate) fn exceeds_thread_spawn_depth_limit(depth: i32, max_depth: i32) -> bool {
    depth > max_depth
}

/// Explicit graph intent tied to the SAME canonical registration observed at
/// revoke, not to the revocation revision. Same-registration teardown transfers
/// this idempotent Closed obligation; only a new actual registration invalidates
/// it. This is graph ordering state, never provider authority or a receipt.
pub(crate) struct NativeGraphCloseStamp {
    root: ThreadId,
    registration_generation: Option<uuid::Uuid>,
    retained_registration: Option<uuid::Uuid>,
    explicit_intent: bool,
}

impl NativeGraphCloseStamp {
    /// A missing identity can fence an originally unloaded observation, but it
    /// cannot prove that a removed loaded allocation was this registration.
    pub(crate) fn can_transfer_from_loaded(&self) -> bool {
        self.registration_generation.is_some()
            && self.registration_generation == self.retained_registration
    }
}
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum GraphLifecycleTestPoint {
    BeforeExplicitCapture = 0,
    AfterExplicitCapture = 1,
    AfterOrdinaryCapture = 2,
}
#[cfg(test)]
#[derive(Default)]
pub(crate) struct GraphLifecyclePause {
    pub(crate) entered: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
}

impl AgentRegistry {
    /// Controlled missing-retained graph fixture only. Keep the canonical
    /// registration tombstone, metadata, and every other authority untouched.
    #[cfg(test)]
    pub(crate) fn remove_retained_authority_for_graph_fixture(&self, thread: ThreadId) {
        let mut state = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = state.retained_authority.remove(&thread).unwrap();
        assert!(entry.phase == CallerPhase::CompletedResidencyEviction);
        assert_eq!(
            state
                .lifecycles
                .get(&thread)
                .unwrap()
                .registration_generation,
            Some(entry.authority.registration_id())
        );
    }

    #[cfg(test)]
    pub(crate) fn pause_graph_at(
        &self,
        point: GraphLifecycleTestPoint,
    ) -> Arc<GraphLifecyclePause> {
        let pause = Arc::new(GraphLifecyclePause::default());
        self.graph_test_pauses.lock().unwrap()[point as usize] = Some(pause.clone());
        pause
    }
    #[cfg(test)]
    pub(crate) async fn graph_test_pause(&self, point: GraphLifecycleTestPoint) {
        let pause = self.graph_test_pauses.lock().unwrap()[point as usize].take();
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
    }
    pub(crate) fn teardown_revision(&self, thread: ThreadId) -> u64 {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lifecycles
            .get(&thread)
            .map(|lifecycle| lifecycle.teardown_revision)
            .unwrap_or(0)
    }

    /// Called with the manager map write guard held. Ownership edges remain
    /// authoritative after residency eviction and explicit close.
    pub(crate) fn revoke_subtree(
        &self,
        root: ThreadId,
        edges: impl IntoIterator<Item = (ThreadId, ThreadId)>,
        explicit_intent: bool,
    ) -> (HashSet<ThreadId>, NativeGraphCloseStamp) {
        let mut state = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut edges: Vec<_> = edges.into_iter().collect();
        edges.extend(state.retained_authority.iter().filter_map(|(id, entry)| {
            entry
                .authority
                .source
                .parent_thread_id()
                .map(|parent| (*id, parent))
        }));
        let mut closed = HashSet::from([root]);
        loop {
            let before = closed.len();
            for (child, parent) in &edges {
                if closed.contains(parent) {
                    closed.insert(*child);
                }
            }
            if before == closed.len() {
                break;
            }
        }
        for id in &closed {
            state.lifecycles.entry(*id).or_default().teardown_revision += 1;
            if let Some(entry) = state.retained_authority.get_mut(id) {
                entry.phase = CallerPhase::Revoked;
            }
            if let Some(agent) = state.thread_paths.get_mut(id) {
                agent.evicted_environments = None;
            }
        }
        let stamp = NativeGraphCloseStamp {
            root,
            registration_generation: state
                .lifecycles
                .get(&root)
                .and_then(|lifecycle| lifecycle.registration_generation),
            retained_registration: state
                .retained_authority
                .get(&root)
                .map(|entry| entry.authority.registration_id()),
            explicit_intent,
        };
        (closed, stamp)
    }

    /// The caller signal and manager map are held by the private startup owner.
    /// No callback may reenter this registry or await.
    pub(crate) fn commit_native_registration<T, G>(
        &self,
        caller: &RetainedAgentAuthority,
        live: bool,
        allow_completed_eviction: bool,
        revisions: &[(ThreadId, u64)],
        authority: Arc<RetainedAgentAuthority>,
        commit: impl FnOnce() -> Result<(T, G)>,
    ) -> Result<T> {
        let mut state = self.active_agents.lock().map_err(|_| {
            crate::agent::child_provider_authorization::invalid(
                "native registration registry is unavailable",
            )
        })?;
        let valid = state
            .retained_authority
            .get(&caller.thread_id)
            .is_some_and(|entry| {
                entry.authority.same_registration(caller)
                    && match entry.phase {
                        CallerPhase::Current => live,
                        CallerPhase::CompletedResidencyEviction => allow_completed_eviction,
                        CallerPhase::Revoked | CallerPhase::ResidencyEvictionInProgress => false,
                    }
            });
        if !valid
            || revisions.iter().any(|(id, revision)| {
                state
                    .lifecycles
                    .get(id)
                    .map(|lifecycle| lifecycle.teardown_revision)
                    .unwrap_or(0)
                    != *revision
            })
        {
            return Err(crate::agent::child_provider_authorization::invalid(
                "native caller or target was closed or replaced during startup",
            ));
        }
        let (result, policy_guard) = commit()?;
        state
            .lifecycles
            .entry(authority.thread_id)
            .or_default()
            .teardown_revision += 1;
        state
            .lifecycles
            .entry(authority.thread_id)
            .or_default()
            .registration_generation = Some(authority.registration_id());
        let parent_registration = authority.source.parent_thread_id().and_then(|parent| {
            state
                .retained_authority
                .get(&parent)
                .map(|entry| entry.authority.registration_id())
        });
        state.retained_authority.insert(
            authority.thread_id,
            RetainedAuthorityEntry {
                authority,
                parent_registration,
                phase: CallerPhase::Current,
            },
        );
        drop(policy_guard);
        Ok(result)
    }
    /// Only successful actual registration installs a NEW Current epoch.
    /// Same-registration snapshot refresh preserves eviction/revocation phase.
    pub(crate) fn register_authority(&self, authority: Arc<RetainedAgentAuthority>) {
        let mut state = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let thread_id = authority.thread_id;
        if let Some(entry) = state.retained_authority.get_mut(&thread_id)
            && entry.authority.same_registration(&authority)
        {
            entry.authority = authority;
            return;
        }
        state
            .lifecycles
            .entry(thread_id)
            .or_default()
            .teardown_revision += 1;
        state
            .lifecycles
            .entry(thread_id)
            .or_default()
            .registration_generation = Some(authority.registration_id());
        let parent_registration = authority.source.parent_thread_id().and_then(|parent| {
            state
                .retained_authority
                .get(&parent)
                .map(|entry| entry.authority.registration_id())
        });
        state.retained_authority.insert(
            thread_id,
            RetainedAuthorityEntry {
                authority,
                parent_registration,
                phase: CallerPhase::Current,
            },
        );
        if let Some(agent) = state.thread_paths.get_mut(&thread_id) {
            agent.evicted_environments = None;
        }
    }

    pub(crate) fn retire_authority(&self, authority: Arc<RetainedAgentAuthority>) {
        let mut state = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = state.retained_authority.get_mut(&authority.thread_id)
            && entry.authority.same_registration(&authority)
        {
            entry.authority = authority;
            entry.phase = CallerPhase::Revoked;
        }
    }

    pub(crate) fn begin_residency_eviction(&self, authority: &RetainedAgentAuthority) -> bool {
        let mut state = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = state
            .retained_authority
            .get_mut(&authority.thread_id)
            .filter(|entry| {
                entry.authority.same_registration(authority) && entry.phase == CallerPhase::Current
            })
        else {
            return false;
        };
        entry.phase = CallerPhase::ResidencyEvictionInProgress;
        true
    }

    /// A real explicit Session stop changes target lifetime before its first
    /// shutdown await. The private eviction owner alone preserves its revision.
    pub(crate) fn note_session_stopped(&self, session: &crate::session::session::Session) {
        let mut state = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = state
            .retained_authority
            .get_mut(&session.thread_id)
            .filter(|entry| entry.authority.matches_session(session))
        else {
            return;
        };
        if entry.phase == CallerPhase::Current {
            entry.phase = CallerPhase::Revoked;
            state
                .lifecycles
                .entry(session.thread_id)
                .or_default()
                .teardown_revision += 1;
        }
    }

    /// Explicit loaded or unloaded close sets a tombstone before its first await.
    pub(crate) fn revoke_caller(&self, thread_id: ThreadId) {
        let _ = self.revoke_caller_stamped(thread_id, /*explicit_intent*/ false);
    }

    pub(crate) fn revoke_caller_stamped(
        &self,
        thread_id: ThreadId,
        explicit_intent: bool,
    ) -> NativeGraphCloseStamp {
        let mut state = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .lifecycles
            .entry(thread_id)
            .or_default()
            .teardown_revision += 1;
        if let Some(entry) = state.retained_authority.get_mut(&thread_id) {
            entry.phase = CallerPhase::Revoked;
        }
        if let Some(agent) = state.thread_paths.get_mut(&thread_id) {
            agent.evicted_environments = None;
        }
        NativeGraphCloseStamp {
            root: thread_id,
            registration_generation: state
                .lifecycles
                .get(&thread_id)
                .and_then(|lifecycle| lifecycle.registration_generation),
            retained_registration: state
                .retained_authority
                .get(&thread_id)
                .map(|entry| entry.authority.registration_id()),
            explicit_intent,
        }
    }

    /// Map membership is held by the caller; release this short registry read
    /// before any graph await. A revoked SAME allocation cannot publish Open.
    pub(crate) fn graph_open_is_current(&self, session: &crate::session::session::Session) -> bool {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retained_authority
            .get(&session.thread_id)
            .is_some_and(|entry| {
                entry.phase == CallerPhase::Current && entry.authority.matches_session(session)
            })
    }

    /// Even absent->absent must match the canonical registration tombstone. A
    /// new registration followed by eviction cannot revive an old Closed write.
    pub(crate) fn graph_close_is_current(&self, stamp: &NativeGraphCloseStamp) -> bool {
        let state = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stamp.explicit_intent
            && state
                .lifecycles
                .get(&stamp.root)
                .and_then(|lifecycle| lifecycle.registration_generation)
                == stamp.registration_generation
            && state
                .retained_authority
                .get(&stamp.root)
                .map(|entry| entry.authority.registration_id())
                == stamp.retained_registration
    }
    /// Called only after exact successful map removal. CAS cannot undo a close
    /// tombstone or attach a stale completion to a replacement registration.
    pub(crate) fn complete_residency_eviction(
        &self,
        authority: Arc<RetainedAgentAuthority>,
        environments: EvictedEnvironments,
    ) {
        let mut state = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let thread_id = authority.thread_id;
        let Some(entry) = state.retained_authority.get_mut(&thread_id) else {
            return;
        };
        if !entry.authority.same_registration(&authority)
            || entry.phase != CallerPhase::ResidencyEvictionInProgress
        {
            return;
        }
        entry.authority = authority;
        entry.phase = CallerPhase::CompletedResidencyEviction;
        if let Some(agent) = state.thread_paths.get_mut(&thread_id) {
            agent.evicted_environments = Some(environments);
        }
    }

    /// Fresh grants require Current; completed residency eviction is exclusively
    /// a private-resume exception. Keep revocation ordered through the callback.
    pub(crate) fn with_fresh_caller<T>(
        &self,
        caller: &crate::session::session::Session,
        registration_id: uuid::Uuid,
        admit: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let state = self.active_agents.lock().map_err(|_| {
            crate::agent::child_provider_authorization::invalid(
                "fresh child caller registry is unavailable",
            )
        })?;
        let valid = state
            .retained_authority
            .get(&caller.thread_id)
            .is_some_and(|entry| {
                entry.phase == CallerPhase::Current
                    && entry.authority.registration_id() == registration_id
                    && entry.authority.matches_session(caller)
            });
        if !valid {
            return Err(crate::agent::child_provider_authorization::invalid(
                "fresh child caller was revoked or changed before startup admission",
            ));
        }
        // No await, map lookup, or registry reentry in this callback.
        let result = admit();
        drop(state);
        result
    }

    /// The callback must not reenter this registry. Hold this lock through the
    /// final synchronous decision so replacement and revocation cannot race it.
    pub(crate) fn with_resume_caller<T>(
        &self,
        authority: &RetainedAgentAuthority,
        live: bool,
        admit: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let state = self.active_agents.lock().map_err(|_| {
            crate::agent::child_provider_authorization::invalid(
                "private resume caller registry is unavailable",
            )
        })?;
        let valid = state
            .retained_authority
            .get(&authority.thread_id)
            .is_some_and(|entry| {
                entry.authority.same_registration(authority)
                    && match entry.phase {
                        CallerPhase::Current => live,
                        CallerPhase::CompletedResidencyEviction => true,
                        CallerPhase::Revoked | CallerPhase::ResidencyEvictionInProgress => false,
                    }
            });
        if !valid {
            return Err(crate::agent::child_provider_authorization::invalid(
                "private resume caller stopped or changed before startup admission",
            ));
        }
        let result = admit();
        drop(state);
        result
    }

    /// One atomic decision over the canonical retained lineage. The callback is
    /// synchronous and must not poll a future, do I/O, or reenter the registry.
    pub(crate) fn with_current_native_operation<T>(
        &self,
        root: &crate::session::session::Session,
        target: &crate::session::session::Session,
        callers: &[&crate::session::session::Session],
        admit: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let state = self.active_agents.lock().map_err(|_| {
            crate::agent::child_provider_authorization::invalid("operation registry is unavailable")
        })?;
        for session in std::iter::once(root)
            .chain(std::iter::once(target))
            .chain(callers.iter().copied())
        {
            if !state
                .retained_authority
                .get(&session.thread_id)
                .is_some_and(|entry| {
                    entry.phase == CallerPhase::Current && entry.authority.matches_session(session)
                })
            {
                return Err(crate::agent::child_provider_authorization::invalid(
                    "operation registration was revoked or replaced",
                ));
            }
        }
        let mut cursor = target.thread_id;
        let mut visited = HashSet::new();
        while cursor != root.thread_id {
            if !visited.insert(cursor) {
                return Err(crate::agent::child_provider_authorization::invalid(
                    "operation lineage contains a cycle",
                ));
            }
            let entry = state.retained_authority.get(&cursor).ok_or_else(|| {
                crate::agent::child_provider_authorization::invalid(
                    "operation lineage is unavailable",
                )
            })?;
            if !matches!(
                entry.phase,
                CallerPhase::Current | CallerPhase::CompletedResidencyEviction
            ) {
                return Err(crate::agent::child_provider_authorization::invalid(
                    "operation lineage was revoked",
                ));
            }
            let parent = entry.authority.source.parent_thread_id().ok_or_else(|| {
                crate::agent::child_provider_authorization::invalid(
                    "operation lineage has another root",
                )
            })?;
            if !entry.parent_registration.is_some_and(|expected| {
                state
                    .retained_authority
                    .get(&parent)
                    .is_some_and(|parent| parent.authority.registration_id() == expected)
            }) {
                return Err(crate::agent::child_provider_authorization::invalid(
                    "operation lineage owner registration changed",
                ));
            }
            cursor = parent;
        }
        admit()
    }

    pub(crate) fn retained_authority(
        &self,
        thread_id: ThreadId,
    ) -> Option<Arc<RetainedAgentAuthority>> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retained_authority
            .get(&thread_id)
            .map(|entry| Arc::clone(&entry.authority))
    }

    pub(crate) fn reserve_spawn_slot(
        self: &Arc<Self>,
        max_threads: Option<usize>,
    ) -> Result<SpawnReservation> {
        if let Some(max_threads) = max_threads {
            if !self.try_increment_spawned(max_threads) {
                return Err(
                    CodexErr::new(CodexErrorDetails::AgentLimitReached { max_threads })
                        .with_agent_context(AgentErrorContext::RegistryCapacity),
                );
            }
        } else {
            self.total_count.fetch_add(1, Ordering::AcqRel);
        }
        Ok(SpawnReservation {
            state: Arc::clone(self),
            active: true,
            reserved_agent_nickname: None,
            reserved_agent_path: None,
        })
    }

    pub(crate) fn release_spawned_thread(&self, thread_id: ThreadId) {
        let removed_counted_agent = {
            let mut active_agents = self
                .active_agents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active_agents
                .thread_paths
                .remove(&thread_id)
                .and_then(|agent| active_agents.agent_tree.remove(agent.path.as_str()))
                .is_some_and(|metadata| {
                    !metadata.agent_path.as_ref().is_some_and(AgentPath::is_root)
                })
        };
        if removed_counted_agent {
            self.total_count.fetch_sub(1, Ordering::AcqRel);
        }
    }

    pub(crate) fn register_root_thread(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root_path = AgentPath::ROOT.to_string();
        let root_thread_id = active_agents
            .agent_tree
            .entry(root_path.clone())
            .or_insert_with(|| AgentMetadata {
                agent_id: Some(thread_id),
                agent_path: Some(AgentPath::root()),
                ..Default::default()
            })
            .agent_id;
        if let Some(root_thread_id) = root_thread_id {
            active_agents
                .thread_paths
                .insert(root_thread_id, RegisteredAgent::new(root_path));
        }
    }

    pub(crate) fn agent_id_for_path(&self, agent_path: &AgentPath) -> Option<ThreadId> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .get(agent_path.as_str())
            .and_then(|metadata| metadata.agent_id)
    }

    pub(crate) fn agent_metadata_for_thread(&self, thread_id: ThreadId) -> Option<AgentMetadata> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .thread_paths
            .get(&thread_id)
            .and_then(|agent| active_agents.agent_tree.get(&agent.path))
            .cloned()
    }

    pub(crate) fn evicted_environments(&self, thread_id: ThreadId) -> Option<EvictedEnvironments> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .thread_paths
            .get(&thread_id)
            .and_then(|agent| agent.evicted_environments.clone())
    }

    pub(crate) fn clear_evicted_environments(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(agent) = active_agents.thread_paths.get_mut(&thread_id) {
            agent.evicted_environments = None;
        }
    }

    pub(crate) fn live_agents(&self) -> Vec<AgentMetadata> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .values()
            .filter(|metadata| {
                metadata.agent_id.is_some()
                    && !metadata.agent_path.as_ref().is_some_and(AgentPath::is_root)
            })
            .cloned()
            .collect()
    }

    fn register_spawned_thread(&self, agent_metadata: AgentMetadata) {
        let mut state = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::register_spawned_thread_locked(&mut state, agent_metadata);
    }

    fn register_spawned_thread_locked(
        active_agents: &mut ActiveAgents,
        agent_metadata: AgentMetadata,
    ) {
        let Some(thread_id) = agent_metadata.agent_id else {
            return;
        };
        let key = agent_metadata
            .agent_path
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("thread:{thread_id}"));
        if let Some(agent_nickname) = agent_metadata.agent_nickname.clone() {
            active_agents.used_agent_nicknames.insert(agent_nickname);
        }
        if let Some(previous_agent) = active_agents
            .thread_paths
            .insert(thread_id, RegisteredAgent::new(key.clone()))
            && previous_agent.path != key
        {
            active_agents
                .agent_tree
                .remove(previous_agent.path.as_str());
        }
        if let Some(previous_metadata) = active_agents.agent_tree.insert(key, agent_metadata)
            && let Some(previous_thread_id) = previous_metadata.agent_id
            && previous_thread_id != thread_id
        {
            active_agents.thread_paths.remove(&previous_thread_id);
        }
    }

    fn reserve_agent_nickname(&self, names: &[&str], preferred: Option<&str>) -> Option<String> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let agent_nickname = if let Some(preferred) = preferred {
            preferred.to_string()
        } else {
            if names.is_empty() {
                return None;
            }
            let available_names: Vec<String> = names
                .iter()
                .map(|name| format_agent_nickname(name, active_agents.nickname_reset_count))
                .filter(|name| !active_agents.used_agent_nicknames.contains(name))
                .collect();
            if let Some(name) = available_names.choose(&mut rand::rng()) {
                name.clone()
            } else {
                active_agents.used_agent_nicknames.clear();
                active_agents.nickname_reset_count += 1;
                if let Some(metrics) = codex_otel::global() {
                    let _ = metrics.counter(
                        "codex.multi_agent.nickname_pool_reset",
                        /*inc*/ 1,
                        &[],
                    );
                }
                format_agent_nickname(
                    names.choose(&mut rand::rng())?,
                    active_agents.nickname_reset_count,
                )
            }
        };
        active_agents
            .used_agent_nicknames
            .insert(agent_nickname.clone());
        Some(agent_nickname)
    }

    fn reserve_agent_path(&self, agent_path: &AgentPath) -> Result<()> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match active_agents.agent_tree.entry(agent_path.to_string()) {
            Entry::Occupied(_) => Err(CodexErr::UnsupportedOperation(format!(
                "agent path `{agent_path}` already exists"
            ))
            .with_agent_context(AgentErrorContext::DuplicatePath)),
            Entry::Vacant(entry) => {
                entry.insert(AgentMetadata {
                    agent_path: Some(agent_path.clone()),
                    ..Default::default()
                });
                Ok(())
            }
        }
    }

    fn release_reserved_agent_path(&self, agent_path: &AgentPath) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active_agents
            .agent_tree
            .get(agent_path.as_str())
            .is_some_and(|metadata| metadata.agent_id.is_none())
        {
            active_agents.agent_tree.remove(agent_path.as_str());
        }
    }

    fn try_increment_spawned(&self, max_threads: usize) -> bool {
        let mut current = self.total_count.load(Ordering::Acquire);
        loop {
            if current >= max_threads {
                return false;
            }
            match self.total_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(updated) => current = updated,
            }
        }
    }
}

pub(crate) struct SpawnReservation {
    state: Arc<AgentRegistry>,
    active: bool,
    reserved_agent_nickname: Option<String>,
    reserved_agent_path: Option<AgentPath>,
}

impl SpawnReservation {
    pub(crate) fn reserve_agent_nickname_with_preference(
        &mut self,
        names: &[&str],
        preferred: Option<&str>,
    ) -> Result<String> {
        let agent_nickname = self
            .state
            .reserve_agent_nickname(names, preferred)
            .ok_or_else(|| {
                CodexErr::UnsupportedOperation("no available agent nicknames".to_string())
                    .with_agent_context(AgentErrorContext::NicknameUnavailable)
            })?;
        self.reserved_agent_nickname = Some(agent_nickname.clone());
        Ok(agent_nickname)
    }

    pub(crate) fn reserve_agent_path(&mut self, agent_path: &AgentPath) -> Result<()> {
        self.state.reserve_agent_path(agent_path)?;
        self.reserved_agent_path = Some(agent_path.clone());
        Ok(())
    }

    pub(crate) fn commit_if_current(
        mut self,
        agent_metadata: AgentMetadata,
        authority: &RetainedAgentAuthority,
    ) -> Result<()> {
        let mut state = self
            .state
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if agent_metadata.agent_id != Some(authority.thread_id)
            || !state
                .retained_authority
                .get(&authority.thread_id)
                .is_some_and(|entry| {
                    entry.phase == CallerPhase::Current
                        && entry.authority.same_registration(authority)
                })
        {
            return Err(crate::agent::child_provider_authorization::invalid(
                "spawned runtime was closed or replaced before metadata commit",
            ));
        }
        AgentRegistry::register_spawned_thread_locked(&mut state, agent_metadata);
        self.reserved_agent_nickname = None;
        self.reserved_agent_path = None;
        self.active = false;
        Ok(())
    }

    pub(crate) fn commit(mut self, agent_metadata: AgentMetadata) {
        self.reserved_agent_nickname = None;
        self.reserved_agent_path = None;
        self.state.register_spawned_thread(agent_metadata);
        self.active = false;
    }
}

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        if self.active {
            if let Some(agent_path) = self.reserved_agent_path.take() {
                self.state.release_reserved_agent_path(&agent_path);
            }
            self.state.total_count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
