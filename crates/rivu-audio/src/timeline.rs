//! Source-relative rules shared by live playback and finite rendering.

use std::{collections::VecDeque, ops::Range};

pub(crate) fn remaining_frames(total: Option<u64>, start: f64, rate: u32) -> Option<u64> {
    total.map(|frames| frames.saturating_sub((start * f64::from(rate)).round() as u64))
}

/// Retain only latency-trimmed frames inside the captured publication budget.
/// Input/EOF ownership and delivery remain with the caller.
pub(crate) fn published_range(
    frames: usize,
    discard: &mut usize,
    remaining: &mut Option<u64>,
) -> Range<usize> {
    let first = frames.min(*discard);
    *discard -= first;
    let count = remaining.map_or(frames - first, |left| {
        (frames - first).min(left.min(usize::MAX as u64) as usize)
    });
    if let Some(left) = remaining {
        *left -= count as u64;
    }
    first..first + count
}

/// Preguard is causal input, never a published media boundary.
pub(crate) fn record_mapping(
    mappings: &mut VecDeque<(u64, f64)>,
    anchor: (u64, f64),
    input_frame: u64,
    frames: usize,
    lead: u64,
    media_start: f64,
    rate: u32,
) {
    let discard = lead.saturating_sub(input_frame).min(frames as u64);
    if discard == frames as u64 {
        return;
    }
    let frame = (input_frame + discard).saturating_sub(lead);
    let media = media_start + (discard as f64 - lead as f64) / f64::from(rate);
    let anchor = mappings.back().copied().unwrap_or(anchor);
    let expected = anchor.1 + frame.saturating_sub(anchor.0) as f64 / f64::from(rate);
    if (media - expected).abs() > 0.5 / f64::from(rate) {
        mappings.push_back((frame, media));
    }
}

/// Return one contiguous media span without crossing the next source boundary.
pub(crate) fn mapped_span(
    mappings: &mut VecDeque<(u64, f64)>,
    anchor: &mut (u64, f64),
    frame: u64,
    frames: usize,
    rate: u32,
) -> (f64, usize) {
    while mappings.front().is_some_and(|mapping| mapping.0 <= frame) {
        *anchor = mappings.pop_front().expect("present media boundary");
    }
    let media = anchor.1 + frame.saturating_sub(anchor.0) as f64 / f64::from(rate);
    let count = mappings.front().map_or(frames, |mapping| {
        frames.min((mapping.0 - frame).min(usize::MAX as u64) as usize)
    });
    (media, count)
}
