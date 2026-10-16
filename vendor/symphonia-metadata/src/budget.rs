// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-input metadata admission, shared by standalone and embedded readers.
//!
//! Reservations are never refunded: repeated revisions, nested frames, and many
//! tiny values cannot evade the input's aggregate byte or object limits. Limits
//! apply before payload allocation. Text reserves four bytes per encoded byte
//! for decoding/owned copies, plus 256 bytes per item for collection overhead.
//! Visual payloads have separate 8 MiB per-image / 16 MiB aggregate limits.
//! Even `Limit::None` is clamped to these finite safety ceilings.

use std::cell::Cell;
use symphonia_core::{common::Limit, meta::MetadataOptions};

pub const MAX_VISUAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const TOTAL_TEXT_BYTES: usize = 4 * 1024 * 1024;
const TOTAL_VISUAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_ITEMS: usize = 4096;
const MAX_DEPTH: usize = 8;
pub(crate) const VISUAL_HEADER_BYTES: usize = 64 * 1024;

thread_local! {
    static ACTIVE: Cell<Option<MetadataBudget>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug)]
pub struct MetadataBudget {
    tag_limit: usize,
    visual_limit: usize,
    text_left: usize,
    visual_left: usize,
    encoded_left: usize,
    items_left: usize,
    depth: usize,
}

impl Default for MetadataBudget {
    fn default() -> Self { Self::new(MetadataOptions::default()) }
}

fn finite(limit: Limit, ceiling: usize) -> usize {
    match limit {
        Limit::Maximum(value) => value.min(ceiling),
        _ => ceiling,
    }
}

impl MetadataBudget {
    pub fn new(options: MetadataOptions) -> Self {
        Self {
            tag_limit: finite(options.limit_tag_bytes, MAX_TEXT_BYTES),
            visual_limit: finite(options.limit_visual_bytes, MAX_VISUAL_BYTES),
            text_left: TOTAL_TEXT_BYTES,
            visual_left: TOTAL_VISUAL_BYTES,
            encoded_left: 24 * 1024 * 1024,
            items_left: MAX_ITEMS,
            depth: 0,
        }
    }

    /// Run parser work against this input's remaining budget. The budget is
    /// restored on unwind and can be resumed when demuxing later packets.
    pub fn with<T>(&mut self, read: impl FnOnce() -> T) -> T {
        let saved = ACTIVE.with(|active| active.replace(Some(*self)));
        struct Restore<'a> { target: &'a mut MetadataBudget, saved: Option<MetadataBudget> }
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                *self.target = ACTIVE.with(|active| active.replace(self.saved)).expect("active budget");
            }
        }
        let _restore = Restore { target: self, saved };
        read()
    }
}

pub struct Scope(bool, std::marker::PhantomData<std::rc::Rc<()>>);
impl Scope {
    pub fn new(options: MetadataOptions) -> Self {
        Self(ACTIVE.with(|active| {
            if active.get().is_some() { false }
            else { active.set(Some(MetadataBudget::new(options))); true }
        }), std::marker::PhantomData)
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        if self.0 { ACTIVE.with(|active| active.set(None)); }
    }
}

fn reserve(read: impl FnOnce(&mut MetadataBudget) -> bool) -> bool {
    let _scope = Scope::new(MetadataOptions::default());
    ACTIVE.with(|active| {
        let mut budget = active.get().expect("metadata reader budget scope");
        let admitted = read(&mut budget);
        active.set(Some(budget));
        admitted
    })
}

pub fn text(bytes: usize) -> bool {
    reserve(|budget| {
        let cost = bytes.saturating_mul(4).saturating_add(256);
        if bytes > budget.tag_limit || cost > budget.text_left || budget.items_left == 0 { return false; }
        budget.text_left -= cost;
        budget.items_left -= 1;
        true
    })
}

pub fn visual_frame(bytes: usize) -> bool {
    reserve(|budget| {
        if budget.visual_limit == 0 || bytes > budget.visual_limit.saturating_add(VISUAL_HEADER_BYTES)
            || bytes > budget.visual_left || budget.items_left == 0 { return false; }
        budget.visual_left -= bytes;
        budget.items_left -= 1;
        true
    })
}

pub fn visual_allowed(bytes: usize) -> bool {
    reserve(|budget| bytes <= budget.visual_limit)
}

// Base64 requires a bounded encoded buffer and a decoded buffer before the
// format-specific image parser can reserve the retained visual itself.
pub(crate) fn encoded_visual(bytes: usize) -> bool {
    reserve(|budget| {
        let maximum = (budget.visual_limit + VISUAL_HEADER_BYTES).div_ceil(3) * 4 + 32;
        if budget.visual_limit == 0 || bytes > maximum || bytes > budget.encoded_left
            || budget.items_left == 0 { return false; }
        budget.encoded_left -= bytes;
        budget.items_left -= 1;
        true
    })
}

pub(crate) fn unsynchronised_visual(bytes: usize) -> bool {
    reserve(|budget| {
        // Unsynchronisation can insert at most one zero after each source byte.
        let maximum = (budget.visual_limit + VISUAL_HEADER_BYTES).saturating_mul(2);
        if budget.visual_limit == 0 || bytes > maximum || bytes > budget.encoded_left
            || budget.items_left == 0 { return false; }
        budget.encoded_left -= bytes;
        budget.items_left -= 1;
        true
    })
}

/// Reserve incremental container metadata packet assembly, before a demuxer
/// creates its full buffer. Embedded readers reserve retained values separately.
pub fn metadata_packet(assembled_bytes: usize, new_bytes: usize) -> bool {
    reserve(|budget| {
        let maximum = if budget.visual_limit == 0 { MAX_TEXT_BYTES } else { 12 * 1024 * 1024 };
        if assembled_bytes.saturating_add(new_bytes) > maximum || new_bytes > budget.encoded_left
            || budget.items_left == 0 { return false; }
        budget.encoded_left -= new_bytes;
        budget.items_left -= 1;
        true
    })
}

pub(crate) struct Depth;
impl Depth {
    pub(crate) fn enter() -> Option<Self> {
        if reserve(|budget| {
            if budget.depth >= MAX_DEPTH { false } else { budget.depth += 1; true }
        }) { Some(Self) } else { None }
    }
}
impl Drop for Depth {
    fn drop(&mut self) { reserve(|budget| { budget.depth -= 1; true }); }
}
