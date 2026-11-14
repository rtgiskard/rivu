/// Semantic changes from one committed catalog transaction. Probe-only writes
/// do not imply visible Track, ordering, or duplicate changes.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct CatalogChanges {
    /// Any persisted track write, including probe state.
    pub changed: bool,
    /// Effective full Track values, including library ordering fields.
    pub library: bool,
    /// Count membership/search facts, independent of row values and ordering.
    pub counts: bool,
    /// Effective queue-row values changed for an already queued identity.
    pub queue: bool,
    /// History membership or effective entry values changed.
    pub history: bool,
    /// Album membership, ordering, member rows, or representative values changed.
    pub albums: bool,
    /// Track insertion or a changed path/CUE identity; missing is not removal.
    pub structure: bool,
    /// Ordinary fingerprint membership or present/missing grouping facts.
    pub duplicate_groups: bool,
    /// Size/mtime verification facts for fingerprinted ordinary members.
    pub duplicate_members: bool,
    /// Whether the explicitly observed current identity changed. None means the
    /// producer did not observe one; consumers conservatively use library impact.
    pub(crate) current_track: Option<bool>,
    /// Successfully scanned new identities excluded by the duration threshold.
    pub skipped_short_tracks: usize,
}

/// Shared dependency rules; producers report facts and consumers publish only
/// affected tokens, caches and projections rather than requesting a full reload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CatalogDependencies {
    pub library_revision: bool,
    pub count_revision: bool,
    pub structure_revision: bool,
    pub queue_revision: bool,
    pub album_revision: bool,
    pub duplicate_group_revision: bool,
    pub duplicate_member_revision: bool,
    pub refresh_history: bool,
    pub refresh_track_total: bool,
    pub clear_track_cache: bool,
    pub clear_catalog_counts: bool,
    pub clear_queue_count: bool,
    pub clear_playlist_count: bool,
    pub clear_duplicate_group_count: bool,
}

impl CatalogChanges {
    pub(crate) fn dependencies(self, playlist_created: bool) -> CatalogDependencies {
        CatalogDependencies {
            library_revision: self.library,
            count_revision: self.counts || self.structure,
            structure_revision: self.structure,
            queue_revision: self.queue,
            album_revision: self.albums,
            duplicate_group_revision: self.duplicate_groups,
            duplicate_member_revision: self.duplicate_members,
            refresh_history: self.history,
            refresh_track_total: self.structure,
            clear_track_cache: self.library,
            clear_catalog_counts: self.counts || self.structure,
            clear_queue_count: self.queue,
            clear_playlist_count: self.library || playlist_created,
            clear_duplicate_group_count: self.duplicate_groups,
        }
    }
}
