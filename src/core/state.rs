use rivu_protocol::model::{GraphExecutionMode, GraphPort};

/// Selected audio source admitted by Core, independent of the latest task and editor.
/// Transport status remains separate: a selected source can be paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessingAudition {
    pub id: u64,
    pub output: Option<GraphPort>,
    /// Cached graph output is Offline; a prepared live runner is Realtime or Hybrid.
    /// Standalone denoise/separation audition has no graph mode.
    pub mode: Option<GraphExecutionMode>,
}

/// Owner of the shared scan/import/admission task and its retained outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaTaskOwner {
    Library,
    Playlist,
    Queue,
}
