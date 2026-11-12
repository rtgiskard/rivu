use super::*;

/// Read-side synchronization is independent of the transaction that invalidated it.
#[derive(Default)]
pub(super) struct ProjectionRecovery {
    pub(super) track_total: bool,
    pub(super) playlist_total: bool,
    pub(super) playlist_tokens: bool,
    pub(super) current_track: bool,
    pub(super) in_flight: bool,
    pub(super) request_attempts: Option<ProjectionAttempts>,
}

/// A failed read consumes this request's attempt just as a successful read does.
#[derive(Clone, Copy, Default)]
pub(super) struct ProjectionAttempts {
    pub(super) track_total: bool,
    pub(super) playlist_total: bool,
    pub(super) playlist_tokens: bool,
    pub(super) current_track: bool,
    pub(super) history: bool,
}

#[derive(Clone, Copy)]
pub(super) struct ProjectionDemand {
    pub(super) track_total: bool,
    pub(super) playlist_total: bool,
    pub(super) history: bool,
    pub(super) revisions: QueryRevisions,
}

#[derive(Default)]
pub(super) struct PreparedProjections {
    pub(super) library_total: Option<Result<usize>>,
    pub(super) history: Option<Result<Vec<HistoryEntry>>>,
    pub(super) playlist_total: Option<Result<usize>>,
}

impl PreparedProjections {
    pub(super) fn read(store: &Store, demand: ProjectionDemand) -> Self {
        Self {
            library_total: demand.track_total.then(|| store.track_count()),
            history: demand.history.then(|| store.history(MAX_QUERY_ROWS)),
            playlist_total: demand.playlist_total.then(|| {
                store
                    .playlist_summary_page(None, 0, 0)
                    .map(|page| page.total)
            }),
        }
    }
}

/// A typed failure after a known commit, never evidence of a failed write.
#[derive(Debug)]
pub(super) struct ProjectionSyncError(pub(super) Vec<anyhow::Error>);
impl std::fmt::Display for ProjectionSyncError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Catalog write committed, but refreshing views failed: ")?;
        for (index, error) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "{error:#}")?;
        }
        Ok(())
    }
}
impl std::error::Error for ProjectionSyncError {}

impl Core {
    pub(super) fn projection_demand(&self) -> ProjectionDemand {
        ProjectionDemand {
            track_total: self.projections.track_total,
            playlist_total: self.projections.playlist_total,
            history: self.transport.history_pending,
            revisions: self.state.query_revisions(),
        }
    }

    pub(super) fn accept_prepared_projections(
        &mut self,
        prepared: PreparedProjections,
    ) -> Result<()> {
        if let Some(attempted) = &mut self.projections.request_attempts {
            attempted.track_total |= prepared.library_total.is_some();
            attempted.playlist_total |= prepared.playlist_total.is_some();
            attempted.history |= prepared.history.is_some();
        }
        let mut errors = Vec::new();
        if let Some(total) = prepared.library_total {
            match total {
                Ok(total) => {
                    self.state.library.track_total = total;
                    self.projections.track_total = false;
                }
                Err(error) => errors.push(
                    error.context("Refreshing library count failed; previous count retained"),
                ),
            }
        }
        if let Some(history) = prepared.history {
            match history {
                Ok(history) => self.replace_history(history),
                Err(error) => errors
                    .push(error.context("Refreshing history failed; previous history retained")),
            }
        }
        if let Some(total) = prepared.playlist_total {
            match total {
                Ok(total) => {
                    self.state.library.playlist_total = total;
                    self.projections.playlist_total = false;
                }
                Err(error) => errors.push(
                    error.context("Refreshing playlist count failed; previous count retained"),
                ),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ProjectionSyncError(errors).into())
        }
    }

