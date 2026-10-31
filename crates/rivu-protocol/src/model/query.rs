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
    TrackPage {
        query: Option<String>,
        favorite: Option<bool>,
        missing: Option<bool>,
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
}

/// Selected equality tokens, owned by the query rather than its consumers.
/// Unselected domains are zeroed, so scheduling and acceptance share one contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryStamp(crate::response::QueryRevisions);

impl QueryStamp {
    pub fn revisions(self) -> crate::response::QueryRevisions {
        self.0
    }

    pub fn continuation_scope(self, query: Option<&str>, playlist_id: Option<i64>) -> [u8; 32] {
        let mut hash = blake3::Hasher::new();
        hash.update(b"rivu occurrence continuation v2");
        hash.update(&[u8::from(playlist_id.is_some())]);
        hash.update(&playlist_id.unwrap_or_default().to_le_bytes());
        // Callers supply the same normalized search text used by SQL.
        hash.update(query.unwrap_or_default().as_bytes());
        hash.update(&self.0.library.to_le_bytes());
        hash.update(&self.0.bitrate.to_le_bytes());
        hash.update(&self.0.queue.to_le_bytes());
        for scope in [self.0.playlist_entries, self.0.playlist_metadata] {
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
    TrackPage(&'a Option<String>, Option<bool>, Option<bool>),
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
}

impl Query {
    pub fn stamp(&self, revisions: crate::response::QueryRevisions) -> QueryStamp {
        use crate::response::QueryRevisions;
        let mut stamp = QueryRevisions::default();
        match self {
            Self::LibraryPage { .. }
            | Self::TrackPage { .. }
            | Self::TrackBatch { .. }
            | Self::LibraryStats
            | Self::LibraryRootStats { .. } => stamp.library = revisions.library,
            Self::Track { track_id } => {
                stamp.track = revisions.track.filter(|scope| scope.track_id == *track_id);
                if stamp.track.is_none() {
                    stamp.library = revisions.library;
                }
            }
            Self::Albums { .. } | Self::AlbumTracks { .. } => {
                stamp.album = revisions.album;
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
            Self::QueuePage { query, .. }
            | Self::QueueBatch { query, .. }
            | Self::QueueEntries { query, .. } => {
                stamp.queue = revisions.queue;
                if query.is_some() {
                    stamp.library = revisions.library;
                }
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
                | Self::TrackPage { .. }
                | Self::TrackBatch { .. }
                | Self::Track { .. }
                | Self::Albums { .. }
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
        QueryStamp(stamp)
    }

    /// Offsets, cursors, page sizes and expected baselines are demand, not identity.
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
            Self::TrackPage {
                query,
                favorite,
                missing,
                ..
            } => QueryIdentity::TrackPage(query, *favorite, *missing),
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
