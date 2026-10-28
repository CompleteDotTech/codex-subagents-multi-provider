use super::*;
use crate::CodexThread;
use crate::agent::api::AgentInfo;
use codex_protocol::error::CodexErrorDetails;
use codex_thread_store::PersistContext;

impl LocalAgentControl {
    /// Submit a shutdown request for a live agent without marking it explicitly closed in
    /// persisted spawn-edge state.
    #[cfg(test)]
    pub(crate) async fn shutdown_live_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        self.runtime.registry.revoke_caller(agent_id);
        let state = self.runtime.upgrade()?;
        let mut captured = self.capture_shutdown(&state, agent_id, false).await;
        let (_, thread) = captured.pop().ok_or(CodexErr::ThreadNotFound(agent_id))?;
        self.shutdown_captured_agent(&state, agent_id, thread).await
    }

    async fn shutdown_captured_agent(
        &self,
        state: &ThreadManagerState,
        agent_id: ThreadId,
        thread: Arc<CodexThread>,
    ) -> CodexResult<String> {
        thread
            .session
            .ensure_rollout_materialized(PersistContext::Standard)
            .await;
        let flush_result = thread.session.flush_rollout().await;
        let result = if matches!(thread.agent_status().await, AgentStatus::Shutdown) {
            Ok(String::new())
        } else {
            thread.submit(Op::Shutdown {}).await
        };
        thread.wait_until_terminated().await;
        state
            .remove_thread_if_matches(&agent_id, &thread, || {
                self.forget_v2_residency(agent_id);
                self.runtime.registry.release_spawned_thread(agent_id);
                self.runtime.mailboxes.remove(agent_id);
            })
            .await;
        flush_result?;
        result
    }
    /// Mark `agent_id` as explicitly closed in persisted spawn-edge state, then shut down the
    /// agent and any live descendants reached from the in-memory tree.
    pub(crate) async fn close_agent(&self, agent_id: ThreadId) -> CodexResult<AgentInfo> {
        self.runtime.registry.revoke_caller(agent_id);
        #[cfg(test)]
        self.runtime
            .registry
            .graph_test_pause(
                crate::agent::registry::GraphLifecycleTestPoint::BeforeExplicitCapture,
            )
            .await;
        let state = self.runtime.upgrade()?;
        let metadata = self.get_agent_metadata(agent_id);
        let known_agent = metadata.is_some();
        let captured = self
            .capture_shutdown_with_graph_stamp(&state, agent_id, true)
            .await;
        #[cfg(test)]
        self.runtime
            .registry
            .graph_test_pause(crate::agent::registry::GraphLifecycleTestPoint::AfterExplicitCapture)
            .await;
        let snapshot = match captured
            .threads
            .iter()
            .find(|(id, _)| *id == agent_id)
            .map(|(_, thread)| Arc::clone(thread))
            .ok_or(CodexErr::ThreadNotFound(agent_id))
        {
            Ok(thread) => {
                let agent = LiveAgent {
                    thread_id: agent_id,
                    metadata: metadata.unwrap_or_default(),
                    status: thread.agent_status().await,
                };
                let config = Box::new(thread.config_snapshot().await);
                if !config.ephemeral
                    && let Err(err) = state
                        .persist_native_graph_closed(&self.runtime, &captured)
                        .await
                {
                    warn!("failed to persist thread-spawn edge status for {agent_id}: {err}");
                }
                AgentInfo::Loaded { agent, config }
            }
            Err(err)
                if known_agent && matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) =>
            {
                if let Err(err) = state
                    .persist_native_graph_closed(&self.runtime, &captured)
                    .await
                {
                    return Err(CodexErr::Fatal(format!(
                        "failed to persist stale thread-spawn edge status for {agent_id}: {err}"
                    )));
                }
                AgentInfo::Unloaded(metadata.unwrap_or_default())
            }
            Err(err) => return Err(err),
        };
        match self
            .shutdown_captured_tree(&state, agent_id, captured.threads)
            .await
        {
            Err(err)
                if known_agent
                    && matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) =>
            {
                Ok(snapshot)
            }
            result => result.map(|_| snapshot),
        }
    }

    /// Shut down `agent_id` and any live descendants reachable from the in-memory spawn tree.
    #[cfg(test)]
    pub(crate) async fn shutdown_agent_tree(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.runtime.upgrade()?;
        let captured = self.capture_shutdown(&state, agent_id, true).await;
        self.shutdown_captured_tree(&state, agent_id, captured)
            .await
    }

    #[cfg(test)]
    async fn capture_shutdown(
        &self,
        state: &ThreadManagerState,
        agent_id: ThreadId,
        descendants: bool,
    ) -> Vec<(ThreadId, Arc<CodexThread>)> {
        let captured = state
            .fence_native_subtree(&self.runtime, agent_id, descendants, |id| {
                self.forget_v2_residency(id);
                self.runtime.registry.release_spawned_thread(id);
                self.runtime.mailboxes.remove(id);
            })
            .await;
        #[cfg(test)]
        self.runtime
            .registry
            .graph_test_pause(crate::agent::registry::GraphLifecycleTestPoint::AfterOrdinaryCapture)
            .await;
        captured
    }

    async fn capture_shutdown_with_graph_stamp(
        &self,
        state: &ThreadManagerState,
        agent_id: ThreadId,
        descendants: bool,
    ) -> crate::thread_manager::NativeShutdownCapture {
        state
            .fence_native_subtree_with_graph_stamp(&self.runtime, agent_id, descendants, |id| {
                self.forget_v2_residency(id);
                self.runtime.registry.release_spawned_thread(id);
                self.runtime.mailboxes.remove(id);
            })
            .await
    }

    async fn shutdown_captured_tree(
        &self,
        state: &ThreadManagerState,
        agent_id: ThreadId,
        captured: Vec<(ThreadId, Arc<CodexThread>)>,
    ) -> CodexResult<String> {
        let mut result = Err(CodexErr::ThreadNotFound(agent_id));
        let mut descendant_error = None;
        for (id, thread) in captured {
            let stopped = self.shutdown_captured_agent(&state, id, thread).await;
            if id == agent_id {
                result = stopped;
            } else if let Err(error) = stopped {
                descendant_error.get_or_insert(error);
            }
        }
        if let Some(error) = descendant_error {
            return Err(error);
        }
        result
    }
}
