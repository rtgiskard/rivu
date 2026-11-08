use crate::model::{
    Batch, BatchContinuation, ClientSnapshot, ConfigurationState, DatabaseOptimization,
    DirectoryRow, LibraryRootCount, LibraryRow, LibrarySnapshot, LibraryStats, Page, PlaybackState,
    PlaylistEntryCursor, PlaylistEntryLocation, PlaylistEntryRow, PlaylistSummary, QueueCursor,
    QueueRow, QueueState, SystemState, Track,
};
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};

/// Independent query/state-section invalidation domains. Tokens are compared for equality, never ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateRevisions {
    pub playback: u64,
    pub queue: u64,
    pub library: u64,
    /// System-section revision; distinct from `SystemState::revision`, which is a publication sequence.
    pub system: u64,
    pub configuration: u64,
    pub processing: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSections {
    pub playback: bool,
    pub queue: bool,
    pub library: bool,
    pub system: bool,
    pub configuration: bool,
    pub processing: bool,
}

impl StateSections {
    pub const ALL: Self = Self {
        playback: true,
        queue: true,
        library: true,
        system: true,
        configuration: true,
        processing: true,
    };
    pub fn is_empty(self) -> bool {
        !self.playback
            && !self.queue
            && !self.library
            && !self.system
            && !self.configuration
            && !self.processing
    }

    pub fn intersection(self, other: Self) -> Self {
        Self {
            playback: self.playback && other.playback,
            queue: self.queue && other.queue,
            library: self.library && other.library,
            system: self.system && other.system,
            configuration: self.configuration && other.configuration,
            processing: self.processing && other.processing,
        }
    }
}

/// Domains and previously applied tokens for a selective state watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateWatch {
    pub revisions: StateRevisions,
    pub sections: StateSections,
}

impl From<StateRevisions> for StateWatch {
    fn from(revisions: StateRevisions) -> Self {
        Self {
            revisions,
            sections: StateSections::ALL,
        }
    }
}

impl StateRevisions {
    pub fn changed_since(self, previous: Self) -> StateSections {
        StateSections {
            playback: self.playback != previous.playback,
            queue: self.queue != previous.queue,
            library: self.library != previous.library,
            system: self.system != previous.system,
            configuration: self.configuration != previous.configuration,
            processing: self.processing != previous.processing,
        }
    }
}

/// A playlist's content/lifetime token, independent of the summary catalog.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistEntryRevision {
    pub playlist_id: i64,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackRevision {
    pub track_id: i64,
    pub revision: u64,
}

/// Query data revisions are captured atomically with the query result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryRevisions {
    pub library: u64,
    pub structure: u64,
    /// Payload freshness token, selected only by bitrate-bearing queries.
    pub bitrate: u64,
    /// Core-owned album collection token captured with the serialized query.
    pub album: u64,
    pub playlist: u64,
    pub queue: u64,
    pub playlist_entries: Option<PlaylistEntryRevision>,
    pub track: Option<TrackRevision>,
    pub playlist_metadata: Option<PlaylistEntryRevision>,
    pub duplicate_groups: u64,
    pub duplicate_members: u64,
}

impl QueryRevisions {
    pub fn with_album_revision(mut self, revision: u64) -> Self {
        self.album = revision;
        self
    }

    pub fn with_bitrate_revision(mut self, revision: u64) -> Self {
        self.bitrate = revision;
        self
    }

    /// Assemble the common library domains for model and consumer projections.
    pub fn from_library(
        library: u64,
        structure: u64,
        playlist: u64,
        queue: u64,
        duplicate_groups: u64,
        duplicate_members: u64,
    ) -> Self {
        Self {
            library,
            structure,
            playlist,
            queue,
            duplicate_groups,
            duplicate_members,
            ..Self::default()
        }
    }

    /// Playlist presence is authoritative in the content/lifetime map; metadata
    /// alone must never revive a deleted playlist's scope.
    pub fn with_playlist_scope(
        mut self,
        playlist_id: Option<i64>,
        entries: &std::collections::BTreeMap<i64, u64>,
        metadata: &crate::model::MetadataRevisions,
    ) -> Self {
        self.playlist_entries = playlist_id.and_then(|playlist_id| {
            entries
                .get(&playlist_id)
                .map(|&revision| PlaylistEntryRevision {
                    playlist_id,
                    revision,
                })
        });
        self.playlist_metadata = self.playlist_entries.map(|scope| PlaylistEntryRevision {
            playlist_id: scope.playlist_id,
            revision: metadata.playlist_revision(scope.playlist_id, self.library),
        });
        self
    }

