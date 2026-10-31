//! Source identity and worker retirement for window-requested full waveforms.
use rivu_protocol::model::Track;
use std::path::PathBuf;

#[derive(Default)]
pub(in crate::gui) struct FullWaveformRequest {
    generation: u64,
    target: Option<FullWaveformTarget>,
    worker: Option<u64>,
}

struct FullWaveformTarget {
    track_id: i64,
    path: PathBuf,
    fingerprint: Option<String>,
    cue_range: Option<(u64, Option<u64>)>,
}

impl FullWaveformTarget {
    fn cue_range(track: &Track) -> Option<(u64, Option<u64>)> {
        track
            .cue
            .as_ref()
            .map(|cue| (cue.start_frame, cue.end_frame))
    }

    fn matches_source(&self, track: &Track) -> bool {
        self.path == track.path
            && self.cue_range == Self::cue_range(track)
            && self.fingerprint == track.fingerprint
    }

    fn matches(&self, track: &Track) -> bool {
        self.track_id == track.id && self.matches_source(track)
    }
}

impl FullWaveformRequest {
    pub(in crate::gui) fn generation(&self) -> u64 {
        self.generation
    }

    pub(in crate::gui) fn begin(&mut self, track: &Track) -> u64 {
        self.invalidate();
        self.target = Some(FullWaveformTarget {
            track_id: track.id,
            path: track.path.clone(),
            fingerprint: track.fingerprint.clone(),
            cue_range: FullWaveformTarget::cue_range(track),
        });
        self.generation
    }

    pub(in crate::gui) fn reserve(&mut self) -> Option<(u64, i64)> {
        if self.worker.is_some() {
            return None;
        }
        let target = self.target.as_ref()?;
        self.worker = Some(self.generation);
        Some((self.generation, target.track_id))
    }

    pub(in crate::gui) fn retire(&mut self, generation: u64) {
        if self.worker == Some(generation) {
            self.worker = None;
        }
    }

    pub(in crate::gui) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.target = None;
    }

    pub(in crate::gui) fn retain_target(
        &mut self,
        track: Option<&Track>,
        visualization_enabled: bool,
    ) {
        if self.target.as_ref().is_some_and(|target| {
            !visualization_enabled || track.is_none_or(|track| !target.matches(track))
        }) {
            self.invalidate();
        }
    }

    pub(in crate::gui) fn saved_hint_changed(&mut self, track: &Track) {
        if self
            .target
            .as_ref()
            .is_some_and(|target| target.matches_source(track))
        {
            self.invalidate();
        }
    }

    pub(in crate::gui) fn accepts(&self, generation: u64, track: Option<&Track>) -> bool {
        self.generation == generation
            && self
                .target
                .as_ref()
                .is_some_and(|target| track.is_some_and(|track| target.matches(track)))
    }
}
