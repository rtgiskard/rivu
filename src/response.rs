use crate::{
    model::{
        DirectoryPage, HistoryEntry, LibraryPage, LibraryStats, PlaylistEntryPage,
        PlaylistSummaryPage, Track, TrackPage,
    },
    projection::ClientSnapshot,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ViewResponse {
    LibraryPage(LibraryPage),
    TrackPage(TrackPage),
    DirectoryPage(DirectoryPage),
    PlaylistSummaries(PlaylistSummaryPage),
    PlaylistEntries(PlaylistEntryPage),
    Track(Option<Track>),
    LibraryStats(LibraryStats),
    History {
        offset: usize,
        rows: Vec<HistoryEntry>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub state: ClientSnapshot,
    #[serde(default)]
    pub view: Option<ViewResponse>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ack {
    pub ok: bool,
    pub error: Option<String>,
    pub revision: u64,
}
