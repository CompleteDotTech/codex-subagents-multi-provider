//! Retains agent-tree membership and resources throughout managed startup.
//! Cleanup releases membership only after acquisition and partial session teardown finish;
//! abandoning that ownership marks tree shutdown as failed.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::sync::PoisonError;

use crate::agent::control::AgentTreeMembership;
use crate::agent::control::AgentTreeTeardownGuard;
use crate::thread_manager::thread_store_error_kind;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrKind;
use codex_protocol::protocol::Op;
use codex_thread_store::LiveThreadInitGuard;
use futures::future::BoxFuture;
use tokio::sync::Mutex;

use super::SessionIo;
use super::session::Session;

#[derive(Default)]
pub(crate) struct SessionStartup {
    teardown: StdMutex<Option<AgentTreeTeardownGuard>>,
    native_admission: StdMutex<NativeAdmissionState>,
    resume_fence: StdMutex<ResumeFenceState>,
    pub(crate) persistence: Mutex<LiveThreadInitGuard>,
    pub(crate) session: OnceLock<Arc<Session>>,
    pub(crate) io: OnceLock<SessionIo>,
}

#[derive(Default)]
enum NativeAdmissionState {
    #[default]
    Unclassified,
    Pending(crate::agent::child_provider_authorization::NativeChildAdmission),
    Checked(crate::agent::child_provider_authorization::NativeChildAdmission),
    Consumed,
    HostCommitted,
}

#[derive(Default)]
enum ResumeFenceState {
    #[default]
    Unclassified,
    Pending(ThreadId),
    Consumed,
}

impl SessionStartup {
    pub(crate) fn requires_owned_writer(&self) -> bool {
        matches!(
            *self
                .native_admission
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            NativeAdmissionState::Pending(_)
                | NativeAdmissionState::Checked(_)
                | NativeAdmissionState::Consumed
        )
    }
    pub(crate) fn defer_resume_fence(&self, target: ThreadId) -> codex_protocol::error::Result<()> {
        let mut state = self
            .resume_fence
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !matches!(*state, ResumeFenceState::Unclassified) {
            return Err(crate::agent::child_provider_authorization::invalid(
                "resume startup fence already classified",
            ));
        }
        *state = ResumeFenceState::Pending(target);
        Ok(())
    }

    pub(crate) fn take_resume_fence(
        &self,
        resumed_target: Option<ThreadId>,
    ) -> codex_protocol::error::Result<Option<ThreadId>> {
        let mut state = self
            .resume_fence
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match std::mem::replace(&mut *state, ResumeFenceState::Consumed) {
            ResumeFenceState::Unclassified if resumed_target.is_some() => {
                Err(crate::agent::child_provider_authorization::invalid(
                    "manager resume startup fence is missing",
                ))
            }
            ResumeFenceState::Unclassified => Ok(None),
            ResumeFenceState::Pending(target) if Some(target) == resumed_target => Ok(Some(target)),
            ResumeFenceState::Pending(_) | ResumeFenceState::Consumed => {
                Err(crate::agent::child_provider_authorization::invalid(
                    "resume startup fence does not match its history or was already consumed",
                ))
            }
        }
    }

    pub(crate) fn set_native_admission(
        &self,
        admission: crate::agent::child_provider_authorization::NativeChildAdmission,
    ) -> codex_protocol::error::Result<()> {
        let mut state = self
            .native_admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !matches!(*state, NativeAdmissionState::Unclassified) {
            return Err(crate::agent::child_provider_authorization::invalid(
                "native startup admission already classified",
            ));
        }
        *state = NativeAdmissionState::Pending(admission);
        Ok(())
    }

    pub(crate) fn check_native_current(&self) -> codex_protocol::error::Result<()> {
        let state = self
            .native_admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match &*state {
            NativeAdmissionState::Pending(admission) | NativeAdmissionState::Checked(admission) => {
                admission.check_current()
            }
            NativeAdmissionState::Unclassified | NativeAdmissionState::HostCommitted => Ok(()),
            NativeAdmissionState::Consumed => {
                Err(crate::agent::child_provider_authorization::invalid(
                    "native startup admission was already consumed",
                ))
            }
        }
    }

    pub(crate) fn admit_native(&self, required: bool) -> codex_protocol::error::Result<()> {
        let mut state = self
            .native_admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match std::mem::replace(&mut *state, NativeAdmissionState::Consumed) {
            NativeAdmissionState::Unclassified if required => {
                Err(crate::agent::child_provider_authorization::invalid(
                    "native startup admission authority is missing",
                ))
            }
            NativeAdmissionState::Unclassified => {
                // Existing explicit host root/internal paths do not carry a
                // native permit. ThreadManager requires it for delegated paths.
                *state = NativeAdmissionState::Unclassified;
                Ok(())
            }
            NativeAdmissionState::Pending(admission) => {
                admission.admit()?;
                *state = NativeAdmissionState::Checked(admission);
                Ok(())
            }
            NativeAdmissionState::Checked(_)
            | NativeAdmissionState::Consumed
            | NativeAdmissionState::HostCommitted => {
                Err(crate::agent::child_provider_authorization::invalid(
                    "native startup admission was already consumed",
                ))
            }
        }
    }

