//! Continuous bounded native PCM capture. Audio admission never waits for its observer.
use crate::Channel;
use rivu_format::config::VisualizationSignal;
use rivu_protocol::pcm::{PCM_BLOCK_FRAMES, PcmHeader, PcmPosition, PcmState};
use rivu_shm::{MAX_PCM_CHANNELS, SharedPcmWindow};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

/// Core-owned PCM window and control plane. No display analysis lives here.
pub struct PcmControl {
    pub(crate) epoch: AtomicU64,
    pub(crate) desired_intent: AtomicU64,
    applied_intent: AtomicU64,
    generation: AtomicU64,
    sample_rate: AtomicU32,
    pub(crate) heard_frame: AtomicU64,
    layout: [AtomicU32; MAX_PCM_CHANNELS],
    channels: AtomicU32,
    playback_active: AtomicBool,
    visualization_enabled: AtomicBool,
    // Capture revision and enablement change atomically, including fast off/on.
    capture: AtomicU64,
    signal_after: AtomicBool,
    window: Arc<SharedPcmWindow>,
    wake: async_channel::Sender<()>,
    updates: async_channel::Receiver<()>,
}
impl Default for PcmControl {
    fn default() -> Self {
        let (wake, updates) = async_channel::bounded(1);
        Self {
            epoch: AtomicU64::new(0),
            desired_intent: AtomicU64::new(0),
            applied_intent: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            sample_rate: AtomicU32::new(48_000),
            heard_frame: AtomicU64::new(0),
            layout: std::array::from_fn(|index| AtomicU32::new(index as u32)),
            channels: AtomicU32::new(2),
            playback_active: AtomicBool::new(false),
            visualization_enabled: AtomicBool::new(true),
            capture: AtomicU64::new(0),
            signal_after: AtomicBool::new(true),
            window: Arc::new(SharedPcmWindow::new().expect("Cannot allocate shared PCM window")),
            wake,
            updates,
        }
    }
}
impl PcmControl {
    pub fn state(&self) -> PcmState {
        loop {
            let epoch = self.epoch.load(Ordering::Acquire);
            let capture = self.capture.load(Ordering::Acquire);
            let state = PcmState {
                epoch,
                intent: self.desired_intent.load(Ordering::Acquire),
                applied_intent: self.applied_intent.load(Ordering::Acquire),
                generation: self.generation.load(Ordering::Acquire),
                capture: capture >> 1,
                heard_frame: self.heard_frame.load(Ordering::Acquire),
                sample_rate: self.sample_rate.load(Ordering::Acquire),
                active: capture & 1 != 0
                    && self.playback_active.load(Ordering::Acquire)
                    && self.visualization_enabled.load(Ordering::Acquire),
            };
            if epoch == self.epoch.load(Ordering::Acquire)
                && capture == self.capture.load(Ordering::Acquire)
            {
                return state;
            }
        }
    }
    /// A notification hint, not a coherent capture identity. Each enabling
    /// transition also notifies, so a concurrent false cannot lose progress.
    pub(crate) fn wants_cursor_notification(&self) -> bool {
        self.capture.load(Ordering::Acquire) & 1 != 0
            && self.playback_active.load(Ordering::Acquire)
            && self.visualization_enabled.load(Ordering::Acquire)
    }
    pub fn set_enabled(&self, enabled: bool) {
        if self
            .capture
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                ((old & 1 != 0) != enabled).then(|| (old & !1).wrapping_add(2) | u64::from(enabled))
            })
            .is_ok()
        {
            self.notify();
        }
    }
    pub fn subscribe(&self) -> async_channel::Receiver<()> {
        self.updates.clone()
    }
    pub fn frontier(&self) -> PcmPosition {
        self.window.frontier()
    }
    pub fn window(&self) -> Arc<SharedPcmWindow> {
        Arc::clone(&self.window)
    }
    pub(crate) fn notify(&self) {
        let _ = self.wake.try_send(());
    }
    pub(crate) fn admit_intent(&self) {
        self.desired_intent.fetch_add(1, Ordering::AcqRel);
        self.notify();
    }
    pub(crate) fn cancel_intent(&self) {
        self.desired_intent.fetch_sub(1, Ordering::AcqRel);
        self.notify();
    }
    pub(crate) fn apply_audio_intents(&self, count: u64) {
        let applied = self.applied_intent.load(Ordering::Acquire);
        let pending = self
            .desired_intent
            .load(Ordering::Acquire)
            .wrapping_sub(applied);
        if count != 0 && pending != 0 {
            self.epoch.fetch_add(1, Ordering::AcqRel);
            self.applied_intent
                .store(applied.wrapping_add(count.min(pending)), Ordering::Release);
            self.epoch.fetch_add(1, Ordering::AcqRel);
            self.notify();
        }
    }
    pub(crate) fn audio_producer(self: &Arc<Self>) -> PcmProducer {
        PcmProducer {
            control: Arc::clone(self),
            layout: std::array::from_fn(|index| self.layout[index].load(Ordering::Acquire)),
            channels: self.channels.load(Ordering::Acquire) as usize,
            gap: None,
            identity: None,
            frame: 0,
        }
    }
    pub(crate) fn signal(&self) -> VisualizationSignal {
        if self.signal_after.load(Ordering::Acquire) {
            VisualizationSignal::After
        } else {
            VisualizationSignal::Before
        }
    }
    pub(crate) fn set_signal(&self, signal: VisualizationSignal) {
        let after = signal == VisualizationSignal::After;
        if self.signal_after.load(Ordering::Acquire) != after {
            self.epoch.fetch_add(1, Ordering::AcqRel);
            self.signal_after.store(after, Ordering::Release);
            self.epoch.fetch_add(1, Ordering::AcqRel);
            self.notify();
        }
    }
    pub(crate) fn set_generation(&self, generation: u64) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.generation.store(generation, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.notify();
    }
    pub(crate) fn set_playback_active(&self, active: bool) {
        if self.playback_active.load(Ordering::Acquire) != active {
            self.epoch.fetch_add(1, Ordering::AcqRel);
            self.playback_active.store(active, Ordering::Release);
            self.epoch.fetch_add(1, Ordering::AcqRel);
            self.notify();
        }
    }
    pub(crate) fn stop(&self) {
        self.set_playback_active(false);
        self.epoch.fetch_add(2, Ordering::AcqRel);
        self.notify();
    }
    pub(crate) fn set_visualization_enabled(&self, enabled: bool) {
        if self.visualization_enabled.load(Ordering::Acquire) != enabled {
            self.epoch.fetch_add(1, Ordering::AcqRel);
            self.visualization_enabled.store(enabled, Ordering::Release);
            self.epoch.fetch_add(1, Ordering::AcqRel);
            self.notify();
        }
    }
    pub(crate) fn reset(&self, sample_rate: u32, layout: &[Channel]) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.sample_rate.store(sample_rate, Ordering::Release);
        self.heard_frame.store(0, Ordering::Release);
        for (slot, channel) in self.layout.iter().zip(layout) {
            slot.store(channel.standard_index(), Ordering::Release);
        }
        self.channels.store(layout.len() as u32, Ordering::Release);
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.notify();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct PcmAudioIdentity {
    epoch: u64,
    intent: u64,
    generation: u64,
    capture: u64,
    sample_rate: u32,
}
#[cfg(test)]
impl PcmAudioIdentity {
    pub(crate) fn sample_rate(self) -> u32 {
        self.sample_rate
    }
}
pub(crate) struct PcmProducer {
    control: Arc<PcmControl>,
    layout: [u32; MAX_PCM_CHANNELS],
    channels: usize,
    gap: Option<PcmAudioIdentity>,
    identity: Option<PcmAudioIdentity>,
    frame: u64,
}
impl PcmProducer {
    pub(crate) fn identity(&self) -> Option<PcmAudioIdentity> {
        let s = self.control.state();
        (s.active && s.epoch.is_multiple_of(2) && s.intent == s.applied_intent).then_some(
            PcmAudioIdentity {
                epoch: s.epoch,
                intent: s.intent,
                generation: s.generation,
                capture: s.capture,
                sample_rate: s.sample_rate,
            },
        )
    }
    pub(crate) fn discontinuity(&mut self) {
        self.gap = self.identity();
    }

