//! Pitch-preserving playback speed between processed PCM and the output ring.
//!
//! Speed is constant within one output segment, so source time is output time
//! scaled once; changing speed starts a new segment, like a seek. At 1.0x PCM
//! goes straight to the output and no stretcher exists.

use super::output::Output;
use anyhow::{Context, Result};
use std::{ffi::c_void, ops::Range, ptr::NonNull};

/// A playback speed within the supported range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackSpeed(f64);

impl PlaybackSpeed {
    pub const MIN: f64 = rivu_format::playback_speed::MIN;
    pub const MAX: f64 = rivu_format::playback_speed::MAX;
    pub const NORMAL: Self = Self(1.0);

    pub fn new(speed: f64) -> Option<Self> {
        (Self::MIN..=Self::MAX)
            .contains(&speed)
            .then_some(Self(speed))
    }

    pub fn get(self) -> f64 {
        self.0
    }

    /// Output frames that play `source` frames.
    pub(crate) fn output_frames(self, source: u64) -> u64 {
        (source as f64 / self.0) as u64
    }
}

impl Default for PlaybackSpeed {
    fn default() -> Self {
        Self::NORMAL
    }
}

unsafe extern "C" {
    fn rivu_stretch_new(channels: i32, sample_rate: f32) -> *mut c_void;
    fn rivu_stretch_free(stretch: *mut c_void);
    fn rivu_stretch_reset(stretch: *mut c_void);
    fn rivu_stretch_input_latency(stretch: *const c_void) -> i32;
    fn rivu_stretch_output_latency(stretch: *const c_void) -> i32;
    fn rivu_stretch_process(
        stretch: *mut c_void,
        input: *const f32,
        input_frames: i32,
        output: *mut f32,
        output_frames: i32,
        channels: i32,
    );
    fn rivu_stretch_flush(
        stretch: *mut c_void,
        output: *mut f32,
        output_frames: i32,
        channels: i32,
        playback_rate: f32,
    );
}

/// Owned Signalsmith instance; all buffers are complete interleaved frames.
struct Stretch {
    raw: NonNull<c_void>,
    channels: usize,
    input_latency: usize,
    output_latency: usize,
    // End-of-input padding that carries processing time past the last frame.
    silence: Vec<f32>,
}

// Used by one owner at a time; the instance has no thread affinity.
unsafe impl Send for Stretch {}

impl Stretch {
    fn new(channels: usize, sample_rate: u32) -> Result<Self> {
        let raw = NonNull::new(unsafe { rivu_stretch_new(channels as i32, sample_rate as f32) })
            .context("Cannot allocate playback speed processor")?;
        let (input_latency, output_latency) = unsafe {
            (
                rivu_stretch_input_latency(raw.as_ptr()) as usize,
                rivu_stretch_output_latency(raw.as_ptr()) as usize,
            )
        };
        Ok(Self {
            raw,
            channels,
            input_latency,
            output_latency,
            silence: vec![0.0; input_latency * channels],
        })
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        unsafe {
            rivu_stretch_process(
                self.raw.as_ptr(),
                input.as_ptr(),
                (input.len() / self.channels) as i32,
                output.as_mut_ptr(),
                (output.len() / self.channels) as i32,
                self.channels as i32,
            );
        }
    }

    fn pad(&mut self, output: &mut [f32]) {
        let silence = std::mem::take(&mut self.silence);
        self.process(&silence, output);
        self.silence = silence;
    }

    /// Zero input keeps advancing at `speed`; at rate 0 Signalsmith would
    /// instead freeze the last spectrum for tails longer than one interval.
    fn flush(&mut self, output: &mut [f32], speed: PlaybackSpeed) {
        unsafe {
            rivu_stretch_flush(
                self.raw.as_ptr(),
                output.as_mut_ptr(),
                (output.len() / self.channels) as i32,
                self.channels as i32,
                speed.get() as f32,
            );
        }
    }
}

impl Drop for Stretch {
    fn drop(&mut self) {
        unsafe { rivu_stretch_free(self.raw.as_ptr()) }
    }
}

// Source frames stretched per call; bounds the staged output.
const MAX_INPUT_FRAMES: usize = 4096;

