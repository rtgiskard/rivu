use rivu_protocol::model::ProcessingPhase;

/// Shared localization key for task and graph-node processing stages.
pub fn phase_key(phase: ProcessingPhase) -> &'static str {
    match phase {
        ProcessingPhase::Queued => "processing-queued",
        ProcessingPhase::Rendering => "processing-rendering",
        ProcessingPhase::Processing => "processing-processing",
        ProcessingPhase::Complete => "processing-complete",
        ProcessingPhase::Cancelled => "processing-cancelled",
        ProcessingPhase::Failed => "processing-failed",
    }
}