    pub(super) fn recover_projections_on_demand(&mut self) {
        if self.database_maintenance_active() || self.database_unavailable {
            return;
        }
        let attempted = self.projections.request_attempts.unwrap_or_default();
        if self.projections.playlist_tokens
            && !attempted.playlist_tokens
            && self.recover_playlist_tokens().is_ok()
        {
            self.publish();
        }
        // Metadata recovery reuses the queue control gate, without rereading its
        // structure. Never replay the catalog command that dirtied this snapshot.
        if self.projections.current_track
            && !attempted.current_track
            && self.refresh_current_track().is_ok()
        {
            self.publish();
        }
        let mut demand = self.projection_demand();
        demand.track_total &= !attempted.track_total;
        demand.playlist_total &= !attempted.playlist_total;
        demand.history &= !attempted.history;
        if !self.projections.in_flight
            && (demand.track_total || demand.playlist_total || demand.history)
            && let Some(reads) = &self.read_queries
        {
            let (completion, receiver) = oneshot::channel();
            self.projections.in_flight = reads.recover(demand, completion);
            if self.projections.in_flight {
                let sender = self.scan_tx.clone();
                self.executor
                    .spawn(async move {
                        let prepared = receiver.await.unwrap_or_default();
                        let _ = sender
                            .send(ScanEvent::Projections { demand, prepared })
                            .await;
                    })
                    .detach();
            }
        }
    }

    pub(super) fn complete_projection_recovery(
        &mut self,
        demand: ProjectionDemand,
        mut prepared: PreparedProjections,
    ) {
        self.projections.in_flight = false;
        if self.database_maintenance_active() || self.database_unavailable {
            return;
        }
        let current = self.state.query_revisions();
        if !self.transport.history_pending || current.library != demand.revisions.library {
            prepared.history = None;
        }
        if !self.projections.track_total || current.structure != demand.revisions.structure {
            prepared.library_total = None;
        }
        if !self.projections.playlist_total || current.playlist != demand.revisions.playlist {
            prepared.playlist_total = None;
        }
        // Failure remains dirty, but this completion never reschedules itself.
        // A later demand or related producer event grants one more attempt.
        let _ = self.accept_prepared_projections(prepared);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_completion_cannot_replace_newer_catalog_projections_or_clear_their_dirtiness() {
        let (_directory, mut core) = super::super::tests::fixture_with_database(Some(10.0));
        core.projections.track_total = true;
        core.projections.playlist_total = true;
        core.transport.history_pending = true;
        let demand = core.projection_demand();
        core.remove_tracks(vec![2]).unwrap();
        let playlist = core
            .store
            .create_playlist_with_tracks("Newer", &[1])
            .unwrap();
        core.playlist_created(playlist, true).unwrap();
        let total = core.state.library.track_total;
        let playlists = core.state.library.playlist_total;
        let history = Arc::clone(&core.state.library.history);
        core.projections.track_total = true;
        core.projections.playlist_total = true;
        core.transport.history_pending = true;
        core.projections.in_flight = true;
        core.complete_projection_recovery(
            demand,
            PreparedProjections {
                library_total: Some(Ok(999)),
                history: Some(Ok(Vec::new())),
                playlist_total: Some(Ok(999)),
            },
        );
        assert_eq!(core.state.library.track_total, total);
        assert_eq!(core.state.library.playlist_total, playlists);
        assert!(Arc::ptr_eq(&core.state.library.history, &history));
        assert!(
            core.projections.track_total
                && core.projections.playlist_total
                && core.transport.history_pending
        );
        assert!(!core.projections.in_flight);
    }

    #[test]
    fn late_completion_keeps_synchronously_satisfied_domains_at_the_same_revision() {
        let (_directory, mut core) = super::super::tests::fixture_with_database(Some(10.0));
        core.store.mark_played(1, 42).unwrap();
        core.projections.track_total = true;
        core.projections.playlist_total = true;
        core.transport.history_pending = true;
        let demand = core.projection_demand();
        core.projections.in_flight = true;
        core.refresh_catalog_projections().unwrap();
        assert_eq!(core.state.query_revisions(), demand.revisions);
        assert!(core.projections.in_flight);
        let history = Arc::clone(&core.state.library.history);
        core.complete_projection_recovery(
            demand,
            PreparedProjections {
                library_total: Some(Ok(999)),
                history: Some(Ok(Vec::new())),
                playlist_total: Some(Ok(999)),
            },
        );
        assert_eq!(core.state.library.track_total, 4);
        assert_eq!(core.state.library.playlist_total, 0);
        assert!(Arc::ptr_eq(&core.state.library.history, &history));
        assert_eq!(core.state.library.history[0].played_at, 42);
        assert!(!core.projections.in_flight);
    }
}