    /// A newly accepted source traversal starts at zero, unlike a gap within it.
    /// Keep global block sequence ordering and mark only its first admitted block.
    pub(crate) fn restart_traversal(&mut self) {
        self.identity = None;
        self.discontinuity();
    }
    #[cfg(test)]
    pub(crate) fn push(&mut self, output_frame: u64, pcm: &[f32]) {
        let Some(identity) = self.identity() else {
            return;
        };
        self.push_samples_for(
            identity,
            output_frame,
            output_frame as f64 / f64::from(identity.sample_rate),
            f64::from(identity.sample_rate).recip(),
            pcm.iter().copied(),
        );
    }
    pub(crate) fn push_mapped(
        &mut self,
        output_frame: u64,
        media_start: f64,
        media_step: f64,
        pcm: &[f32],
    ) {
        let Some(identity) = self.identity() else {
            return;
        };
        self.push_samples_for(
            identity,
            output_frame,
            media_start,
            media_step,
            pcm.iter().copied(),
        );
    }
    pub(crate) fn push_samples_for(
        &mut self,
        identity: PcmAudioIdentity,
        mut output_frame: u64,
        mut media_start: f64,
        media_step: f64,
        mut samples: impl ExactSizeIterator<Item = f32>,
    ) {
        if self.identity() != Some(identity) {
            return;
        }
        if self.identity != Some(identity) {
            self.identity = Some(identity);
            self.frame = 0;
            self.channels = self.control.channels.load(Ordering::Acquire) as usize;
            for (slot, channel) in self.layout.iter_mut().zip(&self.control.layout) {
                *slot = channel.load(Ordering::Acquire);
            }
        }
        debug_assert!(self.channels != 0 && self.channels <= MAX_PCM_CHANNELS);
        debug_assert!(samples.len().is_multiple_of(self.channels));
        while samples.len() != 0 {
            if self.identity() != Some(identity) {
                return;
            }
            let frames = (samples.len() / self.channels).min(PCM_BLOCK_FRAMES);
            let header = PcmHeader {
                sequence: 0, // SharedPcmWindow owns publication ordering.
                epoch: identity.epoch,
                intent: identity.intent,
                generation: identity.generation,
                capture: identity.capture,
                start_frame: self.frame,
                output_frame,
                media_start,
                media_step,
                sample_rate: identity.sample_rate,
                gap: self.gap == Some(identity),
            };
            let mut block = samples.by_ref().take(frames * self.channels);
            let published =
                self.control
                    .window
                    .publish(header, &self.layout[..self.channels], &mut block);
            // Rejected publications must still advance this admitted output span.
            for _ in block {}
            self.frame = self.frame.wrapping_add(frames as u64);
            output_frame = output_frame.wrapping_add(frames as u64);
            media_start += frames as f64 * media_step;
            self.gap = (!published).then_some(identity);
        }
        self.control.notify();
    }
}