pub(super) enum SpeedEvent<'a> {
    Consumed(usize),
    Stretched(u64, &'a [f32]),
}

pub(super) struct SpeedStage {
    speed: PlaybackSpeed,
    next: PlaybackSpeed,
    channels: usize,
    sample_rate: u32,
    stretch: Option<Stretch>,
    // Initialized once. Only this range holds samples awaiting admission.
    staged: Vec<f32>,
    pending: Range<usize>,
    // Leading latency frames to drop so output starts at source frame zero.
    skip: usize,
    consumed: u64,
    useful: u64,
    finished: bool,
}

impl SpeedStage {
    pub(super) fn new(speed: PlaybackSpeed, channels: usize, sample_rate: u32) -> Result<Self> {
        let mut stage = Self {
            speed: PlaybackSpeed::NORMAL,
            next: speed,
            channels,
            sample_rate,
            stretch: None,
            staged: Vec::new(),
            pending: 0..0,
            skip: 0,
            consumed: 0,
            useful: 0,
            finished: false,
        };
        stage.restart()?;
        Ok(stage)
    }

    pub(super) fn speed(&self) -> PlaybackSpeed {
        self.speed
    }

    /// Speed applied by the next segment.
    pub(super) fn set(&mut self, speed: PlaybackSpeed) {
        self.next = speed;
    }

    fn active(&self) -> bool {
        self.speed != PlaybackSpeed::NORMAL
    }

    /// Begin a new segment at the requested speed. A stretcher created by an
    /// earlier segment is reset and reused. On failure the current segment,
    /// including its speed, is left unchanged.
    pub(super) fn restart(&mut self) -> Result<()> {
        if self.next != PlaybackSpeed::NORMAL && self.stretch.is_none() {
            // Without a stretcher no segment has staged anything.
            let stretch = Stretch::new(self.channels, self.sample_rate)?;
            let slowest = |frames: usize| (frames as f64 / PlaybackSpeed::MIN) as usize + 1;
            let frames = slowest(MAX_INPUT_FRAMES)
                .max(slowest(stretch.input_latency) + stretch.output_latency);
            let samples = frames * self.channels;
            self.staged
                .try_reserve_exact(samples)
                .context("Cannot allocate playback speed buffer")?;
            self.staged.resize(samples, 0.0);
            self.stretch = Some(stretch);
        }
        self.speed = self.next;
        self.pending = 0..0;
        self.consumed = 0;
        self.useful = 0;
        self.finished = false;
        self.skip = 0;
        if let Some(stretch) = self.stretch.as_ref().filter(|_| self.active()) {
            unsafe { rivu_stretch_reset(stretch.raw.as_ptr()) };
            self.skip = stretch.output_latency + self.span(0, stretch.input_latency as u64);
        }
        Ok(())
    }

    /// Source seconds played by `output_seconds` of this segment.
    pub(super) fn source_seconds(&self, output_seconds: f64) -> f64 {
        output_seconds * self.speed.get()
    }

    /// Whether this segment stretches. Its admitted audio is then captured as
    /// heard through the `tap` of `push` and `finish`, not as source PCM.
    pub(super) fn stretching(&self) -> bool {
        self.active()
    }

