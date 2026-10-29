use super::*;
use std::collections::HashMap;

/// Includes reservations awaiting owner dispatch, not just active subscriptions.
pub(super) const MAX_TASK_WAITERS: usize = 256;
type TaskReply = std::result::Result<TaskOutcome, QueryError>;

struct PendingWait {
    // Only the owner may activate a reservation after checking current state.
    target: Option<TaskTarget>,
    ready: bool,
    reply: oneshot::Sender<TaskReply>,
}

#[derive(Default)]
pub(super) struct TaskWaiters {
    next_id: u64,
    closed: bool,
    pending: HashMap<u64, PendingWait>,
}

impl TaskWaiters {
    fn reserve(&mut self) -> std::result::Result<(u64, oneshot::Receiver<TaskReply>), QueryError> {
        if self.closed {
            return Err(QueryError::Message("Core is stopped".into()));
        }
        if self.pending.len() == MAX_TASK_WAITERS {
            return Err(QueryError::Message("Task wait capacity exhausted".into()));
        }
        // Never reuse identities: a cancelled request can still be in the queue.
        let id = self.next_id.checked_add(1).ok_or_else(|| {
            QueryError::Message("Task wait registration identities exhausted".into())
        })?;
        self.next_id = id;
        let (reply, receiver) = oneshot::channel();
        self.pending.insert(
            id,
            PendingWait {
                target: None,
                ready: false,
                reply,
            },
        );
        Ok((id, receiver))
    }

    fn take_ready(&mut self) -> Option<PendingWait> {
        let id = self
            .pending
            .iter()
            .find_map(|(id, pending)| pending.ready.then_some(*id))?;
        self.pending.remove(&id)
    }
}

struct WaitRegistration {
    registry: Arc<Mutex<TaskWaiters>>,
    id: u64,
}

impl Drop for WaitRegistration {
    fn drop(&mut self) {
        // Cancellation releases capacity immediately, even before owner dispatch.
        self.registry.lock().pending.remove(&self.id);
    }
}

impl CoreHandle {
    /// Wait indefinitely for an owner-committed terminal outcome.
    /// Dropping this future only cancels the wait, never the business task.
    pub async fn wait_task(
        &self,
        target: TaskTarget,
    ) -> std::result::Result<TaskOutcome, QueryError> {
        let (id, receiver) = self.task_waiters.lock().reserve()?;
        let _registration = WaitRegistration {
            registry: Arc::clone(&self.task_waiters),
            id,
        };
        self.admit(Request {
            operation: Operation::WaitTask {
                target,
                registration: id,
            },
            reply: None,
        })
        .await
        .map_err(|error| QueryError::Message(format!("Core unavailable: {error:#}")))?;
        receiver
            .await
            .map_err(|error| QueryError::Message(format!("Core response unavailable: {error}")))?
    }
}

impl Core {
    pub(super) fn register_task_wait(&self, target: TaskTarget, registration: u64) {
        if !self.task_waiters.lock().pending.contains_key(&registration) {
            return; // A disconnected caller cannot be resurrected by queued work.
        }
        // Inspection and activation share an owner turn, but payload cloning and
        // waking the receiver never hold the shared cancellation registry lock.
        match self.task_outcome(target) {
            Ok(None) => {
                if let Some(pending) = self.task_waiters.lock().pending.get_mut(&registration) {
                    pending.target = Some(target);
                }
            }
            outcome => {
                let pending = self.task_waiters.lock().pending.remove(&registration);
                if let Some(pending) = pending {
                    let _ = pending.reply.send(outcome.map(Option::unwrap));
                }
            }
        }
    }

    /// Borrow active status first; clone full payloads only for terminal replies.
    fn task_outcome(
        &self,
        target: TaskTarget,
    ) -> std::result::Result<Option<TaskOutcome>, QueryError> {
        match target {
            TaskTarget::Scan { id } => {
                if self
                    .active_scan
                    .as_ref()
                    .is_some_and(|task| task.status.id == id)
                    || self
                        .active_external_admission
                        .as_ref()
                        .is_some_and(|task| task.status.id == id)
                {
                    return Ok(None);
                }
                self.scan_results
                    .iter()
                    .find(|status| status.id == id)
                    .cloned()
                    .map(TaskOutcome::Scan)
                    .map(Some)
                    .ok_or_else(|| QueryError::Message("Unknown or expired scan task".into()))
            }
            TaskTarget::Relocation { id } => {
                if self
                    .active_relocation
                    .as_ref()
                    .is_some_and(|task| task.status.id == id)
                {
                    return Ok(None);
                }
                self.relocation_results
                    .iter()
                    .find(|status| status.id == id)
                    .cloned()
                    .map(TaskOutcome::Relocation)
                    .map(Some)
                    .ok_or_else(|| QueryError::Message("Unknown or expired relocation task".into()))
            }
            TaskTarget::Processing { id } => {
                let status = self.processing_status_ref(id).ok_or_else(|| {
                    QueryError::Message("Unknown or expired processing task".into())
                })?;
                if self.processing.active_id() == Some(id)
                    || !matches!(
                        status.phase,
                        ProcessingPhase::Complete
                            | ProcessingPhase::Cancelled
                            | ProcessingPhase::Failed
                    )
                {
                    return Ok(None);
                }
                Ok(Some(TaskOutcome::Processing(Box::new(status.clone()))))
            }
        }
    }

    /// Mark only the explicitly completed identity; ordinary progress does no work.
    pub(super) fn task_completed(&mut self, target: TaskTarget) {
        let mut registry = self.task_waiters.lock();
        for pending in registry
            .pending
            .values_mut()
            .filter(|pending| pending.target == Some(target))
        {
            pending.ready = true;
            self.task_completions_pending = true;
        }
    }

    /// Called after publishing committed state, never while a worker owns the result.
    pub(super) fn flush_task_completions(&mut self) {
        if !std::mem::take(&mut self.task_completions_pending) {
            return;
        }
        loop {
            let pending = self.task_waiters.lock().take_ready();
            let Some(pending) = pending else { break };
            let outcome = self
                .task_outcome(pending.target.unwrap())
                .map(|outcome| outcome.expect("Task completion follows the terminal owner commit"));
            let _ = pending.reply.send(outcome);
        }
    }

    pub(super) fn close_task_waiters(&self) {
        let pending = {
            let mut registry = self.task_waiters.lock();
            registry.closed = true;
            std::mem::take(&mut registry.pending)
        };
        for (_, pending) in pending {
            let _ = pending.reply.send(Err(QueryError::Message(
                "Core stopped before task completion".into(),
            )));
        }
    }
}
