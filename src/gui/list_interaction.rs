//! Shared list interaction payloads and snapshot-based validity checks.

use super::{library::LibraryNode, playlists, queue};
use crate::projection::GuiSnapshot;
use gpui::{Pixels, Point};
use rivu_protocol::model::Command;

#[derive(Clone, Copy)]
pub(super) enum DragDestination {
    Queue,
    Playlist(i64),
    QueueAt {
        index: usize,
        revision: u64,
    },
    PlaylistAt {
        playlist_id: i64,
        index: usize,
        revision: u64,
    },
}

impl DragDestination {
    pub(super) fn playlist_id(self) -> Option<i64> {
        match self {
            Self::Playlist(id)
            | Self::PlaylistAt {
                playlist_id: id, ..
            } => Some(id),
            Self::Queue | Self::QueueAt { .. } => None,
        }
    }

    pub(super) fn current(self, state: &GuiSnapshot) -> bool {
        match self {
            Self::Queue => true,
            Self::QueueAt { revision, .. } => state.queue.revision == revision,
            Self::Playlist(id) => state.library.playlist_entry_revisions.contains_key(&id),
            Self::PlaylistAt {
                playlist_id,
                revision,
                ..
            } => state.library.playlist_entry_revisions.get(&playlist_id) == Some(&revision),
        }
    }
}

#[derive(Clone)]
pub(super) enum TrackSource {
    Tracks(Vec<i64>),
    Album(rivu_protocol::model::AlbumKey),
}

pub(super) struct ContentDrag {
    source: std::cell::OnceCell<TrackSource>,
}

impl ContentDrag {
    pub(super) fn new(source: TrackSource) -> Self {
        let drag = Self::pending();
        drag.initialize(source);
        drag
    }
    pub(super) fn pending() -> Self {
        Self {
            source: std::cell::OnceCell::new(),
        }
    }
    pub(super) fn initialize(&self, source: TrackSource) {
        let _ = self.source.set(source);
    }
    pub(super) fn source(&self) -> Option<&TrackSource> {
        self.source.get()
    }
}

pub(super) enum ListDrag {
    Queue(queue::QueueDrag),
    Playlist(playlists::PlaylistDrag),
}

impl ListDrag {
    pub(super) fn queue(&self) -> Option<&queue::QueueDrag> {
        match self {
            Self::Queue(drag) => Some(drag),
            Self::Playlist(_) => None,
        }
    }

    pub(super) fn playlist(&self) -> Option<&playlists::PlaylistDrag> {
        match self {
            Self::Playlist(drag) => Some(drag),
            Self::Queue(_) => None,
        }
    }

    pub(super) fn can_transfer(&self, state: &GuiSnapshot, destination: DragDestination) -> bool {
        destination.current(state)
            && match self {
                Self::Queue(drag) => {
                    destination.playlist_id().is_some()
                        && drag.revision == state.queue.revision
                        && drag.entries.get().is_some_and(|rows| !rows.is_empty())
                }
                Self::Playlist(drag) => {
                    destination.playlist_id() != Some(drag.playlist_id)
                        && state
                            .library
                            .playlist_entry_revisions
                            .get(&drag.playlist_id)
                            == Some(&drag.revision)
                        && drag.track_ids.get().is_some_and(|ids| !ids.is_empty())
                }
            }
    }

    pub(super) fn track_ids(&self) -> Vec<i64> {
        match self {
            Self::Queue(drag) => drag
                .entries
                .get()
                .unwrap()
                .iter()
                .map(|row| row.track_id)
                .collect(),
            Self::Playlist(drag) => drag.track_ids.get().unwrap().clone(),
        }
    }
}

pub(super) struct LibraryDrag {
    pub(super) nodes: std::cell::OnceCell<Vec<LibraryNode>>,
}

impl LibraryDrag {
    pub(super) fn can_resolve(&self, selection_pending: bool) -> bool {
        !selection_pending && self.nodes.get().is_some_and(|nodes| !nodes.is_empty())
    }

    pub(super) fn nodes(&self) -> &[LibraryNode] {
        self.nodes
            .get()
            .expect("drag nodes initialized at drag start")
    }
}