    /// Return source frames consumed; debit `output_left` by output frames admitted.
    /// Report consumed input before emitting stretched output that can use it.
    pub(super) fn push(
        &mut self,
        output: &mut Output,
        input: &[f32],
        output_left: &mut usize,
        observe: &mut impl FnMut(SpeedEvent<'_>),
    ) -> usize {
        if !self.active() {
            let frames = (input.len() / self.channels).min(*output_left);
            let admitted = output.push(&input[..frames * self.channels]);
            *output_left -= admitted;
            observe(SpeedEvent::Consumed(admitted));
            return admitted;
        }
        if !self.drain(output, output_left, observe) {
            return 0;
        }
        // Round up: a single vacant output frame must still admit a source frame,
        // or a refill reporting vacancy would make no progress below 1.0x. The
        // overshoot stays staged within the reserved bound.
        let room = output.vacant_frames().min(*output_left);
        let frames = (input.len() / self.channels)
            .min((room as f64 * self.speed.get()).ceil() as usize)
            .min(MAX_INPUT_FRAMES);
        if frames != 0 {
            let count = self.span(self.consumed, frames as u64);
            self.consumed += frames as u64;
            observe(SpeedEvent::Consumed(frames));
            let input = &input[..frames * self.channels];
            self.stage(count, |stretch, out| stretch.process(input, out));
            self.drain(output, output_left, observe);
        }
        frames
    }

    /// Render the end-of-input tail once; true when all of it is admitted.
    pub(super) fn finish(
        &mut self,
        output: &mut Output,
        output_left: &mut usize,
        observe: &mut impl FnMut(SpeedEvent<'_>),
    ) -> bool {
        if !self.active() {
            return true;
        }
        if !self.finished {
            if !self.drain(output, output_left, observe) {
                return false;
            }
            self.finished = true;
            let stretch = self.stretch.as_ref().expect("stretcher");
            let (pad, flush) = (stretch.input_latency as u64, stretch.output_latency);
            self.stage(self.span(self.consumed, pad), Stretch::pad);
            let speed = self.speed;
            self.stage(flush, |stretch, out| stretch.flush(out, speed));
            // Emit exactly the frames that play this segment's source.
            let owed = self.speed.output_frames(self.consumed) as i64 - self.useful as i64;
            let end = self.pending.end as i64 + owed * self.channels as i64;
            let end = end.max(self.pending.start as i64) as usize;
            if end > self.pending.end {
                self.staged[self.pending.end..end].fill(0.0);
            }
            self.pending.end = end;
        }
        self.drain(output, output_left, observe)
    }

    /// No stretched audio remains to be admitted.
    pub(super) fn idle(&self) -> bool {
        !self.active() || (self.finished && self.pending.is_empty())
    }

    /// Output frames that play `frames` source frames after `start`.
    fn span(&self, start: u64, frames: u64) -> usize {
        (self.speed.output_frames(start + frames) - self.speed.output_frames(start)) as usize
    }

    fn stage(&mut self, frames: usize, render: impl FnOnce(&mut Stretch, &mut [f32])) {
        let start = self.pending.end;
        self.pending.end += frames * self.channels;
        render(
            self.stretch.as_mut().expect("stretcher"),
            &mut self.staged[start..self.pending.end],
        );
        // Latency frames precede everything kept, so they start at the cursor.
        let skipped = self.skip.min(frames);
        self.skip -= skipped;
        self.pending.start += skipped * self.channels;
        self.useful += (frames - skipped) as u64;
    }

    fn drain(
        &mut self,
        output: &mut Output,
        output_left: &mut usize,
        observe: &mut impl FnMut(SpeedEvent<'_>),
    ) -> bool {
        if !self.pending.is_empty() {
            let frame = output.submitted();
            let pending = &self.staged[self.pending.clone()];
            let frames = (pending.len() / self.channels).min(*output_left);
            let admitted_frames = output.push(&pending[..frames * self.channels]);
            *output_left -= admitted_frames;
            let admitted = admitted_frames * self.channels;
            if admitted != 0 {
                observe(SpeedEvent::Stretched(frame, &pending[..admitted]));
            }
            self.pending.start += admitted;
        }
        let drained = self.pending.is_empty();
        if drained {
            self.pending = 0..0;
        }
        drained
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::Channel;

    const RATE: u32 = 48_000;

    /// Stream `input` through `stage` into a roomy output; return what it played
    /// and check that the tap saw exactly that audio at its output frames.
    fn play(stage: &mut SpeedStage, input: &[f32]) -> Vec<f32> {
        let layout = [Channel::FrontLeft, Channel::FrontRight];
        // Room for the whole stretched segment, including the final drained frames.
        let frames = input.len() / layout.len();
        let capacity = ((frames as f64 / stage.speed().get()).ceil() as usize + 1) * layout.len();
        let (mut output, mut device) =
            crate::output::test_backend_with_capacity(RATE, false, &layout, capacity);
        let mut tapped = Vec::new();
        let mut tap = |event: SpeedEvent<'_>| {
            if let SpeedEvent::Stretched(frame, pcm) = event {
                assert_eq!(frame as usize * 2, tapped.len());
                tapped.extend_from_slice(pcm);
            }
        };
        for chunk in input.chunks(1000 * 2) {
            let mut rest = chunk;
            while !rest.is_empty() {
                let mut budget = 511;
                let before = output.submitted();
                let frames = stage.push(&mut output, rest, &mut budget, &mut tap);
                assert_eq!(output.submitted() - before, (511 - budget) as u64);
                assert!(frames != 0 || budget == 0);
                rest = &rest[frames * 2..];
            }
        }
        loop {
            let before = output.submitted();
            let mut budget = 511;
            let done = stage.finish(&mut output, &mut budget, &mut tap);
            assert_eq!(output.submitted() - before, (511 - budget) as u64);
            if done {
                break;
            }
            assert_eq!(budget, 0);
        }
        assert!(stage.idle());
        let mut played = vec![0.0; output.submitted() as usize * 2];
        device(&mut played, 0);
        if stage.stretching() {
            assert_eq!(tapped, played);
        } else {
            assert!(tapped.is_empty());
        }
        played
    }

    fn render(speed: f64, input: &[f32]) -> Vec<f32> {
        let mut stage = SpeedStage::new(PlaybackSpeed::new(speed).unwrap(), 2, RATE).unwrap();
        play(&mut stage, input)
    }

    fn tone(hz: f64, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let s = (std::f64::consts::TAU * hz * i as f64 / f64::from(RATE)).sin() as f32;
                [s * 0.5, s * 0.5]
            })
            .collect()
    }

    /// Dominant frequency from rising zero crossings over the settled middle.
    fn frequency(stereo: &[f32]) -> f64 {
        let frames = stereo.len() / 2;
        let middle = &stereo[frames / 4 * 2..frames * 3 / 4 * 2];
        let left = || middle.iter().step_by(2);
        let rising = left()
            .zip(left().skip(1))
            .filter(|(a, b)| **a < 0.0 && **b >= 0.0)
            .count();
        rising as f64 * f64::from(RATE) / (middle.len() / 2) as f64
    }

    #[test]
    fn normal_speed_is_bit_exact_passthrough() {
        let input = tone(440.0, RATE as usize / 2);
        assert_eq!(render(1.0, &input), input);
    }

    #[test]
    fn speed_scales_duration_exactly_and_preserves_pitch() {
        let frames = RATE as usize * 2;
        let input = tone(440.0, frames);
        for speed in [0.5, 0.75, 1.5, 2.0] {
            let output = render(speed, &input);
            assert_eq!(
                output.len() / 2,
                (frames as f64 / speed) as usize,
                "{speed}x"
            );
            let hz = frequency(&output);
            assert!((hz - 440.0).abs() < 5.0, "{speed}x played {hz} Hz");
        }
    }

    #[test]
    fn restarted_stage_matches_a_fresh_one() {
        let input = tone(440.0, RATE as usize);
        let mut stage = SpeedStage::new(PlaybackSpeed::new(1.5).unwrap(), 2, RATE).unwrap();
        // An abandoned, unfinished segment must not leak into the next one.
        let layout = [Channel::FrontLeft, Channel::FrontRight];
        let (mut output, _device) =
            crate::output::test_backend_with_capacity(RATE, false, &layout, 2048);
        let warmup = tone(1000.0, 4000);
        let mut budget = output.vacant_frames();
        while stage.push(&mut output, &warmup, &mut budget, &mut |_| {}) != 0 {}
        assert!(!stage.idle());
        stage.set(PlaybackSpeed::new(1.25).unwrap());
        stage.restart().unwrap();
        assert_eq!(play(&mut stage, &input), render(1.25, &input));
    }

    #[test]
    fn slow_speed_admits_input_into_a_single_vacant_frame() {
        let layout = [Channel::FrontLeft, Channel::FrontRight];
        let (mut output, _device) =
            crate::output::test_backend_with_capacity(RATE, false, &layout, 64);
        let fill = vec![0.0; (output.vacant_frames() - 1) * 2];
        output.push(&fill);
        assert_eq!(output.vacant_frames(), 1);
        let mut stage = SpeedStage::new(PlaybackSpeed::new(0.5).unwrap(), 2, RATE).unwrap();
        // A refill that reports vacancy must make progress.
        let mut budget = 1;
        assert!(stage.push(&mut output, &tone(440.0, 16), &mut budget, &mut |_| {}) > 0);
    }
}
