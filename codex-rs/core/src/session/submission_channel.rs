//! Submission-only capacity wakeups around the existing bounded channel.
//!
//! The receiver is deliberately not clonable or exposed: every dequeue and
//! receiver drop must wake guarded producers. No receiver keeps a sender alive.

use super::Submission;
use crate::agent::child_provider_authorization::current::CurrentChildOperation;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use std::sync::Arc;
use tokio::sync::Notify;

#[derive(Clone)]
pub(crate) struct SubmissionSender {
    inner: async_channel::Sender<Submission>,
    capacity_changed: Arc<Notify>,
    #[cfg(test)]
    capacity_waiting_for_test: Arc<Notify>,
}

pub(crate) struct SubmissionReceiver {
    inner: async_channel::Receiver<Submission>,
    capacity_changed: Arc<Notify>,
}

pub(crate) fn bounded_submission_channel(
    capacity: usize,
) -> (SubmissionSender, SubmissionReceiver) {
    let (sender, receiver) = async_channel::bounded(capacity);
    wrap_submission_channel(sender, receiver)
}

#[cfg(test)]
pub(crate) fn unbounded_submission_channel() -> (SubmissionSender, SubmissionReceiver) {
    let (sender, receiver) = async_channel::unbounded();
    wrap_submission_channel(sender, receiver)
}

fn wrap_submission_channel(
    sender: async_channel::Sender<Submission>,
    receiver: async_channel::Receiver<Submission>,
) -> (SubmissionSender, SubmissionReceiver) {
    let capacity_changed = Arc::new(Notify::new());
    (
        SubmissionSender {
            inner: sender,
            capacity_changed: Arc::clone(&capacity_changed),
            #[cfg(test)]
            capacity_waiting_for_test: Arc::new(Notify::new()),
        },
        SubmissionReceiver {
            inner: receiver,
            capacity_changed,
        },
    )
}

impl SubmissionSender {
    pub(crate) async fn send(
        &self,
        submission: Submission,
    ) -> Result<(), async_channel::SendError<Submission>> {
        #[cfg(test)]
        if self.inner.is_full() {
            self.capacity_waiting_for_test.notify_one();
        }
        self.inner.send(submission).await
    }

    pub(crate) async fn send_guarded(
        &self,
        mut submission: Submission,
        guard: Option<&CurrentChildOperation>,
    ) -> CodexResult<()> {
        let Some(guard) = guard else {
            return self
                .send(submission)
                .await
                .map_err(|_| CodexErr::InternalAgentDied);
        };
        loop {
            // Register before checking capacity, so a dequeue between try_send
            // and await cannot be lost. All waiting/polling stays outside the
            // publication, registration and managed-policy locks.
            let changed = self.capacity_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match guard.with_current(|| Ok(self.inner.try_send(submission)))? {
                Ok(()) => return Ok(()),
                Err(async_channel::TrySendError::Closed(_)) => {
                    return Err(CodexErr::InternalAgentDied);
                }
                Err(async_channel::TrySendError::Full(pending)) => {
                    #[cfg(test)]
                    self.capacity_waiting_for_test.notify_one();
                    submission = pending;
                }
            }
            changed.await;
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Observe the real capacity-block point, without changing queue admission.
    #[cfg(test)]
    pub(crate) fn capacity_wait_observer_for_test(&self) -> Arc<Notify> {
        Arc::clone(&self.capacity_waiting_for_test)
    }
}

impl SubmissionReceiver {
    pub(crate) async fn recv(&self) -> Result<Submission, async_channel::RecvError> {
        let submission = self.inner.recv().await?;
        self.capacity_changed.notify_waiters();
        Ok(submission)
    }

    #[cfg(test)]
    pub(crate) fn try_recv(&self) -> Result<Submission, async_channel::TryRecvError> {
        let submission = self.inner.try_recv()?;
        self.capacity_changed.notify_waiters();
        Ok(submission)
    }

    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.len()
    }
}

impl Drop for SubmissionReceiver {
    fn drop(&mut self) {
        // Close before waking, so a full guarded producer observes Closed even
        // if it runs before the inner receiver field is dropped.
        self.inner.close();
        self.capacity_changed.notify_waiters();
    }
}