    /// Manager map -> startup -> exact caller signal -> registry -> binding.
    /// Registration consumes the original continuation, with no await in commit.
    pub(crate) fn commit_registration<T>(
        &self,
        runtime: &crate::agent::control::LocalAgentRuntime,
        authority: Arc<crate::agent::child_provider_authorization::RetainedAgentAuthority>,
        commit: impl FnOnce() -> codex_protocol::error::Result<T>,
    ) -> codex_protocol::error::Result<T> {
        let mut state = self
            .native_admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let native = matches!(*state, NativeAdmissionState::Checked(_));
        let result = match &*state {
            NativeAdmissionState::Checked(admission) => {
                admission.commit_registration(authority, commit)
            }
            NativeAdmissionState::Unclassified => {
                let result = commit()?;
                runtime.register_authority(authority);
                Ok(result)
            }
            _ => Err(crate::agent::child_provider_authorization::invalid(
                "native registration continuation is unavailable",
            )),
        };
        if result.is_ok() {
            *state = if native {
                NativeAdmissionState::Consumed
            } else {
                NativeAdmissionState::HostCommitted
            };
        }
        result
    }

    fn lock_teardown(&self) -> MutexGuard<'_, Option<AgentTreeTeardownGuard>> {
        self.teardown.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn hold_membership(&self, membership: AgentTreeMembership) {
        let teardown = membership.into_teardown_guard("session_startup", /*thread_id*/ None);
        let previous = self.lock_teardown().replace(teardown);
        assert!(previous.is_none(), "agent-tree membership already set");
    }

    pub(crate) fn set_session(&self, session: Arc<Session>) {
        let thread_id = session.thread_id;
        let mut teardown = self.lock_teardown();
        if self.session.set(session).is_ok()
            && let Some(teardown) = teardown.as_mut()
        {
            teardown.set_thread_id(thread_id);
        }
    }

    pub(crate) fn session_teardown(&self, thread_id: ThreadId) -> Option<AgentTreeTeardownGuard> {
        let teardown = self.lock_teardown();
        let teardown = teardown.as_ref()?;
        Some(teardown.clone_for_teardown("session_loop", Some(thread_id)))
    }

    pub(crate) fn release_membership(&self) {
        if let Some(teardown) = self.lock_teardown().take() {
            teardown.complete();
        }
    }

    pub(crate) async fn cleanup(&self) {
        let mut teardown = self.lock_teardown().take();
        if let Some(teardown) = teardown.as_mut()
            && let Some(session) = self.session.get()
        {
            teardown.set_thread_id(session.thread_id);
        }
        if let Some(io) = self.io.get() {
            if !self.requires_owned_writer() {
                // Existing explicit host/inline lifetimes hand persistence to
                // their loop as before; native provisional writers never fall
                // back to the legacy ID-only cleanup contract.
                self.persistence.lock().await.commit();
            }
            // Until map registration commits, LiveThread retains its exact
            // provisional writer identity. Fresh cleanup discards only that
            // acquisition; resumed cleanup always preserves saved history.
            let _ = io.submit(Op::Interrupt).await;
            if let Err(error) = io.shutdown_and_wait().await {
                if let Some(teardown) = teardown.as_ref() {
                    teardown.record_shutdown_failure(
                        "stop_session_io",
                        CodexErrKind::from(&error).into(),
                    );
                }
                tracing::warn!("failed to stop cancelled session init: {error}");
            }
        } else {
            if let Some(session) = self.session.get() {
                super::handlers::shutdown_session_runtime(session).await;
            }
        }
        {
            let mut persistence = self.persistence.lock().await;
            if let Err(error) = persistence.discard().await {
                if let Some(teardown) = teardown.as_ref() {
                    teardown.record_shutdown_failure(
                        "discard_persistence",
                        thread_store_error_kind(&error),
                    );
                }
                tracing::warn!(
                    "failed to discard thread persistence for failed session init: {error}"
                );
            }
        }
        if let Some(teardown) = teardown {
            teardown.complete();
        }
    }
}

/// Cleans up a cancelled session start before releasing its agent-tree membership.
pub(crate) struct SessionStartupGuard {
    startup: Option<Arc<SessionStartup>>,
}

impl SessionStartupGuard {
    pub(crate) fn new(startup: Arc<SessionStartup>) -> Self {
        Self {
            startup: Some(startup),
        }
    }

    pub(crate) fn disarm(mut self) {
        self.startup = None;
    }

    pub(crate) fn cleanup(mut self) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            if let Some(startup) = self.startup.take() {
                startup.cleanup().await;
            }
        })
    }
}

impl Drop for SessionStartupGuard {
    fn drop(&mut self) {
        let Some(startup) = self.startup.take() else {
            return;
        };
        drop(tokio::spawn(async move {
            startup.cleanup().await;
        }));
    }
}