pub(super) fn can_transfer(
    value: &dyn std::any::Any,
    state: &GuiSnapshot,
    destination: DragDestination,
    library_selection_pending: bool,
    content_resolution: bool,
) -> bool {
    if let Some(drag) = value.downcast_ref::<ListDrag>() {
        return drag.can_transfer(state, destination);
    }
    if !destination.current(state) {
        return false;
    }
    if let Some(drag) = value.downcast_ref::<LibraryDrag>() {
        return drag.can_resolve(library_selection_pending);
    }
    value.downcast_ref::<ContentDrag>().is_some_and(|drag| {
        drag.source().is_some_and(|source| match source {
            TrackSource::Tracks(ids) => !ids.is_empty(),
            TrackSource::Album(_) => !content_resolution,
        })
    })
}

#[derive(Clone)]
pub(super) struct ContentMenuRequest {
    pub(super) source: TrackSource,
    pub(super) position: Point<Pixels>,
    pub(super) play: Option<Command>,
}

#[derive(Clone)]
pub(super) enum ContentGuard {
    None,
    Library(u64, u64),
    Queue(u64),
    Playlist(i64, u64),
}

impl ContentGuard {
    pub(super) fn current(&self, state: &GuiSnapshot) -> bool {
        match *self {
            Self::None => true,
            Self::Library(library, structure) => {
                state.library.revision == library && state.library.structure_revision == structure
            }
            Self::Queue(revision) => state.queue.revision == revision,
            Self::Playlist(id, revision) => {
                state.library.playlist_entry_revisions.get(&id) == Some(&revision)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivu_protocol::model::{AlbumKey, Query};
    use std::path::PathBuf;

    fn album_query() -> Query {
        Query::AlbumTracks {
            key: AlbumKey {
                directory: PathBuf::from("/album"),
                title: "Album".into(),
            },
            query: None,
            offset: 0,
            limit: rivu_protocol::model::MAX_QUERY_ROWS,
        }
    }
    #[test]
    fn drag_freezes_exactly_once_and_preserves_occurrences() {
        let drag = ContentDrag::pending();
        assert!(drag.source().is_none());
        drag.initialize(TrackSource::Tracks(vec![8, 3, 8]));
        drag.initialize(TrackSource::Tracks(vec![99]));
        assert!(matches!(drag.source(), Some(TrackSource::Tracks(ids)) if ids == &[8, 3, 8]));
    }

    #[test]
    fn library_transfer_requires_initialized_nodes_and_idle_resolution() {
        use super::{LibraryDrag, LibraryNode, can_transfer};
        use crate::{gui::DragDestination, projection::GuiSnapshot};
        let state = GuiSnapshot::default();
        let drag = LibraryDrag {
            nodes: Default::default(),
        };
        assert!(!can_transfer(
            &drag,
            &state,
            DragDestination::Queue,
            false,
            false
        ));
        drag.nodes
            .set(vec![LibraryNode::Track(8), LibraryNode::Track(3)])
            .unwrap();
        assert!(can_transfer(
            &drag,
            &state,
            DragDestination::Queue,
            false,
            false
        ));
        assert!(!can_transfer(
            &drag,
            &state,
            DragDestination::Queue,
            true,
            false
        ));
        assert!(!can_transfer(
            &drag,
            &state,
            DragDestination::QueueAt {
                index: 0,
                revision: 1
            },
            false,
            false
        ));
        let empty = LibraryDrag {
            nodes: std::cell::OnceCell::from(Vec::new()),
        };
        assert!(!can_transfer(
            &empty,
            &state,
            DragDestination::Queue,
            false,
            false
        ));
    }

    #[test]
    fn content_transfer_checks_initialization_destination_and_collection_gate() {
        use super::can_transfer;
        use crate::{gui::DragDestination, projection::GuiSnapshot};
        let state = GuiSnapshot::default();
        let drag = ContentDrag::pending();
        assert!(!can_transfer(
            &drag,
            &state,
            DragDestination::Queue,
            false,
            false
        ));
        drag.initialize(TrackSource::Tracks(vec![8, 3, 8]));
        assert!(can_transfer(
            &drag,
            &state,
            DragDestination::Queue,
            true,
            true
        ));
        assert!(!can_transfer(
            &drag,
            &state,
            DragDestination::Playlist(9),
            false,
            false
        ));
        let empty = ContentDrag::new(TrackSource::Tracks(Vec::new()));
        assert!(!can_transfer(
            &empty,
            &state,
            DragDestination::Queue,
            false,
            false
        ));
        let Query::AlbumTracks { key, .. } = album_query() else {
            unreachable!()
        };
        let album = ContentDrag::new(TrackSource::Album(key));
        assert!(can_transfer(
            &album,
            &state,
            DragDestination::Queue,
            false,
            false
        ));
        assert!(!can_transfer(
            &album,
            &state,
            DragDestination::Queue,
            false,
            true
        ));
    }
}
