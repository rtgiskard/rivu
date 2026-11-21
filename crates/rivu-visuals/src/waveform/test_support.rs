use super::*;

pub fn frame(peaks: Vec<Option<f32>>) -> WaveformFrame {
    let counts: Vec<_> = peaks
        .iter()
        .map(|value| u64::from(value.is_some()))
        .collect();
    WaveformFrame {
        revision: 1,
        path: Some(PathBuf::from("track.flac")),
        duration: Some(10.0),
        span_seconds: 10.0,
        bin_frames: 80_000,
        bin_count: peaks.len() as u64,
        identity: Some(WaveformIdentity {
            content: None,
            sample_rate: 8_000,
        }),
        max_peak: peaks.iter().flatten().copied().fold(1.0, f32::max),
        bin_revisions: counts.clone(),
        rms_counts: counts.clone(),
        left_rms_counts: counts.clone(),
        right_rms_counts: counts,
        rms: peaks.clone(),
        left_peaks: peaks.clone(),
        right_peaks: peaks.clone(),
        left_rms: peaks.clone(),
        right_rms: peaks.clone(),
        peaks,
        ..WaveformFrame::default()
    }
}

pub fn track(path: &str, decoder_hint: DecoderHint) -> Track {
    Track {
        duration: Some(10.0),
        decoder_hint,
        ..rivu_protocol::test_support::track(1, path)
    }
}

/// In-memory fixture access for application-owner integration tests.
/// This module is unavailable in normal builds.
impl Waveform {
    pub fn test_preview(data: WaveformFrame) -> Self {
        let source = Source::from_frame(&data).map(Rc::new);
        let mut waveform = Self::new(Arc::new(RwLock::new(data)));
        waveform.source = source;
        waveform
    }

    pub fn test_start_full(&mut self, hint: DecoderHint) -> (u64, Arc<AtomicBool>) {
        self.start_full(hint)
    }

    pub fn test_finish_full(&mut self, generation: u64, frame: WaveformFrame) -> bool {
        self.finish_full(generation, Ok(frame))
    }

    pub fn test_pending_full(&self) -> bool {
        self.manual.is_some()
    }
    pub fn test_full_peaks(&self) -> Option<&[Option<f32>]> {
        self.full.as_ref().map(|full| full.frame.peaks.as_slice())
    }
    pub fn test_dragging(&self) -> bool {
        self.dragging
    }

    pub fn test_capture_seek(&mut self, generation: u64) {
        self.seek_generation = generation;
        self.dragging = true;
        let mut interaction = self.interaction.borrow_mut();
        interaction
            .panels
            .insert(1, (Bounds::default(), Some(20.0)));
        interaction.active_panel = Some(1);
        interaction.seek_duration = Some(20.0);
    }

    pub fn test_panel_bounds(&mut self, id: u64, bounds: Bounds<Pixels>, duration: f64) {
        self.interaction
            .borrow_mut()
            .panels
            .insert(id, (bounds, Some(duration)));
    }

    pub fn test_remove_panel(&mut self, id: u64) -> Option<u64> {
        self.remove_interaction_panel(id)
    }

    pub fn test_retain_panels(&mut self, keep: impl FnMut(u64) -> bool) -> Option<u64> {
        self.retain_interaction_panels(keep)
    }

    pub fn test_release_seek(&mut self) {
        self.release_seek_capture();
    }
}
