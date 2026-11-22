//! Query identity and selected revision contracts.
use super::{
    AlbumKey, AlbumSort, BatchContinuation, LibrarySort, PlaylistEntryCursor, QueueCursor,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Query {
    OtherTracksPage {
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: LibrarySort,
        offset: usize,
        limit: usize,
    },
    OtherTracksLocate {
        track_id: i64,
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: LibrarySort,
    },
    OtherTracksCount,
    Albums {
        query: Option<String>,
        sort: AlbumSort,
        offset: usize,
        limit: usize,
    },
    AlbumTracks {
        key: AlbumKey,
        query: Option<String>,
        offset: usize,
        limit: usize,
    },
    LibraryPage {
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: LibrarySort,
        offset: usize,
        limit: usize,
    },
    DirectoryPage {
        path: PathBuf,
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        offset: usize,
        limit: usize,
    },
    PlaylistSummaries {
        query: Option<String>,
        offset: usize,
        limit: usize,
    },
    PlaylistEntries {
        playlist_id: i64,
        query: Option<String>,
        offset: usize,
        limit: usize,
    },
    PlaylistEntryLocations {
        playlist_id: i64,
        query: Option<String>,
        entry_ids: Vec<i64>,
    },
    Track {
        track_id: i64,
    },
    LibraryStats,
    TrackBatch {
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        after: Option<i64>,
        limit: usize,
        expected: Option<crate::response::QueryRevisions>,
    },
    PlaylistSummaryBatch {
        query: Option<String>,
        after: Option<i64>,
        limit: usize,
        expected: Option<crate::response::QueryRevisions>,
    },
    PlaylistEntryBatch {
        playlist_id: i64,
        query: Option<String>,
        after: Option<BatchContinuation<PlaylistEntryCursor>>,
        limit: usize,
        expected: Option<crate::response::QueryRevisions>,
    },
    LibraryRootPage {
        path: PathBuf,
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: LibrarySort,
        offset: usize,
        limit: usize,
    },
    LibraryLocate {
        track_id: i64,
        path: PathBuf,
        tree: bool,
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        sort: LibrarySort,
    },
    LibraryRootCounts {
        roots: Vec<PathBuf>,
    },
    LibraryRootStats {
        roots: Vec<PathBuf>,
    },
    ScanTask {
        id: u64,
    },
    QueuePage {
        query: Option<String>,
        offset: usize,
        limit: usize,
    },
    QueueBatch {
        query: Option<String>,
        after: Option<BatchContinuation<QueueCursor>>,
        limit: usize,
        expected: Option<crate::response::QueryRevisions>,
    },
    QueueEntries {
        query: Option<String>,
        queue_ids: Vec<u64>,
    },
    PlaylistEntry {
        entry_id: i64,
    },
    DuplicateGroups {
        offset: usize,
        limit: usize,
    },
    DuplicateMembers {
        fingerprint: String,
        offset: usize,
        limit: usize,
    },
    RelocationTask {
        id: u64,
    },
    DuplicateMemberSelection {
        fingerprint: String,
        track_ids: Vec<i64>,
    },
    ProcessingTask {
        id: u64,
    },
    PlaylistSummary {
        playlist_id: i64,
    },
    LibraryMatchCounts {
        roots: Vec<PathBuf>,
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
        include_other: bool,
    },
}

/// Selected equality tokens, owned by the query rather than its consumers.
/// Unselected domains are zeroed, so scheduling and acceptance share one contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryStamp {
    revisions: crate::response::QueryRevisions,
    kind: QueryStampKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum QueryStampKind {
    #[default]
    Rows,
    AlbumTracks,
}

/// Independent reasons a projection loses authority, not a refresh policy.
/// Owners still decide how to retain display rows, counts and business state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryImpact {
    pub identity_changed: bool,
    /// A payload-only dependency changed, even if coordinates also changed.
    pub payload_changed: bool,
    /// Row membership, ordering or count authority may have changed.
    pub coordinates_changed: bool,
}

impl QueryImpact {
    pub fn rows_changed(self) -> bool {
        self.identity_changed || self.payload_changed || self.coordinates_changed
    }
}

impl QueryStamp {
    pub fn revisions(self) -> crate::response::QueryRevisions {
        self.revisions
    }