    /// Select data scopes without changing query identity or stamp semantics.
    pub fn for_query(
        self,
        query: &crate::model::Query,
        entries: &std::collections::BTreeMap<i64, u64>,
        metadata: &crate::model::MetadataRevisions,
    ) -> Self {
        use crate::model::Query;
        let playlist_id = match query {
            Query::PlaylistEntries { playlist_id, .. }
            | Query::PlaylistEntryBatch { playlist_id, .. }
            | Query::PlaylistEntryLocations { playlist_id, .. } => Some(*playlist_id),
            _ => None,
        };
        let mut revisions = self.with_playlist_scope(playlist_id, entries, metadata);
        revisions.track = match query {
            Query::Track { track_id } => Some(TrackRevision {
                track_id: *track_id,
                revision: metadata.track_revision(*track_id, self.library),
            }),
            _ => None,
        };
        revisions
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ViewResponse {
    Albums(Page<crate::model::AlbumSummary>),
    AlbumTracks(Page<LibraryRow>),
    LibraryPage(Page<LibraryRow>),
    TrackPage(Page<Track>),
    DirectoryPage(Page<DirectoryRow>),
    PlaylistSummaries(Page<PlaylistSummary>),
    PlaylistEntries(Page<PlaylistEntryRow>),
    Track(Option<Track>),
    LibraryStats(LibraryStats),
    TrackBatch(Batch<Track, i64>),
    PlaylistSummaryBatch(Batch<PlaylistSummary, i64>),
    PlaylistEntryBatch(Batch<PlaylistEntryRow, BatchContinuation<PlaylistEntryCursor>>),
    LibraryRootCounts(Vec<LibraryRootCount>),
    LibraryRootStats(Vec<crate::model::LibraryRootStats>),
    OtherTracksCount(usize),
    LibraryLocate(crate::model::LibraryLocateResult),
    ScanTask(crate::model::ScanTaskStatus),
    QueuePage(Page<QueueRow>),
    QueueBatch(Batch<QueueRow, BatchContinuation<QueueCursor>>),
    QueueEntries(Vec<QueueRow>),
    PlaylistEntry(Option<PlaylistEntryLocation>),
    DuplicateGroups(Page<crate::model::DuplicateGroup>),
    DuplicateMembers(Page<crate::model::DuplicateMember>),
    RelocationTask(crate::model::RelocationTaskStatus),
    PlaylistEntryLocations(Vec<PlaylistEntryLocation>),
    ProcessingTask(Option<Box<crate::model::ProcessingTaskStatus>>),
    PlaylistSummary(Option<PlaylistSummary>),
}
/// Final task status, returned only after the owner commits its outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskOutcome {
    Scan(crate::model::ScanTaskStatus),
    Relocation(crate::model::RelocationTaskStatus),
    Processing(Box<crate::model::ProcessingTaskStatus>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryError {
    RevisionChanged,
    Cancelled,
    Message(String),
}

impl From<anyhow::Error> for QueryError {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast::<Self>() {
            Ok(error) => error,
            Err(error) => Self::Message(format!("{error:#}")),
        }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RevisionChanged => {
                formatter.write_str("Query data changed while reading; restart the listing")
            }
            Self::Cancelled => formatter.write_str("Query demand was cancelled"),
            Self::Message(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for QueryError {}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryResponse {
    pub revisions: QueryRevisions,
    pub result: Result<ViewResponse, QueryError>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaybackSnapshot {
    pub playback: PlaybackState,
    pub current_track: Option<Arc<Track>>,
    pub source_generation: u64,
    pub processing_audition: Option<crate::model::ProcessingAudition>,
    pub processing_selection: Option<crate::model::ProcessingAudition>,
}

/// Requested independently from system state, including an authoritative empty task.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProcessingState {
    pub task: Option<Arc<crate::model::ProcessingTaskStatus>>,
}

/// Only requested sections are present; omitted sections must be preserved by clients.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StateResponse {
    pub revisions: StateRevisions,
    pub playback: Option<PlaybackSnapshot>,
    pub queue: Option<QueueState>,
    pub library: Option<LibrarySnapshot>,
    pub system: Option<SystemState>,
    pub configuration: Option<ConfigurationState>,
    pub processing: Option<ProcessingState>,
}

impl StateResponse {
    pub fn sections(&self) -> StateSections {
        StateSections {
            playback: self.playback.is_some(),
            queue: self.queue.is_some(),
            library: self.library.is_some(),
            system: self.system.is_some(),
            configuration: self.configuration.is_some(),
            processing: self.processing.is_some(),
        }
    }
}

/// A client's applied payload and freshness tokens share one instance boundary.
/// Announced tokens never acknowledge sections that have not actually been read.
#[derive(Clone, Debug)]
pub struct ObservedState {
    instance_id: u32,
    snapshot: ClientSnapshot,
    applied: StateRevisions,
}

impl ObservedState {
    pub fn new(instance_id: u32) -> Self {
        Self {
            instance_id,
            snapshot: ClientSnapshot::default(),
            applied: StateRevisions::default(),
        }
    }

    pub fn instance_id(&self) -> u32 {
        self.instance_id
    }

    pub fn snapshot(&self) -> &ClientSnapshot {
        &self.snapshot
    }

    pub fn applied(&self) -> StateRevisions {
        self.applied
    }

    /// Apply only a response read from this instance. Callers must serialize the
    /// complete read-and-apply operation; tokens are opaque, not ordered clocks.
    pub fn apply(&mut self, instance_id: u32, response: StateResponse) -> bool {
        if self.instance_id != instance_id {
            return false;
        }
        let current = response.revisions;
        let state = &mut self.snapshot;
        let applied = &mut self.applied;
        if let Some(section) = response.playback {
            state.playback = section.playback;
            if state.current_track != section.current_track {
                state.current_track = section.current_track;
            }
            state.source_generation = section.source_generation;
            state.processing_audition = section.processing_audition;
            state.processing_selection = section.processing_selection;
            applied.playback = current.playback;
        }
        if let Some(section) = response.queue {
            state.queue = section;
            applied.queue = current.queue;
        }
        if let Some(section) = response.library {
            state.library = section;
            applied.library = current.library;
        }
        if let Some(section) = response.system {
            state.system = section;
            applied.system = current.system;
        }
        if let Some(section) = response.configuration {
            state.configuration = section;
            applied.configuration = current.configuration;
        }
        if let Some(section) = response.processing {
            state.processing = section.task;
            applied.processing = current.processing;
        }
        true
    }
}

/// What is known about a command, not a guarantee of transaction rollback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AckStatus {
    Succeeded,
    /// The business owner returned an error; effects may already be committed.
    Failed,
    /// The command was not admitted to its business owner.
    NotAdmitted,
    /// Admission or completion cannot be established. Never replay automatically.
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ack {
    pub status: AckStatus,
    pub error: Option<String>,
    pub revision: u64,
    /// Present when this command accepted a scan, import, recheck or external admission task.
    pub scan_id: Option<u64>,
    /// Present only when this command accepted a manual relocation task.
    pub relocation_id: Option<u64>,
    pub processing_id: Option<u64>,
    /// Present only after successful database optimization and physical worker retirement.
    pub database_optimization: Option<DatabaseOptimization>,
}

impl Ack {
    pub fn from_result(result: anyhow::Result<()>, revision: u64) -> Self {
        match result {
            Ok(()) => Self::new(AckStatus::Succeeded, None, revision),
            Err(error) => Self::new(AckStatus::Failed, Some(format!("{error:#}")), revision),
        }
    }

    pub fn new(status: AckStatus, error: Option<String>, revision: u64) -> Self {
        Self {
            status,
            error,
            revision,
            scan_id: None,
            relocation_id: None,
            processing_id: None,
            database_optimization: None,
        }
    }

    pub fn is_success(&self) -> bool {
        self.status == AckStatus::Succeeded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_read_does_not_acknowledge_unread_concurrent_changes() {
        let applied = StateRevisions {
            playback: 1,
            queue: 1,
            library: 1,
            system: 1,
            configuration: 1,
            processing: 1,
        };
        let mut observed = ObservedState {
            instance_id: 7,
            snapshot: ClientSnapshot::default(),
            applied,
        };
        let notified = StateRevisions {
            playback: 2,
            ..applied
        };
        let requested = notified.changed_since(applied);
        // Queue changes after the notification but before the playback-only read.
        let response = StateResponse {
            revisions: StateRevisions {
                queue: 2,
                ..notified
            },
            playback: Some(PlaybackSnapshot {
                playback: PlaybackState {
                    position: 8.0,
                    ..PlaybackState::default()
                },
                current_track: None,
                source_generation: 0,
                processing_audition: None,
                processing_selection: None,
            }),
            ..StateResponse::default()
        };
        assert_eq!(response.sections(), requested);
        let next = response.revisions;
        assert!(observed.apply(7, response));
        assert_eq!(observed.snapshot().playback.position, 8.0);
        assert_eq!(
            next.changed_since(observed.applied()),
            StateSections {
                queue: true,
                ..StateSections::default()
            }
        );
    }

    #[test]
    fn runtime_and_configuration_reads_preserve_omitted_state_and_unread_tokens() {
        let mut observed = ObservedState::new(7);
        observed.apply(
            7,
            StateResponse {
                configuration: Some(ConfigurationState {
                    config: Arc::new(rivu_format::config::Config {
                        query_batch_size: 37,
                        ..Default::default()
                    }),
                    config_path: "/existing/config.toml".into(),
                }),
                ..StateResponse::default()
            },
        );
        let retained_config = Arc::clone(&observed.snapshot().configuration.config);
        let current = StateRevisions {
            system: 2,
            configuration: 3,
            ..StateRevisions::default()
        };
        let runtime = StateResponse {
            revisions: current,
            system: Some(SystemState {
                selected_device: Some("runtime output".into()),
                ..SystemState::default()
            }),
            ..StateResponse::default()
        };
        observed.apply(7, runtime);
        assert!(Arc::ptr_eq(
            &observed.snapshot().configuration.config,
            &retained_config
        ));
        assert_eq!(
            observed.snapshot().configuration.config_path,
            std::path::Path::new("/existing/config.toml")
        );
        assert_eq!(
            current.changed_since(observed.applied()),
            StateSections {
                configuration: true,
                ..StateSections::default()
            }
        );

        let configuration = StateResponse {
            revisions: StateRevisions {
                system: 4,
                ..current
            },
            configuration: Some(ConfigurationState {
                config: Arc::new(rivu_format::config::Config {
                    query_batch_size: 91,
                    ..Default::default()
                }),
                config_path: "/new/config.toml".into(),
            }),
            ..StateResponse::default()
        };
        let next = configuration.revisions;
        observed.apply(7, configuration);
        assert_eq!(
            observed.snapshot().configuration.config.query_batch_size,
            91
        );
        assert_eq!(
            observed.snapshot().system.selected_device.as_deref(),
            Some("runtime output")
        );
        assert_eq!(
            next.changed_since(observed.applied()),
            StateSections {
                system: true,
                ..StateSections::default()
            }
        );
    }

    #[test]
    fn missing_sections_and_previous_instance_never_advance_applied_state() {
        let mut observed = ObservedState::new(8);
        let revisions = StateRevisions {
            queue: 9,
            system: 6,
            ..Default::default()
        };
        assert!(!observed.apply(
            7,
            StateResponse {
                revisions,
                system: Some(SystemState {
                    last_error: Some("old instance".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }
        ));
        assert_eq!(observed.instance_id(), 8);
        assert_eq!(observed.applied(), StateRevisions::default());
        assert!(observed.snapshot().system.last_error.is_none());
        assert!(observed.apply(
            8,
            StateResponse {
                revisions,
                ..Default::default()
            }
        ));
        assert_eq!(observed.applied(), StateRevisions::default());
        assert_eq!(
            revisions.changed_since(observed.applied()),
            StateSections {
                queue: true,
                system: true,
                ..Default::default()
            }
        );
    }
}
