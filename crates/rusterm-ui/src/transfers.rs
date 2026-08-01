use std::path::PathBuf;

/// The direction of a transfer relative to the local machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEndpoint {
    Local(PathBuf),
    Remote(String),
}

impl FileEndpoint {
    /// Returns the transfer direction when `self` is the source and
    /// `destination` is the destination.
    pub fn direction_to(&self, destination: &Self) -> Option<TransferDirection> {
        match (self, destination) {
            (Self::Local(_), Self::Remote(_)) => Some(TransferDirection::Upload),
            (Self::Remote(_), Self::Local(_)) => Some(TransferDirection::Download),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRequest {
    pub session: String,
    pub source: FileEndpoint,
    pub destination: FileEndpoint,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferStatus {
    Queued,
    Running,
    Succeeded,
    Failed(String),
    Cancelled,
}

impl TransferStatus {
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed(_) | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferJob {
    pub id: String,
    pub session: String,
    pub source: FileEndpoint,
    pub destination: FileEndpoint,
    pub transferred: u64,
    pub total: u64,
    pub status: TransferStatus,
    pub attempt: u32,
}

impl TransferJob {
    pub fn direction(&self) -> Option<TransferDirection> {
        self.source.direction_to(&self.destination)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferState {
    pub jobs: Vec<TransferJob>,
    pub concurrency_limit: usize,
}

impl Default for TransferState {
    fn default() -> Self {
        Self {
            jobs: Vec::new(),
            concurrency_limit: 2,
        }
    }
}

impl TransferState {
    pub fn new(concurrency_limit: usize) -> Self {
        Self {
            concurrency_limit,
            ..Self::default()
        }
    }

    /// Adds a queued transfer. Duplicate job IDs are rejected.
    pub fn enqueue(
        &mut self,
        id: impl Into<String>,
        session: impl Into<String>,
        source: FileEndpoint,
        destination: FileEndpoint,
        total: u64,
    ) -> bool {
        let id = id.into();
        if self.find(&id).is_some() {
            return false;
        }

        self.jobs.push(TransferJob {
            id,
            session: session.into(),
            source,
            destination,
            transferred: 0,
            total,
            status: TransferStatus::Queued,
            attempt: 0,
        });
        true
    }

    pub fn find(&self, id: &str) -> Option<&TransferJob> {
        self.jobs.iter().find(|job| job.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut TransferJob> {
        self.jobs.iter_mut().find(|job| job.id == id)
    }

    /// Selects the first queued job without changing its status.
    pub fn next_queued(&self) -> Option<&TransferJob> {
        self.jobs
            .iter()
            .find(|job| job.status == TransferStatus::Queued)
    }

    pub fn running_count(&self) -> usize {
        self.jobs
            .iter()
            .filter(|job| job.status == TransferStatus::Running)
            .count()
    }

    /// Starts the first queued job when a concurrency slot is available.
    pub fn start_next(&mut self) -> Option<&TransferJob> {
        if self.running_count() >= self.concurrency_limit {
            return None;
        }

        let index = self
            .jobs
            .iter()
            .position(|job| job.status == TransferStatus::Queued)?;
        self.jobs[index].status = TransferStatus::Running;
        Some(&self.jobs[index])
    }

    /// Marks a particular queued job as running when a slot is available.
    pub fn mark_running(&mut self, id: &str) -> bool {
        if self.running_count() >= self.concurrency_limit {
            return false;
        }

        let Some(job) = self.find_mut(id) else {
            return false;
        };
        if job.status != TransferStatus::Queued {
            return false;
        }

        job.status = TransferStatus::Running;
        true
    }

    /// Updates a running job's progress without allowing it to move backwards.
    pub fn report_progress(&mut self, id: &str, transferred: u64) -> bool {
        let Some(job) = self.find_mut(id) else {
            return false;
        };
        if job.status != TransferStatus::Running || transferred < job.transferred {
            return false;
        }

        job.transferred = transferred;
        true
    }

    pub fn succeed(&mut self, id: &str) -> bool {
        let Some(job) = self.find_mut(id) else {
            return false;
        };
        if job.status != TransferStatus::Running {
            return false;
        }

        job.status = TransferStatus::Succeeded;
        true
    }

    pub fn fail(&mut self, id: &str, reason: impl Into<String>) -> bool {
        let Some(job) = self.find_mut(id) else {
            return false;
        };
        if job.status != TransferStatus::Running {
            return false;
        }

        job.status = TransferStatus::Failed(reason.into());
        true
    }

    /// Cancels a queued or running job. Repeated cancellation and cancellation
    /// of any other terminal state are no-ops.
    pub fn cancel(&mut self, id: &str) -> bool {
        let Some(job) = self.find_mut(id) else {
            return false;
        };
        if !matches!(job.status, TransferStatus::Queued | TransferStatus::Running) {
            return false;
        }

        job.status = TransferStatus::Cancelled;
        true
    }

    /// Requeues a failed or cancelled job as a new attempt.
    pub fn retry(&mut self, id: &str) -> bool {
        let Some(job) = self.find_mut(id) else {
            return false;
        };
        if !matches!(
            job.status,
            TransferStatus::Failed(_) | TransferStatus::Cancelled
        ) {
            return false;
        }

        job.status = TransferStatus::Queued;
        job.transferred = 0;
        job.attempt += 1;
        true
    }

    /// Removes terminal jobs and returns the number removed.
    pub fn clear_finished(&mut self) -> usize {
        let previous_len = self.jobs.len();
        self.jobs.retain(|job| !job.status.is_finished());
        previous_len - self.jobs.len()
    }

    /// Cancels all cancellable jobs belonging to `session`, returning the
    /// number of state transitions performed.
    pub fn cancel_for_session(&mut self, session: &str) -> usize {
        let mut cancelled = 0;
        for job in &mut self.jobs {
            if job.session == session
                && matches!(job.status, TransferStatus::Queued | TransferStatus::Running)
            {
                job.status = TransferStatus::Cancelled;
                cancelled += 1;
            }
        }
        cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(path: &str) -> FileEndpoint {
        FileEndpoint::Local(PathBuf::from(path))
    }

    fn remote(path: &str) -> FileEndpoint {
        FileEndpoint::Remote(path.to_string())
    }

    fn enqueue(state: &mut TransferState, id: &str, session: &str, total: u64) {
        assert!(state.enqueue(id, session, local("local.txt"), remote("remote.txt"), total));
    }

    #[test]
    fn default_state_has_two_slots_and_enqueue_initializes_job() {
        let mut state = TransferState::default();
        assert_eq!(state.concurrency_limit, 2);
        assert!(state.jobs.is_empty());

        enqueue(&mut state, "job-1", "session-1", 100);
        let job = state.find("job-1").unwrap();
        assert_eq!(job.session, "session-1");
        assert_eq!(job.transferred, 0);
        assert_eq!(job.total, 100);
        assert_eq!(job.status, TransferStatus::Queued);
        assert_eq!(job.attempt, 0);
        assert_eq!(job.direction(), Some(TransferDirection::Upload));
    }

    #[test]
    fn direction_is_derived_from_endpoint_order() {
        assert_eq!(
            remote("remote.txt").direction_to(&local("local.txt")),
            Some(TransferDirection::Download)
        );
        assert_eq!(local("a").direction_to(&local("b")), None);
        assert_eq!(remote("a").direction_to(&remote("b")), None);
    }

    #[test]
    fn enqueue_rejects_duplicate_ids_without_replacing_the_job() {
        let mut state = TransferState::default();
        enqueue(&mut state, "job-1", "session-1", 100);

        assert!(!state.enqueue("job-1", "session-2", remote("other"), local("other"), 200,));
        assert_eq!(state.jobs.len(), 1);
        assert_eq!(state.find("job-1").unwrap().session, "session-1");
    }

    #[test]
    fn start_next_is_fifo_and_respects_concurrency_limit() {
        let mut state = TransferState::new(2);
        enqueue(&mut state, "job-1", "session", 10);
        enqueue(&mut state, "job-2", "session", 10);
        enqueue(&mut state, "job-3", "session", 10);

        assert_eq!(state.next_queued().unwrap().id, "job-1");
        assert_eq!(state.start_next().unwrap().id, "job-1");
        assert_eq!(state.start_next().unwrap().id, "job-2");
        assert!(state.start_next().is_none());
        assert_eq!(state.running_count(), 2);
        assert_eq!(state.next_queued().unwrap().id, "job-3");

        assert!(state.succeed("job-1"));
        assert_eq!(state.start_next().unwrap().id, "job-3");
    }

    #[test]
    fn zero_concurrency_limit_never_starts_a_job() {
        let mut state = TransferState::new(0);
        enqueue(&mut state, "job-1", "session", 10);

        assert!(state.start_next().is_none());
        assert!(!state.mark_running("job-1"));
        assert_eq!(state.find("job-1").unwrap().status, TransferStatus::Queued);
    }

    #[test]
    fn mark_running_requires_a_queued_job_and_an_available_slot() {
        let mut state = TransferState::new(1);
        enqueue(&mut state, "job-1", "session", 10);
        enqueue(&mut state, "job-2", "session", 10);

        assert!(state.mark_running("job-2"));
        assert!(!state.mark_running("job-2"));
        assert!(!state.mark_running("job-1"));
        assert!(!state.mark_running("missing"));
        assert_eq!(state.find("job-1").unwrap().status, TransferStatus::Queued);
    }

    #[test]
    fn progress_is_running_only_and_monotonic() {
        let mut state = TransferState::default();
        enqueue(&mut state, "job-1", "session", 100);

        assert!(!state.report_progress("job-1", 10));
        assert!(state.mark_running("job-1"));
        assert!(state.report_progress("job-1", 40));
        assert!(state.report_progress("job-1", 40));
        assert!(!state.report_progress("job-1", 39));
        assert_eq!(state.find("job-1").unwrap().transferred, 40);

        assert!(state.report_progress("job-1", 150));
        assert_eq!(state.find("job-1").unwrap().transferred, 150);
        assert!(!state.report_progress("missing", 1));
    }

    #[test]
    fn succeed_is_running_only_and_is_terminal() {
        let mut state = TransferState::default();
        enqueue(&mut state, "job-1", "session", 100);

        assert!(!state.succeed("job-1"));
        assert!(state.mark_running("job-1"));
        assert!(state.report_progress("job-1", 60));
        assert!(state.succeed("job-1"));
        assert_eq!(state.find("job-1").unwrap().transferred, 60);
        assert_eq!(
            state.find("job-1").unwrap().status,
            TransferStatus::Succeeded
        );

        assert!(!state.fail("job-1", "late error"));
        assert!(!state.cancel("job-1"));
        assert!(!state.report_progress("job-1", 100));
        assert_eq!(
            state.find("job-1").unwrap().status,
            TransferStatus::Succeeded
        );
    }

    #[test]
    fn fail_is_running_only_and_preserves_progress() {
        let mut state = TransferState::default();
        enqueue(&mut state, "job-1", "session", 100);

        assert!(!state.fail("job-1", "not running"));
        assert!(state.mark_running("job-1"));
        assert!(state.report_progress("job-1", 25));
        assert!(state.fail("job-1", "connection lost"));
        assert_eq!(state.find("job-1").unwrap().transferred, 25);
        assert_eq!(
            state.find("job-1").unwrap().status,
            TransferStatus::Failed("connection lost".to_string())
        );
        assert!(!state.succeed("job-1"));
        assert!(!state.cancel("job-1"));
    }

    #[test]
    fn cancel_is_idempotent_for_queued_and_running_jobs() {
        let mut state = TransferState::default();
        enqueue(&mut state, "queued", "session", 10);
        enqueue(&mut state, "running", "session", 10);
        assert!(state.mark_running("running"));

        assert!(state.cancel("queued"));
        assert!(!state.cancel("queued"));
        assert!(state.cancel("running"));
        assert!(!state.cancel("running"));
        assert!(!state.cancel("missing"));
        assert_eq!(
            state.find("queued").unwrap().status,
            TransferStatus::Cancelled
        );
        assert_eq!(
            state.find("running").unwrap().status,
            TransferStatus::Cancelled
        );
    }

    #[test]
    fn retry_only_requeues_failed_or_cancelled_jobs() {
        let mut state = TransferState::default();
        enqueue(&mut state, "failed", "session", 100);
        enqueue(&mut state, "cancelled", "session", 100);
        enqueue(&mut state, "queued", "session", 100);

        assert!(state.mark_running("failed"));
        assert!(state.report_progress("failed", 70));
        assert!(state.fail("failed", "network"));
        assert!(state.cancel("cancelled"));

        assert!(state.retry("failed"));
        assert!(state.retry("cancelled"));
        assert!(!state.retry("queued"));
        assert!(!state.retry("missing"));

        for id in ["failed", "cancelled"] {
            let job = state.find(id).unwrap();
            assert_eq!(job.status, TransferStatus::Queued);
            assert_eq!(job.transferred, 0);
            assert_eq!(job.attempt, 1);
        }

        assert!(state.mark_running("failed"));
        assert!(state.fail("failed", "again"));
        assert!(state.retry("failed"));
        assert_eq!(state.find("failed").unwrap().attempt, 2);
    }

    #[test]
    fn cancel_for_session_only_changes_cancellable_matching_jobs() {
        let mut state = TransferState::new(4);
        for (id, session) in [
            ("queued-a", "a"),
            ("running-a", "a"),
            ("failed-a", "a"),
            ("queued-b", "b"),
        ] {
            enqueue(&mut state, id, session, 10);
        }
        assert!(state.mark_running("running-a"));
        assert!(state.mark_running("failed-a"));
        assert!(state.fail("failed-a", "failure"));

        assert_eq!(state.cancel_for_session("a"), 2);
        assert_eq!(state.cancel_for_session("a"), 0);
        assert_eq!(
            state.find("queued-a").unwrap().status,
            TransferStatus::Cancelled
        );
        assert_eq!(
            state.find("running-a").unwrap().status,
            TransferStatus::Cancelled
        );
        assert_eq!(
            state.find("failed-a").unwrap().status,
            TransferStatus::Failed("failure".to_string())
        );
        assert_eq!(
            state.find("queued-b").unwrap().status,
            TransferStatus::Queued
        );
    }

    #[test]
    fn clear_finished_removes_all_terminal_statuses_only() {
        let mut state = TransferState::new(4);
        for id in ["queued", "running", "succeeded", "failed", "cancelled"] {
            enqueue(&mut state, id, "session", 10);
        }
        assert!(state.mark_running("running"));
        assert!(state.mark_running("succeeded"));
        assert!(state.succeed("succeeded"));
        assert!(state.mark_running("failed"));
        assert!(state.fail("failed", "failure"));
        assert!(state.cancel("cancelled"));

        assert_eq!(state.clear_finished(), 3);
        assert_eq!(
            state
                .jobs
                .iter()
                .map(|job| job.id.as_str())
                .collect::<Vec<_>>(),
            vec!["queued", "running"]
        );
        assert_eq!(state.clear_finished(), 0);
    }

    #[test]
    fn find_mut_allows_runtime_metadata_updates() {
        let mut state = TransferState::default();
        enqueue(&mut state, "job-1", "session", 10);

        state.find_mut("job-1").unwrap().total = 20;
        assert_eq!(state.find("job-1").unwrap().total, 20);
        assert!(state.find_mut("missing").is_none());
    }
}