    /// Compare selected dependencies for the same query identity. Identity is
    /// owned separately by QueryTracker; zero is a valid revision token.
    pub fn impact(self, next: Self) -> QueryImpact {
        let mut previous_coordinates = self.revisions;
        let mut next_coordinates = next.revisions;
        let mut payload_changed = previous_coordinates.bitrate != next_coordinates.bitrate;
        previous_coordinates.bitrate = 0;
        next_coordinates.bitrate = 0;
        if self.kind == QueryStampKind::AlbumTracks && next.kind == QueryStampKind::AlbumTracks {
            // Album membership/order/counts are album-owned. Library updates
            // refresh member statistics without retiring those coordinates.
            payload_changed |= previous_coordinates.library != next_coordinates.library;
            previous_coordinates.library = 0;
            next_coordinates.library = 0;
        }
        QueryImpact {
            identity_changed: false,
            payload_changed,
            coordinates_changed: self.kind != next.kind || previous_coordinates != next_coordinates,
        }
    }

    pub fn continuation_scope(self, query: Option<&str>, playlist_id: Option<i64>) -> [u8; 32] {
        let mut hash = blake3::Hasher::new();
        hash.update(b"rivu occurrence continuation v2");
        hash.update(&[u8::from(playlist_id.is_some())]);
        hash.update(&playlist_id.unwrap_or_default().to_le_bytes());
        // Callers supply the same normalized search text used by SQL.
        hash.update(query.unwrap_or_default().as_bytes());
        hash.update(&self.revisions.library.to_le_bytes());
        hash.update(&self.revisions.bitrate.to_le_bytes());
        hash.update(&self.revisions.queue.to_le_bytes());
        for scope in [
            self.revisions.playlist_entries,
            self.revisions.playlist_metadata,
        ] {
            hash.update(&[u8::from(scope.is_some())]);
            let scope = scope.unwrap_or_default();
            hash.update(&scope.playlist_id.to_le_bytes());
            hash.update(&scope.revision.to_le_bytes());
        }
        *hash.finalize().as_bytes()
    }
}