#[cfg(test)]
pub(crate) struct CapturedSample {
    pub samples: [f32; 2],
    pub gap: bool,
    pub header: PcmHeader,
}
/// Test-only observer folds native PCM on the reader side, like the GUI.
#[cfg(test)]
pub(crate) struct TestCapture {
    control: Arc<PcmControl>,
    next: u64,
    offset: usize,
    layout: [u32; MAX_PCM_CHANNELS],
    channels: usize,
    weights: Vec<[f32; 2]>,
}
#[cfg(test)]
impl TestCapture {
    pub(crate) fn new(control: &Arc<PcmControl>) -> Self {
        let position = control.frontier();
        Self {
            control: Arc::clone(control),
            next: position.sequence.max(1),
            offset: usize::from(position.offset),
            layout: [0; MAX_PCM_CHANNELS],
            channels: 0,
            weights: Vec::new(),
        }
    }
    pub(crate) fn read(&mut self, output: &mut [[f32; 2]]) -> Option<(PcmHeader, usize)> {
        if output.is_empty() {
            return None;
        }
        let latest = self.control.frontier().sequence;
        let oldest = latest
            .saturating_sub(self.control.window.retained_blocks() as u64 - 1)
            .max(1);
        if self.next < oldest {
            self.next = oldest;
            self.offset = 0;
        }
        while self.next <= latest {
            let Some(block) = self.control.window.block(self.next) else {
                if self.next == latest {
                    return None;
                }
                self.next += 1;
                self.offset = 0;
                continue;
            };
            let mut header = block.header();
            if !self.control.state().accepts(header) {
                self.next += 1;
                self.offset = 0;
                continue;
            }
            if self.offset >= block.len() {
                if self.next < latest {
                    self.next += 1;
                    self.offset = 0;
                    continue;
                }
                return None;
            }
            if block.layout() != &self.layout[..self.channels] {
                self.channels = block.layout().len();
                self.layout[..self.channels].copy_from_slice(block.layout());
                let mut speakers = [Channel::FrontLeft; MAX_PCM_CHANNELS];
                for (channel, index) in speakers.iter_mut().zip(block.layout()) {
                    *channel = Channel::from_standard_index(*index).expect("valid native speaker");
                }
                self.weights = crate::channels::analysis_weights(&speakers[..self.channels]);
            }
            let count = (block.len() - self.offset).min(output.len());
            for (frame, stereo) in output[..count].iter_mut().enumerate() {
                *stereo = [0.0; 2];
                for (channel, weight) in self.weights.iter().enumerate() {
                    let sample = block.sample(self.offset + frame, channel);
                    stereo[0] += sample * weight[0];
                    stereo[1] += sample * weight[1];
                }
            }
            header.start_frame += self.offset as u64;
            header.output_frame += self.offset as u64;
            header.media_start += self.offset as f64 * header.media_step;
            header.gap &= self.offset == 0;
            self.offset += count;
            if self.offset == block.len() && (block.len() == PCM_BLOCK_FRAMES || self.next < latest)
            {
                self.next += 1;
                self.offset = 0;
            }
            return Some((header, count));
        }
        None
    }
    pub(crate) fn try_pop(&mut self) -> Option<CapturedSample> {
        let mut samples = [[0.0; 2]];
        let (header, _) = self.read(&mut samples)?;
        Some(CapturedSample {
            samples: samples[0],
            gap: header.gap,
            header,
        })
    }
    pub(crate) fn pop_iter(&mut self) -> impl Iterator<Item = CapturedSample> + '_ {
        std::iter::from_fn(|| self.try_pop())
    }
}
#[cfg(test)]
impl PcmProducer {
    pub(crate) fn capture_for_test(
        control: &Arc<PcmControl>,
        layout: &[Channel],
    ) -> (Self, TestCapture) {
        control.reset(control.state().sample_rate, layout);
        (control.audio_producer(), TestCapture::new(control))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Channel::{FrontLeft, FrontRight};

    fn capture() -> (Arc<PcmControl>, PcmProducer, TestCapture) {
        let control = Arc::new(PcmControl::default());
        control.set_generation(17);
        control.reset(96_000, &[FrontLeft, FrontRight]);
        control.set_playback_active(true);
        control.set_enabled(true);
        let producer = control.audio_producer();
        let consumer = TestCapture::new(&control);
        (control, producer, consumer)
    }

    #[test]
    fn cursor_notifications_resume_after_each_enabling_transition() {
        let (control, _, _) = capture();
        let updates = control.subscribe();
        for gate in 0..3 {
            match gate {
                0 => control.set_enabled(false),
                1 => control.set_playback_active(false),
                _ => control.set_visualization_enabled(false),
            }
            assert!(!control.wants_cursor_notification());
            while updates.try_recv().is_ok() {}
            control.heard_frame.store(4096 + gate, Ordering::Release);
            match gate {
                0 => control.set_enabled(true),
                1 => control.set_playback_active(true),
                _ => control.set_visualization_enabled(true),
            }
            updates.try_recv().unwrap();
            assert!(control.wants_cursor_notification());
            assert_eq!(control.state().heard_frame, 4096 + gate);
        }
    }

    #[test]
    fn published_blocks_preserve_samples_and_capture_output_coordinates() {
        let (control, mut producer, _) = capture();
        let samples: Vec<_> = (0..2500).flat_map(|i| [i as f32, -(i as f32)]).collect();
        producer.push(70, &samples);
        let window = control.window();
        let mut cursor = 0;
        for sequence in 1..=window.sequence() {
            let block = window.block(sequence).unwrap();
            let header = block.header();
            assert!(control.state().accepts(header));
            assert_eq!(header.generation, 17);
            assert_eq!(header.sample_rate, 96_000);
            assert_eq!(header.start_frame, cursor);
            assert_eq!(header.output_frame, cursor + 70);
            assert!(!header.gap);
            for frame in 0..block.len() {
                assert_eq!(block.sample(frame, 0), cursor as f32);
                assert_eq!(block.sample(frame, 1), -(cursor as f32));
                cursor += 1;
            }
        }
        assert_eq!(cursor, 2500);
    }

    #[test]
    fn new_observer_skips_published_prefix_and_reads_each_appended_tail_once() {
        let (control, mut producer, _) = capture();
        producer.push(100, &[0.9; 6]);
        let mut consumer = TestCapture::new(&control);
        assert!(consumer.try_pop().is_none());
        for (frame, value) in [(3, 0.25), (4, 0.5), (5, 0.75)] {
            producer.push(100 + frame, &[value; 2]);
            assert_eq!(
                control.frontier(),
                PcmPosition {
                    sequence: 1,
                    offset: frame as u16 + 1
                }
            );
            let sample = consumer.try_pop().unwrap();
            assert_eq!(sample.samples, [value; 2]);
            assert_eq!(sample.header.start_frame, frame);
            assert_eq!(sample.header.output_frame, 100 + frame);
            assert!(!sample.gap);
            assert!(consumer.try_pop().is_none());
        }
    }

    #[test]
    fn native_window_preserves_all_speakers_and_unfolded_samples() {
        let (control, _, _) = capture();
        let layout: Vec<_> = (0..18)
            .map(|index| Channel::from_standard_index(index).unwrap())
            .collect();
        control.reset(96_000, &layout);
        let mut producer = control.audio_producer();
        let samples: Vec<_> = (0..18 * 3).map(|sample| sample as f32 / 64.0).collect();
        producer.push(123, &samples);
        let window = control.window();
        let block = window.block(window.sequence()).unwrap();
        assert_eq!(block.layout(), &(0..18).collect::<Vec<_>>());
        assert_eq!(block.len(), 3);
        assert_eq!(block.header().output_frame, 123);
        for frame in 0..3 {
            for channel in 0..18 {
                assert_eq!(block.sample(frame, channel), samples[frame * 18 + channel]);
            }
        }
    }

    #[test]
    fn overwritten_capture_retains_latest_pcm_without_a_source_gap() {
        let (control, mut producer, _) = capture();
        let blocks = rivu_shm::PCM_QUEUE_BLOCKS;
        let frames = (blocks + 2) * PCM_BLOCK_FRAMES;
        producer.push(0, &vec![0.25; frames * 2]);
        let window = control.window();
        assert!(window.block(1).is_none());
        assert!(window.block(2).is_none());
        let mut cursor = (2 * PCM_BLOCK_FRAMES) as u64;
        for sequence in 3..=window.sequence() {
            let block = window.block(sequence).unwrap();
            assert!(!block.header().gap);
            assert_eq!(block.header().start_frame, cursor);
            assert_eq!(block.header().output_frame, cursor);
            for frame in 0..block.len() {
                assert_eq!(block.sample(frame, 0), 0.25);
                assert_eq!(block.sample(frame, 1), 0.25);
            }
            cursor += block.len() as u64;
        }
        assert_eq!(cursor, frames as u64);
        producer.push(cursor, &[0.5; 24]);
        let block = window.block(window.sequence()).unwrap();
        assert_eq!(block.len(), 12);
        assert!(!block.header().gap);
        assert_eq!(block.header().start_frame, cursor);
        assert_eq!(block.header().output_frame, cursor);
    }

    #[test]
    fn queued_intent_revokes_partial_blocks_and_retained_before_cannot_be_relabelled() {
        let (control, mut producer, mut consumer) = capture();
        let old = producer.identity().unwrap();
        producer.push(0, &[0.25; 32]);
        let mut output = [[0.0; 2]; 4];
        assert_eq!(consumer.read(&mut output).unwrap().1, 4);
        control.admit_intent();
        assert!(producer.identity().is_none());
        assert!(consumer.read(&mut output).is_none());
        control.apply_audio_intents(1);
        producer.push_samples_for(old, 16, 0.0, 1.0 / 96_000.0, [1.0; 16].into_iter());
        assert!(consumer.read(&mut output).is_none());
        let new = producer.identity().unwrap();
        assert!(new != old);
        producer.push_samples_for(new, 24, 0.0, 1.0 / 96_000.0, [0.5; 8].into_iter());
        let (header, _) = consumer.read(&mut output).unwrap();
        assert_eq!(header.intent, 1);
        assert_eq!(header.start_frame, 0);
        assert_eq!(header.output_frame, 24);
        assert_eq!(output, [[0.5; 2]; 4]);
    }

    #[test]
    fn every_capture_boundary_rejects_old_pcm_and_notifies_without_audio() {
        let (control, mut producer, mut consumer) = capture();
        let updates = control.subscribe();
        while updates.try_recv().is_ok() {}
        let mut output = [[0.0; 2]; PCM_BLOCK_FRAMES];
        for boundary in 0..6 {
            let old = producer.identity().unwrap();
            producer.push(0, &[1.0; 32]);
            updates.try_recv().unwrap();
            match boundary {
                0 => {
                    control.set_enabled(false);
                    control.set_enabled(true);
                }
                1 => {
                    control.set_playback_active(false);
                    control.set_playback_active(true);
                }
                2 => {
                    control.set_visualization_enabled(false);
                    control.set_visualization_enabled(true);
                }
                3 => control.set_signal(VisualizationSignal::Before),
                4 => control.reset(48_000, &[FrontLeft, FrontRight]),
                _ => control.set_generation(18),
            }
            updates.try_recv().unwrap();
            assert!(producer.identity().unwrap() != old);
            assert!(consumer.read(&mut output).is_none());
            producer.push_samples_for(old, 16, 0.0, 1.0 / 96_000.0, [1.0; 32].into_iter());
            assert!(consumer.read(&mut output).is_none());
        }
    }
    #[test]
    fn media_mapping_comes_from_admitted_pcm_and_offsets_follow_it() {
        let (control, mut producer, mut consumer) = capture();
        producer.push_mapped(123, 0.25, 1.0 / 96_000.0, &[0.25; 32]);
        let first = consumer.try_pop().unwrap();
        assert_eq!(first.header.media_start, 0.25);
        let second = consumer.try_pop().unwrap();
        assert_eq!(second.header.media_start, 0.25 + 1.0 / 96_000.0);
        producer.push_mapped(139, 1.5, 1.0 / 96_000.0, &[0.5; 16]);
        let window = control.window();
        assert_eq!(
            window
                .block(window.sequence())
                .unwrap()
                .header()
                .media_start,
            1.5
        );
        assert_eq!(
            window
                .block(window.sequence())
                .unwrap()
                .header()
                .output_frame,
            139
        );
    }
}
