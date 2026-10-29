use super::*;

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

    pub fn test_seek_seconds(&self, position: Point<Pixels>, panel: Option<u64>) -> Option<f64> {
        self.interaction.borrow().seek_seconds(position, panel)
    }

    pub fn test_seek_duration(&mut self, duration: f64) {
        self.interaction.borrow_mut().seek_duration = Some(duration);
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
