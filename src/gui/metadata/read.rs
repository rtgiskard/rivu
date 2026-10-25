//! Coordinates details demand, projection validity, and the single physical reader.
//!
//! The edit session's token protects its baseline and identity; projection generation
//! independently protects query freshness. Neither identity substitutes for the other.
//! A pending refresh is also the one coalesced queued demand while a worker exists.
//! Logical cancellation preserves that demand and never releases the physical gate.

use super::session::{LoadToken, MetadataDraft, MetadataSession, MetadataValues};
use crate::{core::QueryTicket, projection::QueryProjection};
use rivu_protocol::{
    model::Query,
    response::{QueryResponse, QueryRevisions, ViewResponse},
};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(in crate::gui) struct MetadataReadCoordinator {
    projection: QueryProjection,
    selected: Option<i64>,
    quiet_until: Option<Instant>,
    needs_refresh: bool,
    worker: Option<ReadWorker>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadWorker {
    Quiet(MetadataRead),
    Reading(MetadataRead),
    TimedOut(MetadataRead),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MetadataRead {
    token: LoadToken,
    track_id: i64,
    generation: u64,
}

impl MetadataRead {
    pub(super) fn query(self) -> Query {
        Query::Track {
            track_id: self.track_id,
        }
    }
}

pub(super) struct MetadataReadTask {
    pub(super) read: MetadataRead,
    pub(super) delay: Duration,
    pub(super) ticket: QueryTicket,
}

pub(super) struct ReadEnvironment {
    pub(super) visible: bool,
    pub(super) revisions: QueryRevisions,
}

pub(super) enum ReadCompletion {
    Retired,
    Loaded(Option<MetadataValues>),
    Failed,
}

impl MetadataReadCoordinator {
    pub(super) fn sync(&mut self, selected: Option<i64>, revisions: QueryRevisions, now: Instant) {
        let changed = self.selected != selected;
        self.selected = selected;
        if let Some(track_id) = selected {
            if self.projection.sync(&Query::Track { track_id }, revisions) || changed {
                self.demand_changed(now);
            }
        } else {
            if changed {
                self.projection.cancel();
            }
            self.needs_refresh = false;
            self.quiet_until = None;
        }
    }

    /// An explicit request also retries an unchanged identity or failed read.
    pub(super) fn request(&mut self, id: i64, revisions: QueryRevisions, now: Instant) {
        self.selected = Some(id);
        if !self
            .projection
            .sync(&Query::Track { track_id: id }, revisions)
        {
            self.projection.cancel();
        }
        self.demand_changed(now);
    }

    fn demand_changed(&mut self, now: Instant) {
        self.quiet_until = Some(now + Duration::from_millis(80));
        self.needs_refresh = true;
    }

    /// Hidden windows stop publishing, not the already submitted physical worker.
    pub(super) fn cancel(&mut self) {
        if self.worker.is_some() {
            self.projection.cancel();
            self.needs_refresh = self.selected.is_some();
        }
    }

    pub(super) fn prepare(
        &mut self,
        session: &mut MetadataSession,
        visible: bool,
        now: Instant,
    ) -> Option<MetadataReadTask> {
        if !visible || !self.needs_refresh || self.worker.is_some() {
            return None;
        }
        let track_id = self.selected?;
        let read = MetadataRead {
            token: session.begin_load(track_id),
            track_id,
            generation: self.projection.generation(),
        };
        self.worker = Some(ReadWorker::Quiet(read));
        self.needs_refresh = false;
        Some(MetadataReadTask {
            read,
            delay: self.quiet_until.map_or(Duration::ZERO, |deadline| {
                deadline.saturating_duration_since(now)
            }),
            ticket: self.projection.ticket(),
        })
    }

    fn current(
        &self,
        read: MetadataRead,
        session: &MetadataSession,
        environment: &ReadEnvironment,
    ) -> bool {
        environment.visible
            && !self.needs_refresh
            && self.selected == Some(read.track_id)
            && session.accepts_load(read.token)
            && self
                .projection
                .accepts(&read.query(), read.generation, environment.revisions)
    }

    /// Called when the quiet timer ends, before any I/O is submitted.
    pub(super) fn submit(
        &mut self,
        read: MetadataRead,
        session: &MetadataSession,
        environment: ReadEnvironment,
    ) -> bool {
        if self.worker != Some(ReadWorker::Quiet(read)) {
            return false;
        }
        if self.current(read, session, &environment) {
            self.worker = Some(ReadWorker::Reading(read));
            return true;
        }
        self.retire();
        false
    }

    fn retire(&mut self) {
        self.worker = None;
        self.needs_refresh = self.selected.is_some();
    }

    /// Logical failure changes only presentation; the submitted reader still owns
    /// its slot until `finish`, even if Retry or another selection arrives.
    pub(super) fn timeout(
        &mut self,
        read: MetadataRead,
        session: &mut MetadataSession,
        environment: ReadEnvironment,
        response: QueryResponse,
    ) -> bool {
        if self.worker != Some(ReadWorker::Reading(read))
            || !self.current(read, session, &environment)
        {
            return false;
        }
        let Err(error) = response.result else {
            return false;
        };
        self.worker = Some(ReadWorker::TimedOut(read));
        session.fail_load(read.token, error.to_string())
    }

    /// Completion alone retires submitted I/O. Widget text is borrowed only here;
    /// the coordinator never stores a second draft or changes the session baseline.
    pub(super) fn finish(
        &mut self,
        read: MetadataRead,
        session: &mut MetadataSession,
        environment: ReadEnvironment,
        response: QueryResponse,
        draft: MetadataDraft<'_>,
        unexpected_response: impl FnOnce() -> String,
    ) -> ReadCompletion {
        if self.worker == Some(ReadWorker::TimedOut(read)) {
            self.worker = None;
            return if self.needs_refresh {
                ReadCompletion::Retired
            } else {
                ReadCompletion::Failed
            };
        }
        if self.worker != Some(ReadWorker::Reading(read)) {
            return ReadCompletion::Retired;
        }
        if !self.current(read, session, &environment) {
            self.retire();
            return ReadCompletion::Retired;
        }
        self.worker = None;
        let result = match response.result {
            Ok(result) => result,
            Err(error) => {
                session.fail_load(read.token, error.to_string());
                return ReadCompletion::Failed;
            }
        };
        if !self
            .projection
            .accepts(&read.query(), read.generation, response.revisions)
        {
            self.needs_refresh = self.selected.is_some();
            return ReadCompletion::Retired;
        }
        if let ViewResponse::Track(track) = result {
            ReadCompletion::Loaded(session.accept_load(read.token, track, draft))
        } else {
            session.fail_load(read.token, unexpected_response());
            ReadCompletion::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivu_protocol::response::QueryError;

    fn environment(visible: bool) -> ReadEnvironment {
        ReadEnvironment {
            visible,
            revisions: QueryRevisions::default(),
        }
    }

    fn response() -> QueryResponse {
        QueryResponse {
            revisions: QueryRevisions::default(),
            result: Ok(ViewResponse::Track(None)),
        }
    }

    fn finish(
        owner: &mut MetadataReadCoordinator,
        session: &mut MetadataSession,
        read: MetadataRead,
        visible: bool,
    ) -> ReadCompletion {
        owner.finish(
            read,
            session,
            environment(visible),
            response(),
            MetadataValues::default().as_draft(),
            || "wrong response".into(),
        )
    }

    fn request(
        owner: &mut MetadataReadCoordinator,
        session: &mut MetadataSession,
        id: i64,
        now: Instant,
    ) {
        session.select(id);
        owner.request(id, QueryRevisions::default(), now);
    }

    #[test]
    fn timeout_settles_loading_but_retry_and_replacement_wait_for_retirement() {
        use super::super::session::MetadataReadState;
        for replacement in [false, true] {
            let now = Instant::now();
            let mut owner = MetadataReadCoordinator::default();
            let mut session = MetadataSession::default();
            request(&mut owner, &mut session, 1, now);
            let first = owner.prepare(&mut session, true, now).unwrap();
            assert!(owner.submit(first.read, &session, environment(true)));
            assert!(owner.timeout(
                first.read,
                &mut session,
                environment(true),
                QueryResponse {
                    revisions: QueryRevisions::default(),
                    result: Err(QueryError::Message("timed out".into())),
                }
            ));
            assert_eq!(
                session.read_state(),
                &MetadataReadState::Failed("timed out".into())
            );
            assert!(owner.prepare(&mut session, true, now).is_none());
            request(
                &mut owner,
                &mut session,
                if replacement { 2 } else { 1 },
                now,
            );
            assert!(owner.prepare(&mut session, true, now).is_none());
            assert!(matches!(
                finish(&mut owner, &mut session, first.read, true),
                ReadCompletion::Retired
            ));
            // The deliberately successful late reply did not replace timeout state.
            assert_eq!(session.selected(), Some(if replacement { 2 } else { 1 }));
            let next = owner.prepare(&mut session, true, now).unwrap();
            assert_eq!(next.read.track_id, if replacement { 2 } else { 1 });
            assert!(owner.submit(next.read, &session, environment(true)));
            assert!(matches!(
                finish(&mut owner, &mut session, next.read, true),
                ReadCompletion::Loaded(_)
            ));
        }
    }

    #[test]
    fn obsolete_timeout_cannot_fail_replacement_or_release_reader() {
        let now = Instant::now();
        let mut owner = MetadataReadCoordinator::default();
        let mut session = MetadataSession::default();
        request(&mut owner, &mut session, 1, now);
        let first = owner.prepare(&mut session, true, now).unwrap();
        assert!(owner.submit(first.read, &session, environment(true)));
        request(&mut owner, &mut session, 2, now);
        assert!(!owner.timeout(
            first.read,
            &mut session,
            environment(true),
            QueryResponse {
                revisions: QueryRevisions::default(),
                result: Err(QueryError::Message("obsolete".into())),
            }
        ));
        assert!(owner.prepare(&mut session, true, now).is_none());
        assert!(matches!(
            finish(&mut owner, &mut session, first.read, true),
            ReadCompletion::Retired
        ));
        assert_eq!(
            owner
                .prepare(&mut session, true, now)
                .unwrap()
                .read
                .track_id,
            2
        );
    }

    #[test]
    fn latest_demand_waits_for_worker_and_only_remaining_quiet_interval() {
        let now = Instant::now();
        let mut owner = MetadataReadCoordinator::default();
        let mut session = MetadataSession::default();
        request(&mut owner, &mut session, 1, now);
        let first = owner.prepare(&mut session, true, now).unwrap();
        assert_eq!(first.delay, Duration::from_millis(80));
        assert!(owner.submit(first.read, &session, environment(true)));
        request(&mut owner, &mut session, 2, now + Duration::from_millis(20));
        assert!(owner.prepare(&mut session, true, now).is_none());
        assert!(matches!(
            finish(&mut owner, &mut session, first.read, true),
            ReadCompletion::Retired
        ));
        let next = owner
            .prepare(&mut session, true, now + Duration::from_millis(50))
            .unwrap();
        assert_eq!(next.read.track_id, 2);
        assert_eq!(next.delay, Duration::from_millis(50));
        // A repeated completion cannot release the replacement's worker gate.
        finish(&mut owner, &mut session, first.read, true);
        assert!(owner.prepare(&mut session, true, now).is_none());
        assert!(owner.submit(next.read, &session, environment(true)));
    }

    #[test]
    fn hidden_timer_retires_and_reopening_without_snapshot_change_restores_demand() {
        let now = Instant::now();
        let mut owner = MetadataReadCoordinator::default();
        let mut session = MetadataSession::default();
        request(&mut owner, &mut session, 1, now);
        assert!(owner.prepare(&mut session, false, now).is_none());
        let first = owner
            .prepare(&mut session, true, now + Duration::from_millis(30))
            .unwrap();
        assert_eq!(first.delay, Duration::from_millis(50));
        assert!(!owner.submit(first.read, &session, environment(false)));
        assert!(owner.prepare(&mut session, false, now).is_none());
        let reopened = owner
            .prepare(&mut session, true, now + Duration::from_millis(100))
            .unwrap();
        assert_eq!(reopened.delay, Duration::ZERO);
    }

    #[test]
    fn logical_cancel_retains_physical_worker_until_hidden_completion() {
        let now = Instant::now();
        let mut owner = MetadataReadCoordinator::default();
        let mut session = MetadataSession::default();
        request(&mut owner, &mut session, 1, now);
        let first = owner.prepare(&mut session, true, now).unwrap();
        assert!(owner.submit(first.read, &session, environment(true)));
        owner.cancel();
        assert!(owner.prepare(&mut session, true, now).is_none());
        assert!(matches!(
            finish(&mut owner, &mut session, first.read, false),
            ReadCompletion::Retired
        ));
        assert!(owner.prepare(&mut session, false, now).is_none());
        let reopened = owner
            .prepare(&mut session, true, now + Duration::from_millis(100))
            .unwrap();
        assert_eq!(reopened.delay, Duration::ZERO);
        assert!(owner.submit(reopened.read, &session, environment(true)));
    }

    #[test]
    fn tab_hidden_during_io_preserves_demand_without_explicit_cancel() {
        let now = Instant::now();
        let mut owner = MetadataReadCoordinator::default();
        let mut session = MetadataSession::default();
        request(&mut owner, &mut session, 1, now);
        let first = owner.prepare(&mut session, true, now).unwrap();
        assert!(owner.submit(first.read, &session, environment(true)));
        finish(&mut owner, &mut session, first.read, false);
        assert!(owner.prepare(&mut session, true, now).is_some());
    }

    #[test]
    fn clearing_and_reselecting_same_track_rejects_old_session_and_keeps_worker() {
        let now = Instant::now();
        let mut owner = MetadataReadCoordinator::default();
        let mut session = MetadataSession::default();
        request(&mut owner, &mut session, 1, now);
        let first = owner.prepare(&mut session, true, now).unwrap();
        assert!(owner.submit(first.read, &session, environment(true)));
        session.clear_selection();
        owner.sync(None, QueryRevisions::default(), now);
        assert!(owner.prepare(&mut session, true, now).is_none());
        request(&mut owner, &mut session, 1, now);
        assert!(owner.prepare(&mut session, true, now).is_none());
        assert!(matches!(
            finish(&mut owner, &mut session, first.read, true),
            ReadCompletion::Retired
        ));
        let next = owner.prepare(&mut session, true, now).unwrap();
        assert!(owner.submit(next.read, &session, environment(true)));
        assert!(!session.accepts_load(first.read.token));
    }

    #[test]
    fn invalidated_session_cannot_publish_even_with_current_projection() {
        let now = Instant::now();
        let mut owner = MetadataReadCoordinator::default();
        let mut session = MetadataSession::default();
        request(&mut owner, &mut session, 1, now);
        let first = owner.prepare(&mut session, true, now).unwrap();
        assert!(owner.submit(first.read, &session, environment(true)));
        session.discard_draft();
        assert!(matches!(
            finish(&mut owner, &mut session, first.read, true),
            ReadCompletion::Retired
        ));
        assert!(owner.prepare(&mut session, true, now).is_some());
    }

    #[test]
    fn current_failure_stops_automatic_retry_but_explicit_retry_can_load() {
        let now = Instant::now();
        let mut owner = MetadataReadCoordinator::default();
        let mut session = MetadataSession::default();
        request(&mut owner, &mut session, 1, now);
        let first = owner.prepare(&mut session, true, now).unwrap();
        assert!(owner.submit(first.read, &session, environment(true)));
        assert!(matches!(
            owner.finish(
                first.read,
                &mut session,
                environment(true),
                QueryResponse {
                    revisions: QueryRevisions::default(),
                    result: Err(QueryError::Message("read failed".into())),
                },
                MetadataValues::default().as_draft(),
                || "wrong response".into(),
            ),
            ReadCompletion::Failed
        ));
        assert!(owner.prepare(&mut session, true, now).is_none());
        request(&mut owner, &mut session, 1, now);
        let retry = owner.prepare(&mut session, true, now).unwrap();
        assert!(owner.submit(retry.read, &session, environment(true)));
        assert!(matches!(
            finish(&mut owner, &mut session, retry.read, true),
            ReadCompletion::Loaded(_)
        ));
        assert!(owner.prepare(&mut session, true, now).is_none());
    }

    #[test]
    fn freshness_change_queues_latest_read_without_releasing_physical_worker() {
        let now = Instant::now();
        let mut owner = MetadataReadCoordinator::default();
        let mut session = MetadataSession::default();
        request(&mut owner, &mut session, 1, now);
        let first = owner.prepare(&mut session, true, now).unwrap();
        assert!(owner.submit(first.read, &session, environment(true)));
        let revisions = QueryRevisions {
            library: 1,
            ..Default::default()
        };
        let changed = now + Duration::from_millis(100);
        owner.sync(Some(1), revisions, changed);
        assert!(!first.ticket.is_current());
        assert!(owner.prepare(&mut session, true, changed).is_none());
        // A late duplicate timer callback must not retire already submitted I/O.
        assert!(!owner.submit(first.read, &session, environment(true)));
        assert!(owner.prepare(&mut session, true, changed).is_none());
        assert!(matches!(
            finish(&mut owner, &mut session, first.read, true),
            ReadCompletion::Retired
        ));
        let next = owner.prepare(&mut session, true, changed).unwrap();
        assert_eq!(next.delay, Duration::from_millis(80));
        assert!(owner.submit(
            next.read,
            &session,
            ReadEnvironment {
                visible: true,
                revisions,
            },
        ));
    }

    #[test]
    fn mismatched_response_revisions_restore_demand_without_overwriting_session() {
        let now = Instant::now();
        let mut owner = MetadataReadCoordinator::default();
        let mut session = MetadataSession::default();
        request(&mut owner, &mut session, 1, now);
        let first = owner.prepare(&mut session, true, now).unwrap();
        assert!(owner.submit(first.read, &session, environment(true)));
        let mut changed = response();
        changed.revisions.library = 1;
        assert!(matches!(
            owner.finish(
                first.read,
                &mut session,
                environment(true),
                changed,
                MetadataValues::default().as_draft(),
                || "wrong response".into(),
            ),
            ReadCompletion::Retired
        ));
        assert!(session.accepts_load(first.read.token));
        assert!(owner.prepare(&mut session, true, now).is_some());
    }

    #[test]
    fn clearing_selection_while_reading_drops_demand_only_after_worker_finishes() {
        let now = Instant::now();
        let mut owner = MetadataReadCoordinator::default();
        let mut session = MetadataSession::default();
        request(&mut owner, &mut session, 1, now);
        let first = owner.prepare(&mut session, true, now).unwrap();
        assert!(owner.submit(first.read, &session, environment(true)));
        session.clear_selection();
        owner.sync(None, QueryRevisions::default(), now);
        assert!(matches!(
            finish(&mut owner, &mut session, first.read, true),
            ReadCompletion::Retired
        ));
        assert!(owner.prepare(&mut session, true, now).is_none());
        request(&mut owner, &mut session, 2, now);
        assert!(owner.prepare(&mut session, true, now).is_some());
    }
}
