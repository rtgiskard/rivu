use crate::{
    model::{
        Batch, DirectoryRow, LibraryRow, LibrarySnapshot, LibraryStats, Page, PlaybackState,
        PlaylistEntryCursor, PlaylistEntryRow, PlaylistSummary, QueueState, SystemState, Track,
    },
    projection::ClientSnapshot,
};
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};

/// Independent invalidation domains. Tokens are compared for equality, never ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateRevisions {
    pub playback: u64,
    pub queue: u64,
    pub library: u64,
    pub system: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSections {
    pub playback: bool,
    pub queue: bool,
    pub library: bool,
    pub system: bool,
}

impl StateSections {
    pub const ALL: Self = Self {
        playback: true,
        queue: true,
        library: true,
        system: true,
    };
    pub fn is_empty(self) -> bool {
        !self.playback && !self.queue && !self.library && !self.system
    }
}

impl StateRevisions {
    pub fn changed_since(self, previous: Self) -> StateSections {
        StateSections {
            playback: self.playback != previous.playback,
            queue: self.queue != previous.queue,
            library: self.library != previous.library,
            system: self.system != previous.system,
        }
    }

    /// Advance only sections actually read; other tokens may refer to unseen changes.
    pub fn apply_sections(&mut self, current: Self, sections: StateSections) {
        if sections.playback {
            self.playback = current.playback;
        }
        if sections.queue {
            self.queue = current.queue;
        }
        if sections.library {
            self.library = current.library;
        }
        if sections.system {
            self.system = current.system;
        }
    }
}

/// Query data revisions are captured atomically with the query result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryRevisions {
    pub library: u64,
    pub structure: u64,
    pub playlist: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ViewResponse {
    LibraryPage(Page<LibraryRow>),
    TrackPage(Page<Track>),
    DirectoryPage(Page<DirectoryRow>),
    PlaylistSummaries(Page<PlaylistSummary>),
    PlaylistEntries(Page<PlaylistEntryRow>),
    Track(Option<Track>),
    LibraryStats(LibraryStats),
    TrackBatch(Batch<Track, i64>),
    PlaylistSummaryBatch(Batch<PlaylistSummary, i64>),
    PlaylistEntryBatch(Batch<PlaylistEntryRow, PlaylistEntryCursor>),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryError {
    RevisionChanged,
    Message(String),
}

impl QueryError {
    pub fn from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        if message.starts_with("Query data changed while reading") {
            Self::RevisionChanged
        } else {
            Self::Message(message)
        }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RevisionChanged => {
                formatter.write_str("Query data changed while reading; restart the listing")
            }
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
}

/// Only requested sections are present; omitted sections must be preserved by clients.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StateResponse {
    pub revisions: StateRevisions,
    pub playback: Option<PlaybackSnapshot>,
    pub queue: Option<QueueState>,
    pub library: Option<LibrarySnapshot>,
    pub system: Option<SystemState>,
}

impl StateResponse {
    pub fn sections(&self) -> StateSections {
        StateSections {
            playback: self.playback.is_some(),
            queue: self.queue.is_some(),
            library: self.library.is_some(),
            system: self.system.is_some(),
        }
    }

    pub fn apply_to(self, state: &mut ClientSnapshot) {
        if let Some(section) = self.playback {
            state.playback = section.playback;
            state.current_track = section.current_track;
        }
        if let Some(section) = self.queue {
            state.queue = section;
        }
        if let Some(section) = self.library {
            state.library = section;
        }
        if let Some(section) = self.system {
            state.system = section;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ack {
    pub ok: bool,
    pub error: Option<String>,
    pub revision: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_read_does_not_acknowledge_unread_concurrent_changes() {
        let mut applied = StateRevisions {
            playback: 1,
            queue: 1,
            library: 1,
            system: 1,
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
            }),
            ..StateResponse::default()
        };
        assert_eq!(response.sections(), requested);
        applied.apply_sections(response.revisions, response.sections());
        let mut snapshot = ClientSnapshot::default();
        let next = response.revisions;
        response.apply_to(&mut snapshot);
        assert_eq!(snapshot.playback.position, 8.0);
        assert_eq!(
            next.changed_since(applied),
            StateSections {
                queue: true,
                ..StateSections::default()
            }
        );
    }
}