/// Borrowed projection identity: demand never allocates or changes the data key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryIdentity<'a> {
    OtherTracksPage(&'a Option<String>, Option<bool>, Option<bool>, LibrarySort),
    OtherTracksLocate(
        i64,
        &'a Option<String>,
        Option<bool>,
        Option<bool>,
        LibrarySort,
    ),
    OtherTracksCount,
    Albums(&'a Option<String>, AlbumSort),
    AlbumTracks(&'a AlbumKey, &'a Option<String>),
    LibraryPage(&'a Option<String>, Option<bool>, Option<bool>, LibrarySort),
    DirectoryPage(
        &'a std::path::Path,
        &'a Option<String>,
        Option<bool>,
        Option<bool>,
    ),
    PlaylistSummaries(&'a Option<String>),
    PlaylistEntries(i64, &'a Option<String>),
    PlaylistEntryLocations(i64, &'a [i64], &'a Option<String>),
    Track(i64),
    LibraryStats,
    TrackBatch(&'a Option<String>, Option<bool>, Option<bool>),
    PlaylistSummaryBatch(&'a Option<String>),
    PlaylistEntryBatch(i64, &'a Option<String>),
    LibraryRootPage(
        &'a std::path::Path,
        &'a Option<String>,
        Option<bool>,
        Option<bool>,
        LibrarySort,
    ),
    LibraryLocate(
        i64,
        &'a std::path::Path,
        bool,
        &'a Option<String>,
        Option<bool>,
        Option<bool>,
        LibrarySort,
    ),
    LibraryRootCounts(&'a [PathBuf]),
    LibraryRootStats(&'a [PathBuf]),
    ScanTask(u64),
    QueuePage(&'a Option<String>),
    QueueBatch(&'a Option<String>),
    QueueEntries(&'a [u64], &'a Option<String>),
    PlaylistEntry(i64),
    DuplicateGroups,
    DuplicateMembers(&'a str),
    RelocationTask(u64),
    DuplicateMemberSelection(&'a str, &'a [i64]),
    ProcessingTask(u64),
    PlaylistSummary(i64),
    LibraryMatchCounts(
        &'a [PathBuf],
        &'a Option<String>,
        Option<bool>,
        Option<bool>,
        bool,
    ),
}

impl Query {
    pub fn stamp(&self, revisions: crate::response::QueryRevisions) -> QueryStamp {
        use crate::response::QueryRevisions;
        let mut stamp = QueryRevisions::default();
        match self {
            Self::LibraryPage { .. }
            | Self::TrackBatch { .. }
            | Self::LibraryStats
            | Self::LibraryRootStats { .. } => stamp.library = revisions.library,
            Self::Track { track_id } => {
                stamp.track = revisions.track.filter(|scope| scope.track_id == *track_id);
                if stamp.track.is_none() {
                    stamp.library = revisions.library;
                }
            }
            Self::Albums { .. } => {
                stamp.album = revisions.album;
            }
            Self::AlbumTracks { .. } => {
                stamp.library = revisions.library;
                stamp.album = revisions.album;
            }
            Self::LibraryMatchCounts { .. } => {
                // Matching counts do not depend on row payload or display order.
                stamp.counts = revisions.counts;
                stamp.structure = revisions.structure;
            }
            Self::DirectoryPage { .. }
            | Self::OtherTracksPage { .. }
            | Self::OtherTracksLocate { .. }
            | Self::LibraryRootPage { .. }
            | Self::LibraryLocate { .. } => {
                stamp.library = revisions.library;
                stamp.structure = revisions.structure;
            }
            Self::LibraryRootCounts { .. } | Self::OtherTracksCount => {
                stamp.structure = revisions.structure;
            }
            Self::PlaylistSummaries { .. }
            | Self::PlaylistSummaryBatch { .. }
            | Self::PlaylistSummary { .. } => {
                stamp.playlist = revisions.playlist;
            }
            Self::PlaylistEntries { playlist_id, .. }
            | Self::PlaylistEntryBatch { playlist_id, .. } => {
                stamp.playlist_metadata = revisions
                    .playlist_metadata
                    .filter(|scope| scope.playlist_id == *playlist_id);
                if stamp.playlist_metadata.is_none() {
                    stamp.library = revisions.library;
                }
                stamp.playlist_entries = revisions
                    .playlist_entries
                    .filter(|scope| scope.playlist_id == *playlist_id);
            }
            Self::PlaylistEntryLocations {
                playlist_id, query, ..
            } => {
                stamp.playlist_entries = revisions
                    .playlist_entries
                    .filter(|scope| scope.playlist_id == *playlist_id);
                if query.is_some() {
                    stamp.playlist_metadata = revisions
                        .playlist_metadata
                        .filter(|scope| scope.playlist_id == *playlist_id);
                    if stamp.playlist_metadata.is_none() {
                        stamp.library = revisions.library;
                    }
                }
            }
            Self::PlaylistEntry { .. } => stamp.playlist_entries = revisions.playlist_entries,
            Self::QueuePage { .. } | Self::QueueBatch { .. } | Self::QueueEntries { .. } => {
                stamp.queue = revisions.queue;
            }
            Self::DuplicateGroups { .. } => stamp.duplicate_groups = revisions.duplicate_groups,
            Self::DuplicateMembers { .. } | Self::DuplicateMemberSelection { .. } => {
                stamp.library = revisions.library;
                stamp.duplicate_members = revisions.duplicate_members;
                stamp.queue = revisions.queue;
            }
            Self::ScanTask { .. } | Self::RelocationTask { .. } | Self::ProcessingTask { .. } => {}
        }
        if matches!(
            self,
            Self::LibraryPage { .. }
                | Self::TrackBatch { .. }
                | Self::Track { .. }
                | Self::AlbumTracks { .. }
                | Self::DirectoryPage { .. }
                | Self::OtherTracksPage { .. }
                | Self::LibraryRootPage { .. }
                | Self::PlaylistEntries { .. }
                | Self::PlaylistEntryBatch { .. }
                | Self::QueuePage { .. }
                | Self::QueueBatch { .. }
                | Self::QueueEntries { .. }
                | Self::DuplicateMembers { .. }
                | Self::DuplicateMemberSelection { .. }
        ) {
            stamp.bitrate = revisions.bitrate;
        }
        QueryStamp {
            revisions: stamp,
            kind: if matches!(self, Self::AlbumTracks { .. }) {
                QueryStampKind::AlbumTracks
            } else {
                QueryStampKind::Rows
            },
        }
    }

    /// Pagination demand does not affect query identity.
    pub fn identity(&self) -> QueryIdentity<'_> {
        match self {
            Self::OtherTracksPage {
                query,
                favorite,
                missing,
                sort,
                ..
            } => QueryIdentity::OtherTracksPage(query, *favorite, *missing, *sort),
            Self::OtherTracksLocate {
                track_id,
                query,
                favorite,
                missing,
                sort,
            } => QueryIdentity::OtherTracksLocate(*track_id, query, *favorite, *missing, *sort),
            Self::OtherTracksCount => QueryIdentity::OtherTracksCount,
            Self::Albums { query, sort, .. } => QueryIdentity::Albums(query, *sort),
            Self::AlbumTracks { key, query, .. } => QueryIdentity::AlbumTracks(key, query),
            Self::LibraryPage {
                query,
                favorite,
                missing,
                sort,
                ..
            } => QueryIdentity::LibraryPage(query, *favorite, *missing, *sort),
            Self::DirectoryPage {
                path,
                query,
                favorite,
                missing,
                ..
            } => QueryIdentity::DirectoryPage(path, query, *favorite, *missing),
            Self::PlaylistSummaries { query, .. } => QueryIdentity::PlaylistSummaries(query),
            Self::PlaylistSummary { playlist_id } => QueryIdentity::PlaylistSummary(*playlist_id),
            Self::PlaylistEntries {
                playlist_id, query, ..
            } => QueryIdentity::PlaylistEntries(*playlist_id, query),
            Self::PlaylistEntryLocations {
                playlist_id,
                entry_ids,
                query,
            } => QueryIdentity::PlaylistEntryLocations(*playlist_id, entry_ids, query),
            Self::Track { track_id } => QueryIdentity::Track(*track_id),
            Self::LibraryStats => QueryIdentity::LibraryStats,
            Self::TrackBatch {
                query,
                favorite,
                missing,
                ..
            } => QueryIdentity::TrackBatch(query, *favorite, *missing),
            Self::PlaylistSummaryBatch { query, .. } => QueryIdentity::PlaylistSummaryBatch(query),
            Self::PlaylistEntryBatch {
                playlist_id, query, ..
            } => QueryIdentity::PlaylistEntryBatch(*playlist_id, query),
            Self::LibraryRootPage {
                path,
                query,
                favorite,
                missing,
                sort,
                ..
            } => QueryIdentity::LibraryRootPage(path, query, *favorite, *missing, *sort),
            Self::LibraryLocate {
                track_id,
                path,
                tree,
                query,
                favorite,
                missing,
                sort,
            } => QueryIdentity::LibraryLocate(
                *track_id, path, *tree, query, *favorite, *missing, *sort,
            ),
            Self::LibraryRootCounts { roots } => QueryIdentity::LibraryRootCounts(roots),
            Self::LibraryMatchCounts {
                roots,
                query,
                favorite,
                missing,
                include_other,
            } => {
                QueryIdentity::LibraryMatchCounts(roots, query, *favorite, *missing, *include_other)
            }
            Self::LibraryRootStats { roots } => QueryIdentity::LibraryRootStats(roots),
            Self::ScanTask { id } => QueryIdentity::ScanTask(*id),
            Self::QueuePage { query, .. } => QueryIdentity::QueuePage(query),
            Self::QueueBatch { query, .. } => QueryIdentity::QueueBatch(query),
            Self::QueueEntries { queue_ids, query } => {
                QueryIdentity::QueueEntries(queue_ids, query)
            }
            Self::PlaylistEntry { entry_id } => QueryIdentity::PlaylistEntry(*entry_id),
            Self::DuplicateGroups { .. } => QueryIdentity::DuplicateGroups,
            Self::DuplicateMembers { fingerprint, .. } => {
                QueryIdentity::DuplicateMembers(fingerprint)
            }
            Self::RelocationTask { id } => QueryIdentity::RelocationTask(*id),
            Self::ProcessingTask { id } => QueryIdentity::ProcessingTask(*id),
            Self::DuplicateMemberSelection {
                fingerprint,
                track_ids,
            } => QueryIdentity::DuplicateMemberSelection(fingerprint, track_ids),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::{PlaylistEntryRevision, QueryRevisions, TrackRevision};

    fn impact(query: &Query, previous: QueryRevisions, next: QueryRevisions) -> QueryImpact {
        query.stamp(previous).impact(query.stamp(next))
    }

    fn album_tracks() -> Query {
        Query::AlbumTracks {
            key: AlbumKey {
                directory: "/music".into(),
                title: "Album".into(),
            },
            query: None,
            offset: 0,
            limit: 2,
        }
    }

    #[test]
    fn bitrate_changes_only_selected_payload_dependencies() {
        let previous = QueryRevisions::default();
        let next = previous.with_bitrate_revision(1);
        for query in [
            Query::LibraryPage {
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
                offset: 0,
                limit: 2,
            },
            Query::TrackBatch {
                query: None,
                favorite: None,
                missing: None,
                after: None,
                limit: 2,
                expected: None,
            },
            Query::Track { track_id: 3 },
            album_tracks(),
            Query::DirectoryPage {
                path: "/music".into(),
                query: None,
                favorite: None,
                missing: None,
                offset: 0,
                limit: 2,
            },
            Query::OtherTracksPage {
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
                offset: 0,
                limit: 2,
            },
            Query::LibraryRootPage {
                path: "/music".into(),
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
                offset: 0,
                limit: 2,
            },
            Query::PlaylistEntries {
                playlist_id: 7,
                query: None,
                offset: 0,
                limit: 2,
            },
            Query::PlaylistEntryBatch {
                playlist_id: 7,
                query: None,
                after: None,
                limit: 2,
                expected: None,
            },
            Query::QueuePage {
                query: None,
                offset: 0,
                limit: 2,
            },
            Query::QueueBatch {
                query: None,
                after: None,
                limit: 2,
                expected: None,
            },
            Query::QueueEntries {
                query: None,
                queue_ids: vec![1],
            },
            Query::DuplicateMembers {
                fingerprint: "hash".into(),
                offset: 0,
                limit: 2,
            },
            Query::DuplicateMemberSelection {
                fingerprint: "hash".into(),
                track_ids: vec![1],
            },
        ] {
            assert_ne!(query.stamp(previous), query.stamp(next), "{query:?}");
            let change = impact(&query, previous, next);
            assert!(change.rows_changed() && change.payload_changed, "{query:?}");
            assert!(
                !change.identity_changed && !change.coordinates_changed,
                "{query:?}"
            );
            assert_eq!(change, impact(&query, next, previous), "{query:?}");
        }
        for query in [
            Query::Albums {
                query: None,
                sort: AlbumSort::default(),
                offset: 0,
                limit: 2,
            },
            Query::LibraryStats,
            Query::OtherTracksCount,
            Query::LibraryRootCounts {
                roots: vec!["/music".into()],
            },
            Query::PlaylistSummaries {
                query: None,
                offset: 0,
                limit: 2,
            },
            Query::PlaylistSummaryBatch {
                query: None,
                after: None,
                limit: 2,
                expected: None,
            },
            Query::PlaylistEntry { entry_id: 1 },
            Query::PlaylistEntryLocations {
                playlist_id: 7,
                query: None,
                entry_ids: vec![1],
            },
            Query::PlaylistEntryLocations {
                playlist_id: 7,
                query: Some("needle".into()),
                entry_ids: vec![1],
            },
            Query::LibraryLocate {
                track_id: 1,
                path: "/music".into(),
                tree: true,
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
            },
            Query::OtherTracksLocate {
                track_id: 1,
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
            },
            Query::DuplicateGroups {
                offset: 0,
                limit: 2,
            },
            Query::ScanTask { id: 1 },
            Query::RelocationTask { id: 1 },
            Query::ProcessingTask { id: 1 },
        ] {
            assert_eq!(query.stamp(previous), query.stamp(next), "{query:?}");
            for (from, to) in [(previous, next), (next, previous)] {
                assert_eq!(
                    impact(&query, from, to),
                    QueryImpact::default(),
                    "{query:?}"
                );
            }
        }
    }

    #[test]
    fn match_counts_and_pages_select_distinct_revisions() {
        let revisions = QueryRevisions {
            counts: 1,
            structure: 2,
            library: 3,
            bitrate: 4,
            queue: 5,
            ..Default::default()
        };
        let count_revisions = QueryRevisions {
            counts: 1,
            structure: 2,
            ..Default::default()
        };
        let count = Query::LibraryMatchCounts {
            roots: vec!["/music".into()],
            query: Some("needle".into()),
            favorite: Some(true),
            missing: None,
            include_other: true,
        };
        {
            assert_eq!(count.stamp(revisions).revisions(), count_revisions);
            for changed in [
                count_revisions.with_count_revision(2),
                QueryRevisions {
                    structure: 3,
                    ..count_revisions
                },
            ] {
                let change = impact(&count, revisions, changed);
                assert!(change.coordinates_changed);
                assert!(!change.payload_changed);
            }
        }
        for query in [
            Query::LibraryRootPage {
                path: "/music".into(),
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
                offset: 0,
                limit: 1,
            },
            Query::OtherTracksPage {
                query: None,
                favorite: None,
                missing: None,
                sort: LibrarySort::Id,
                offset: 0,
                limit: 1,
            },
        ] {
            assert_eq!(
                query.stamp(revisions).revisions(),
                QueryRevisions {
                    library: 3,
                    structure: 2,
                    bitrate: 4,
                    ..Default::default()
                }
            );
        }
    }

    #[test]
    fn album_member_payload_and_coordinates_have_independent_authority() {
        let query = album_tracks();
        let initial = QueryRevisions::default();
        let payload = QueryRevisions {
            library: 1,
            ..initial
        };
        let membership = initial.with_album_revision(1);
        for (previous, next) in [(initial, payload), (payload, initial)] {
            let change = impact(&query, previous, next);
            assert!(change.payload_changed && change.rows_changed());
            assert!(!change.coordinates_changed);
        }
        for (previous, next) in [(initial, membership), (membership, initial)] {
            let change = impact(&query, previous, next);
            assert!(change.coordinates_changed && change.rows_changed());
            assert!(!change.payload_changed);
        }
        let both = impact(&query, initial, payload.with_album_revision(1));
        assert!(both.payload_changed && both.coordinates_changed);
    }

    #[test]
    fn count_and_occurrence_queries_keep_their_own_revision_dependencies() {
        let previous = QueryRevisions {
            playlist_entries: Some(PlaylistEntryRevision {
                playlist_id: 7,
                revision: 0,
            }),
            playlist_metadata: Some(PlaylistEntryRevision {
                playlist_id: 7,
                revision: 0,
            }),
            ..Default::default()
        };
        let metadata = QueryRevisions {
            library: 1,
            playlist_metadata: Some(PlaylistEntryRevision {
                playlist_id: 7,
                revision: 1,
            }),
            ..previous
        };
        let unfiltered = Query::PlaylistEntryLocations {
            playlist_id: 7,
            query: None,
            entry_ids: vec![1],
        };
        let filtered = Query::PlaylistEntryLocations {
            playlist_id: 7,
            query: Some("needle".into()),
            entry_ids: vec![1],
        };
        assert!(!impact(&unfiltered, previous, metadata).rows_changed());
        assert!(impact(&filtered, previous, metadata).coordinates_changed);
        let entries = QueryRevisions {
            playlist_entries: Some(PlaylistEntryRevision {
                playlist_id: 7,
                revision: 1,
            }),
            ..previous
        };
        assert!(impact(&unfiltered, previous, entries).coordinates_changed);
        let count = Query::OtherTracksCount;
        let structure = QueryRevisions {
            structure: 1,
            ..previous
        };
        assert!(!impact(&count, previous, metadata).rows_changed());
        assert!(impact(&count, previous, structure).coordinates_changed);
        let query = Query::QueuePage {
            query: Some("needle".into()),
            offset: 0,
            limit: 2,
        };
        assert!(!impact(&query, previous, metadata).rows_changed());
        let queue = QueryRevisions {
            queue: 1,
            ..previous
        };
        assert!(impact(&query, previous, queue).coordinates_changed);
    }

    #[test]
    fn scoped_track_impact_ignores_unrelated_metadata_and_uses_library_fallback() {
        let query = Query::Track { track_id: 3 };
        let previous = QueryRevisions {
            track: Some(TrackRevision {
                track_id: 3,
                revision: 0,
            }),
            ..Default::default()
        };
        let unrelated = QueryRevisions {
            library: 1,
            queue: 1,
            ..previous
        };
        assert!(!impact(&query, previous, unrelated).rows_changed());
        let related = QueryRevisions {
            track: Some(TrackRevision {
                track_id: 3,
                revision: 1,
            }),
            ..previous
        };
        assert!(impact(&query, previous, related).coordinates_changed);
        let fallback = QueryRevisions::default();
        let changed = QueryRevisions {
            library: 1,
            ..fallback
        };
        assert!(impact(&query, fallback, changed).coordinates_changed);
    }

    #[test]
    fn duplicate_payload_change_remains_visible_during_coordinate_changes() {
        let query = Query::DuplicateMembers {
            fingerprint: "hash".into(),
            offset: 0,
            limit: 2,
        };
        let previous = QueryRevisions::default();
        let next = QueryRevisions {
            bitrate: 1,
            queue: 1,
            ..previous
        };
        let change = impact(&query, previous, next);
        assert!(change.payload_changed && change.coordinates_changed && change.rows_changed());
        assert!(!change.identity_changed);
    }
}
