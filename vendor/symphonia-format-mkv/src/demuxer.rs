// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::num::NonZero;

use symphonia_core::errors::{Error, Result, SeekErrorKind, seek_error, unsupported_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_MKV;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::TimeBase;

use log::{info, warn};

use crate::codecs::make_track_codec_params;
use crate::ebml::{EbmlElementInfo, EbmlError, EbmlIterator, EbmlSchema, ReadEbml};
use crate::lacing::{Frame, extract_frames};
use crate::schema::{MkvElement, MkvSchema};
use crate::segment::{
    AttachmentsElement, BlockGroupElement, ChaptersElement, CuesElement, EbmlHeaderElement,
    InfoElement, MatroskaTicks, NonZeroMatroskaTicks, SeekHeadElement, SegmentTicks,
    TagsElement, TargetTagsMap, TracksElement,
};

const MKV_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_MKV, short_name: "matroska", long_name: "Matroska / WebM" };

// CodecDelay and DiscardPadding are always nanoseconds. Packet timing uses the same
// exact unit so neither is rounded to the (often millisecond) Segment timebase.
const PACKET_TIME_BASE: TimeBase = TimeBase::new(
    NonZero::new(1).expect("1 is non-zero"),
    NonZero::new(1_000_000_000).expect("1_000_000_000 is non-zero"),
);

pub struct TrackState {
    /// The Matroska track number (Symphonia's track ID).
    track_num: u32,
    /// The default frame duration.
    pub(crate) default_frame_duration: Option<NonZeroMatroskaTicks>,
    /// The codec delay.
    pub(crate) codec_delay: MatroskaTicks,
    /// The native timebase of block-relative timestamps and durations.
    pub(crate) track_time_base: TimeBase,
    /// The native timebase of cluster timestamps.
    pub(crate) segment_time_base: TimeBase,
}

/// Matroska (MKV) and WebM demultiplexer.
///
/// `MkvReader` implements a demuxer for the Matroska and WebM formats.
pub struct MkvReader<'s> {
    /// Iterator over EBML element headers
    iter: EbmlIterator<MediaSourceStream<'s>, MkvSchema>,
    /// Whether the underlying media source supports random access.
    is_seekable: bool,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    track_states: HashMap<u32, TrackState>,
    attachments: Vec<Attachment>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    cues: Option<CuesElement>,
    current_cluster: Option<ClusterState>,
    /// The first cluster's offset from the Segment data position.
    first_cluster_pos: u64,
    frames: VecDeque<Frame>,
}

#[derive(Copy, Clone, Debug)]
struct ClusterState {
    /// The cluster timestamp in Segment ticks..
    timestamp: Option<SegmentTicks>,
    /// The start position in bytes of the cluster.
    start: u64,
}

impl<'s> MkvReader<'s> {
    pub fn try_new(mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let _scope = symphonia_metadata::budget::Scope::new(Default::default());
        // Get the total length of the stream, if possible.
        let (is_seekable, total_len) = (mss.is_seekable(), mss.byte_len());

        match total_len {
            Some(len) if is_seekable => info!("stream is seekable with len={len} bytes."),
            _ => (),
        }

        let mut it = EbmlIterator::new(mss, MkvSchema, total_len);

        // Read the EBML header.
        let ebml = it.next_element::<EbmlHeaderElement>()?;

        if !matches!(ebml.doc_type.as_str(), "matroska" | "webm") {
            return unsupported_error("mkv: not a matroska / webm file");
        }

        // Read the root element, Segment.
        let segment_pos = match it.next_header()? {
            Some(elem) if elem.element_type() == MkvElement::Segment => elem.data_pos(),
            _ => return unsupported_error("mkv: missing segment element"),
        };

        // Descend into the Segment element.
        it.push_element()?;

        let mut segment_tracks = None;
        let mut info = None;
        let mut cues = None;
        let mut current_cluster = None;
        let mut seek_positions = Vec::new();
        let mut tags = Vec::new();
        let mut attachments = None;
        let mut chapters = None;

        while let Ok(Some(header)) = it.next_header() {
            match header.element_type() {
                MkvElement::SeekHead => {
                    let seek_head = it.read_master_element::<SeekHeadElement>()?;
                    for element in seek_head.seeks.into_vec() {
                        let element_type = match it.schema().get_element_info(element.id as u32) {
                            Some(info) => info.element_type(),
                            None => continue,
                        };
                        seek_positions.push((element_type, element.position));
                    }
                }
                MkvElement::Tracks => {
                    segment_tracks = Some(it.read_master_element::<TracksElement>()?);
                }
                MkvElement::Info => {
                    info = Some(it.read_master_element::<InfoElement>()?);
                }
                MkvElement::Cues => {
                    cues = Some(it.read_master_element::<CuesElement>()?);
                }
                MkvElement::Tags => {
                    // Multiple tags element per segment allowed.
                    if let Some(tag) = it.read_metadata_master::<TagsElement>()? {
                        tags.push(tag);
                    }
                }
                MkvElement::Cluster => {
                    // Set state for current cluster for the first call of `next_element`.
                    current_cluster =
                        Some(ClusterState { timestamp: None, start: header.pos() - segment_pos });

                    // Don't look forward into the stream since we can't be sure that we'll
                    // find anything useful.
                    break;
                }
                MkvElement::Attachments => {
                    // Only one attachments element per segment is expected.
                    if attachments.is_some() {
                        log::warn!("unexpected attachments element");
                    }
                    attachments = it.read_metadata_master::<AttachmentsElement>()?;
                }
                MkvElement::Chapters => {
                    // Only one chapters element per segment is expected.
                    if chapters.is_some() {
                        log::warn!("unexpected chapters element");
                    }
                    chapters = it.read_metadata_master::<ChaptersElement>()?;
                }
                other => {
                    log::debug!("top-level scan ignored element {other:?}");
                }
            }
        }

        if is_seekable {
            // All elements preceeding the element iterator's current position have already been
            // read and do not need to be revisited.
            seek_positions.retain(|sp| sp.1 >= it.pos());
            // Make sure we don't jump backwards unnecessarily.
            seek_positions.sort_by_key(|sp| sp.1);

            for (_, pos) in seek_positions {
                // Ascend back to the segment element.
                it.pop_elements_upto(MkvElement::Segment)?;

                // Seek the iterator to the child element.
                it.seek_to_child(pos)?;

                // Resume iteration.
                let element_type = match it.next_header()? {
                    Some(header) => header.element_type(),
                    _ => continue,
                };

                // Safety: The element type or position may be incorrect. The element iterator will
                // validate the type (as declared in the header) of the element at the seeked
                // position against the element type asked to be read.
                match element_type {
                    MkvElement::Tracks => {
                        segment_tracks = Some(it.read_master_element::<TracksElement>()?);
                    }
                    MkvElement::Info => {
                        info = Some(it.read_master_element::<InfoElement>()?);
                    }
                    MkvElement::Tags => {
                        // Multiple tags element per segment allowed.
                        if let Some(tag) = it.read_metadata_master::<TagsElement>()? {
                            tags.push(tag);
                        }
                    }
                    MkvElement::Cues => {
                        cues = Some(it.read_master_element::<CuesElement>()?);
                    }
                    MkvElement::Attachments => {
                        // Only one attachments element per segment is expected.
                        if attachments.is_some() {
                            log::warn!("unexpected attachments element after meta seek");
                        }
                        attachments = it.read_metadata_master::<AttachmentsElement>()?;
                    }
                    MkvElement::Chapters => {
                        // Only one chapters element per segment is expected.
                        if chapters.is_some() {
                            log::warn!("unexpected chapters element after meta seek");
                        }
                        chapters = it.read_metadata_master::<ChaptersElement>()?;
                    }
                    _ => (),
                }
            }
        }

        let segment_tracks =
            segment_tracks.ok_or(Error::DecodeError("mkv: missing Tracks element"))?;

        // If seekable, seek to the start of the first cluster, if known, or the start of the
        // segment. If unseekable, the element iterator is already positioned at the start of the
        // first cluster.
        if is_seekable {
            let cluster_pos = current_cluster.as_ref().map(|cluster| cluster.start).unwrap_or(0);
            it.seek_to_child(cluster_pos)?;
            let _ = it.next_header()?;
        }

        // Descend into the cluster.
        it.push_element()?;

        let info = info.ok_or(Error::DecodeError("mkv: missing Info element"))?;

        // Create a hashmap of all per-target tags (edition, chapter, & attachment tags).
        let mut per_target_tags: TargetTagsMap = Default::default();

        segment_tracks.get_target_uids(&mut per_target_tags);

        if let Some(chapters) = &chapters {
            chapters.get_target_uids(&mut per_target_tags);
        }
        if let Some(attachments) = &attachments {
            attachments.get_target_uids(&mut per_target_tags);
        }

        // Begin with externally provided metadata and chapters.
        let mut metadata = opts.external_data.metadata.unwrap_or_default();

        // Post-process all tag elements into metadata revisions, while also collecting per-target
        // tags.
        let is_video = segment_tracks.tracks.as_ref().iter().any(|t| t.video.is_some());
        for tag in tags {
            metadata.push(tag.into_metadata(&mut per_target_tags, is_video));
        }

        // Post-process chapters element.
        let chapters = chapters
            .map(|chapters| chapters.into_chapter_group(&mut per_target_tags))
            .unwrap_or(opts.external_data.chapters);

        // Process attachments element.
        let attachments = attachments
            .map(|attachments| attachments.into_attachments(&mut per_target_tags))
            .unwrap_or_default();

        // Should TimeBase use a u64/u64 rational?
        // Reduce the timebase to reduce the chance of overflows later.
        let time_base = TimeBase::new(
            info.timestamp_scale
                .try_into()
                .map_err(|_| Error::Unsupported("mkv: timestamp scale too large (report this)"))?,
            NonZero::new(1_000_000_000).expect("1_000_000_000 is non-zero"),
        )
        .reduce();

        let mut tracks = Vec::new();
        let mut track_states = HashMap::new();

        for track in segment_tracks.tracks {
            // The track's timebase is scaled by the track timestamp scale.
            let track_time_base = time_base
                .scale(track.track_timestamp_scale)
                .ok_or(Error::DecodeError("mkv: track timebase is invalid"))?;

            // Create the track state.
            let state = TrackState {
                // TODO: This should be 64-bit, but track IDs are 32-bit.
                track_num: u32::try_from(track.number.get())
                    .map_err(|_| Error::Unsupported("mkv: track number too large (report this)"))?,
                default_frame_duration: track.default_duration,
                codec_delay: track.codec_delay,
                track_time_base,
                segment_time_base: time_base,
            };

            // Create the track.
            let mut tr = Track::new(state.track_num);

            tr.with_time_base(PACKET_TIME_BASE);

            if let Some(lang_bcp47) = &track.lang_bcp47 {
                tr.with_language(lang_bcp47);
            }
            else {
                tr.with_language(&track.lang);
            }

            tr.with_flags(track.flags);

            if let Some(codec_params) = make_track_codec_params(track)? {
                tr.with_codec_params(codec_params);
            }

            tracks.push(tr);
            track_states.insert(state.track_num, state);
        }

        // Populate media information.
        let mut media_info = MediaInfo::new();

        media_info.with_time_base(time_base);

        if let Some(duration) = info.duration {
            media_info.with_duration(Duration::new(duration.get().round() as u64));
        }

        Ok(Self {
            iter: it,
            is_seekable,
            media_info,
            tracks,
            track_states,
            attachments,
            chapters,
            metadata,
            cues,
            current_cluster,
            first_cluster_pos: current_cluster.as_ref().map(|cluster| cluster.start).unwrap_or(0),
            frames: VecDeque::new(),
        })
    }

    fn seek_track_by_ts_forward(&mut self, track_id: u32, ts: Timestamp) -> Result<SeekedTo> {
        let actual_ts = 'out: loop {
            // Skip frames from the buffer until the given timestamp
            while let Some(frame) = self.frames.front() {
                let next_frame_pts = frame
                    .pts
                    .checked_add(frame.dur)
                    .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

                if next_frame_pts >= ts && frame.track_num == track_id {
                    break 'out frame.pts;
                }
                else {
                    self.frames.pop_front();
                }
            }

            if !self.next_element()? {
                // There are no more elements.
                return Err(Error::SeekError(SeekErrorKind::OutOfRange));
            }
        };

        Ok(SeekedTo { track_id, required_ts: ts, actual_ts })
    }

    fn seek_track_by_ts_atomic(
        &mut self,
        id: u32,
        tb: TimeBase,
        ts: Timestamp,
    ) -> Result<SeekedTo> {
        if !self.is_seekable {
            // A streaming source cannot rewind or restore a failed seek. Continue
            // scanning its existing frame queue without discarding pending laces.
            return self.seek_track_by_ts_forward(id, ts);
        }

        // Save the iterator and cluster states to restore in-case of and error.
        let iter_state = self.iter.save_state();
        let cluster_state = self.current_cluster;
        let frames = std::mem::take(&mut self.frames);

        match self.seek_track_by_ts(id, tb, ts) {
            Err(err) => {
                // Restore saved iterator and cluster states.
                self.iter.restore_state(iter_state)?;
                self.current_cluster = cluster_state;
                self.frames = frames;
                Err(err)
            }
            value => value,
        }
    }

    fn seek_track_by_ts(&mut self, id: u32, _tb: TimeBase, ts: Timestamp) -> Result<SeekedTo> {
        log::debug!("seeking track_id={id} to ts={ts}");

        // If cues exist, seek to the nearest cue point.
        if let Some(cues) = &self.cues {
            let mut target_cue_point = None;

            // Cue timestamps are Segment ticks, while packet timestamps are nanoseconds.
            let ts = u64::try_from(ts.get())
                .map(MatroskaTicks::from)
                .map_err(|_| Error::SeekError(SeekErrorKind::OutOfRange))?
                .into_segment_ticks(
                    self.media_info.time_base.expect("media info time base always populated"),
                );

            for cue_point in &cues.points {
                if cue_point.time > ts {
                    break;
                }
                target_cue_point = Some(cue_point);
            }

            let target_cue_point =
                target_cue_point.ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

            log::debug!(
                "found cue point: track_id={}, ts={}, seg_pos={}",
                target_cue_point.positions.track.get(),
                target_cue_point.time,
                target_cue_point.positions.cluster_pos
            );

            // Ascend back to the segment element.
            self.iter.pop_elements_upto(MkvElement::Segment)?;

            // Seek to the specific cluster element.
            self.iter.seek_to_child(target_cue_point.positions.cluster_pos)?;

            // Resume iteration.
            let cluster = match self.iter.next_header()? {
                // The seeked element is a cluster.
                Some(header) if header.element_type() == MkvElement::Cluster => header,
                // The seeked element is not a cluster or there were no more elements at the cue
                // position. The cue point was malformed.
                _ => return seek_error(SeekErrorKind::Unseekable),
            };

            // Cue timestamps are already in Segment ticks.
            let timestamp = target_cue_point.time;

            // Update the current cluster metadata.
            self.current_cluster =
                Some(ClusterState { timestamp: Some(timestamp), start: cluster.pos() });

            // Descend into the cluster element.
            self.iter.push_element()?;

            // If a cluster relative position is available, seek to the exact simple block or
            // block group element.
            if let Some(cluster_rel_pos) = target_cue_point.positions.cluster_rel_pos {
                self.iter.seek_to_child(cluster_rel_pos)?;
            }
        }
        else if self.is_seekable {
            // Without an index, rescan from the first cluster so backward seeks (including
            // seeking after EOF) do not depend on the current iterator position.
            self.iter.pop_elements_upto(MkvElement::Segment)?;
            self.iter.seek_to_child(self.first_cluster_pos)?;
            self.current_cluster = None;
        }

        // Seek to exact block.
        self.seek_track_by_ts_forward(id, ts)
    }

    fn next_element(&mut self) -> Result<bool> {
        match self.iter.next_header()? {
            None => {
                // The EBML iterator has consumed all child elements at the current level of the
                // document.
                match self.iter.parent() {
                    None => {
                        // The parent is the document. The media has ended.
                        return Ok(false);
                    }
                    Some(parent) if parent.element_type() == MkvElement::Segment => {
                        // Keep the Segment as an ancestor so a later seek can revisit its
                        // clusters, even after a sized Segment has been fully consumed.
                        return Ok(false);
                    }
                    Some(parent) if parent.element_type() == MkvElement::Cluster => {
                        // The parent was a cluster element. Reset the cluster state.
                        self.current_cluster = None;
                    }
                    _ => (),
                }

                // Ascend to its parent.
                self.iter.pop_element()?;
            }
            Some(child) => {
                match child.element_type() {
                    // Cluster element.
                    MkvElement::Cluster => {
                        self.current_cluster =
                            Some(ClusterState { timestamp: None, start: child.data_pos() });

                        // Descend into the cluster.
                        self.iter.push_element()?;
                    }
                    // Children of a cluster element.
                    MkvElement::Timestamp => {
                        // Cluster timestamp element.
                        match self.current_cluster.as_mut() {
                            Some(cc) => {
                                cc.timestamp = self.iter.read_u64()?.map(SegmentTicks::from)
                            }
                            _ => log::warn!("expected to have cluster"),
                        }
                    }
                    block_type @ (MkvElement::SimpleBlock | MkvElement::BlockGroup) => {
                        // Get the current cluster information.
                        let Some(cluster) = self.current_cluster.as_ref()
                        else {
                            log::warn!("expected to have cluster");
                            return Ok(true);
                        };

                        // Get the cluster timestamp.
                        let Some(cluster_ts) = cluster.timestamp
                        else {
                            log::warn!("missing cluster timestamp");
                            return Ok(true);
                        };

                        // Get block data, duration, and exact nanosecond padding.
                        let (data, duration, discard_padding) = match block_type {
                            MkvElement::SimpleBlock => (self.iter.read_binary()?, None, None),
                            MkvElement::BlockGroup => {
                                let group = self.iter.read_master_element::<BlockGroupElement>()?;
                                (group.data, group.duration, group.discard_padding)
                            }
                            _ => unreachable!(),
                        };

                        // Extract frames.
                        if !extract_frames(
                            &data,
                            duration,
                            discard_padding,
                            cluster_ts,
                            &self.track_states,
                            &mut self.frames,
                        )? {
                            warn!("pts for block is too large");
                            return Ok(false);
                        }
                    }
                    // All other elements.
                    other => {
                        log::debug!("ignored element {other:?}");
                    }
                }
            }
        }

        Ok(true)
    }
}

impl ProbeableFormat<'_> for MkvReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>>
    where
        Self: Sized,
    {
        Ok(Box::new(MkvReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            MKV_FORMAT_INFO,
            &["webm", "mkv"],
            &["video/webm", "video/x-matroska"],
            &[b"\x1A\x45\xDF\xA3"] // Top-level element Ebml element
        )]
    }
}

impl FormatReader for MkvReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &MKV_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    fn chapters(&self) -> Option<&ChapterGroup> {
        self.chapters.as_ref()
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if self.tracks.is_empty() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        match to {
            SeekTo::Time { time, track_id } => {
                let track = match track_id {
                    Some(id) => self.tracks.iter().find(|track| track.id == id),
                    None => self.tracks.first(),
                };
                let track = track.ok_or(Error::SeekError(SeekErrorKind::InvalidTrack))?;
                let tb = track.time_base.expect("track always has a timebase");
                let ts = match tb.calc_timestamp(time) {
                    Some(ts) => ts,
                    None => {
                        warn!("seek ts is too large");
                        return seek_error(SeekErrorKind::OutOfRange);
                    }
                };
                let track_id = track.id;
                self.seek_track_by_ts_atomic(track_id, tb, ts)
            }
            SeekTo::Timestamp { ts, track_id } => {
                match self.tracks.iter().find(|t| t.id == track_id) {
                    Some(track) => {
                        let tb = track.time_base.expect("track always has a timebase");
                        self.seek_track_by_ts_atomic(track_id, tb, ts)
                    }
                    None => seek_error(SeekErrorKind::InvalidTrack),
                }
            }
        }
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(frame) = self.frames.pop_front() {
                let mut packet = Packet::new(frame.track_num, frame.pts, frame.dur, frame.data);
                packet.trim_start = frame.trim_start;
                packet.trim_end = frame.trim_end;
                return Ok(Some(packet));
            }

            if !self.next_element()? {
                // Reached the end of stream.
                return Ok(None);
            }
        }
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.iter.into_inner()
    }
}

impl Scoreable for MkvReader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl ReadEbml for MediaSourceStream<'_> {}

impl From<EbmlError> for Error {
    fn from(value: EbmlError) -> Self {
        // All non-IO EBML errors are mapped to a decode error.
        let msg = match value {
            EbmlError::IoError(err) => return Error::IoError(err),
            EbmlError::InvalidEbmlElementIdLength => "mkv (ebml): invalid ebml element id length",
            EbmlError::InvalidEbmlDataLength => "mkv (ebml): invalid ebml vint length",
            EbmlError::UnknownElement => "mkv (ebml): the element is unknown",
            EbmlError::UnknownElementDataSize => "mkv (ebml): the element data size is unknown",
            EbmlError::UnexpectedElement => "mkv (ebml): encountered an unexpected element",
            EbmlError::UnexpectedElementDataType => {
                "mkv (ebml): unexpected element data type for the operation"
            }
            EbmlError::UnexpectedElementDataSize => {
                "mkv (ebml): unexpected data size for the element's data type"
            }
            EbmlError::NoElement => "mkv (ebml): no current element",
            EbmlError::NoParent => "mkv (ebml): no parent element",
            EbmlError::NotAnAncestor => {
                "mkv (ebml): the element is not an ancestor of the current element"
            }
            EbmlError::Overrun => "mkv (ebml): the element was overrun when read",
            EbmlError::ExpectedMasterElement => "mkv (ebml): expected a master element",
            EbmlError::ExpectedNonMasterElement => "mkv (ebml): expected a non-master element",
            EbmlError::SeekOutOfRange => "mkv (ebml): the seek was out of range",
            EbmlError::BufferTooSmall => "mkv (ebml): the buffer is too small",
            EbmlError::MaximumDepthReached => "mkv (ebml): maximum ebml document depth reached",
            EbmlError::ElementError(err) => err,
        };
        Error::DecodeError(msg)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn element(id: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut bytes = id.to_vec();
        bytes.extend_from_slice(&((1u64 << 56) | payload.len() as u64).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn uint(id: &[u8], value: u64) -> Vec<u8> {
        element(id, &value.to_be_bytes())
    }

    fn block(timestamp: i16, flags: u8, data: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0x81];
        bytes.extend_from_slice(&timestamp.to_be_bytes());
        bytes.push(flags);
        bytes.extend_from_slice(data);
        bytes
    }

    fn group(timestamp: i16, flags: u8, data: &[u8], padding: i64) -> Vec<u8> {
        let mut payload = element(&[0xa1], &block(timestamp, flags, data));
        payload.extend(element(&[0x75, 0xa2], &padding.to_be_bytes()));
        element(&[0xa0], &payload)
    }

    fn cluster(timestamp: u64, blocks: &[u8]) -> Vec<u8> {
        let mut payload = uint(&[0xe7], timestamp);
        payload.extend_from_slice(blocks);
        element(&[0x1f, 0x43, 0xb6, 0x75], &payload)
    }

    fn cues(positions: &[u64]) -> Vec<u8> {
        let mut points = Vec::new();
        for (index, &position) in positions.iter().enumerate() {
            let mut track_position = uint(&[0xf7], 1);
            track_position.extend(uint(&[0xf1], position));
            let mut point = uint(&[0xb3], index as u64 * 100);
            point.extend(element(&[0xb7], &track_position));
            points.extend(element(&[0xbb], &point));
        }
        element(&[0x1c, 0x53, 0xbb, 0x6b], &points)
    }

    fn fixture(clusters: &[Vec<u8>], indexed: bool, codec_delay: u64) -> Vec<u8> {
        let mut info = element(&[0x4d, 0x80], b"test");
        info.extend(element(&[0x57, 0x41], b"test"));
        let mut segment = element(&[0x15, 0x49, 0xa9, 0x66], &info);
        let mut track = uint(&[0xd7], 1);
        track.extend(uint(&[0x73, 0xc5], 1));
        track.extend(element(&[0x86], b"A_OPUS"));
        track.extend(uint(&[0x23, 0xe3, 0x83], 20_000_000));
        track.extend(uint(&[0x56, 0xaa], codec_delay));
        let mut audio = element(&[0xb5], &48_000f64.to_be_bytes());
        audio.extend(uint(&[0x9f], 1));
        track.extend(element(&[0xe1], &audio));
        segment.extend(element(&[0x16, 0x54, 0xae, 0x6b], &element(&[0xae], &track)));
        if indexed {
            let mut position = segment.len() as u64 + cues(&vec![0; clusters.len()]).len() as u64;
            let positions: Vec<_> = clusters.iter().map(|cluster| {
                let start = position;
                position += cluster.len() as u64;
                start
            }).collect();
            segment.extend(cues(&positions));
        }
        for cluster in clusters {
            segment.extend_from_slice(cluster);
        }
        let mut bytes = element(&[0x1a, 0x45, 0xdf, 0xa3], &element(&[0x42, 0x82], b"webm"));
        bytes.extend(element(&[0x18, 0x53, 0x80, 0x67], &segment));
        bytes
    }

    fn reader(bytes: Vec<u8>) -> MkvReader<'static> {
        let stream = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
        MkvReader::try_new(stream, Default::default()).unwrap()
    }

    fn seek(reader: &mut MkvReader<'_>, ns: i64) -> SeekedTo {
        reader.seek(
            SeekMode::Accurate,
            SeekTo::Timestamp { ts: Timestamp::new(ns), track_id: 1 },
        ).unwrap()
    }

    #[test]
    fn opus_discard_padding_preserves_exact_sample_count() {
        // Six 20ms packets decode 5760 samples. OpusHead's 312-sample pre-skip
        // plus the final 13.5ms/648-sample discard leave exactly 100ms at 48kHz.
        let mut blocks = Vec::new();
        for index in 0..6 {
            blocks.extend(group(index * 20, 0, &[index as u8], if index == 5 { 13_500_000 } else { 0 }));
        }
        let mut reader = reader(fixture(&[cluster(0, &blocks)], false, 6_500_000));
        assert_eq!(reader.tracks()[0].time_base, Some(PACKET_TIME_BASE));
        assert_eq!(reader.media_info().time_base.unwrap().denom.get(), 1000);
        let mut packets = Vec::new();
        while let Some(packet) = reader.next_packet().unwrap() {
            packets.push(packet);
        }
        assert_eq!(packets.len(), 6);
        assert_eq!(packets[0].pts.get(), -6_500_000);
        assert_eq!(packets[5].trim_end.get(), 13_500_000);
        assert_eq!(packets[5].dur.get(), 6_500_000);
        let trimmed_samples: u64 = packets.iter().map(|packet| {
            packet.trim_end.get() * 48_000 / 1_000_000_000
        }).sum();
        assert_eq!(5760 - 312 - trimmed_samples, 4800);
    }

    #[test]
    fn discard_padding_applies_only_at_lace_boundary() {
        // Three one-byte frames, using each supported lacing scheme.
        for (flags, data) in [
            (0x02, vec![2, 1, 1, 10, 11, 12]),
            (0x04, vec![2, 10, 11, 12]),
            (0x06, vec![2, 0x81, 0xbf, 10, 11, 12]),
        ] {
            for padding in [13_500_000i64, -13_500_000, 25_000_000, -25_000_000] {
                let blocks = group(0, flags, &data, padding);
                let mut reader = reader(fixture(&[cluster(0, &blocks)], false, 0));
                let packets: Vec<_> = (0..3).map(|_| reader.next_packet().unwrap().unwrap()).collect();
                assert!(reader.next_packet().unwrap().is_none());
                assert_eq!(packets.iter().map(|p| p.trim_start.get() + p.trim_end.get()).sum::<u64>(), padding.unsigned_abs());
                assert_eq!(packets.iter().map(|p| p.dur.get()).sum::<u64>(), 60_000_000 - padding.unsigned_abs());
                let untouched = if padding < 0 { 2 } else { 0 };
                assert_eq!(packets[untouched].dur.get(), 20_000_000);
                assert_eq!(packets[untouched].trim_start.get(), 0);
                assert_eq!(packets[untouched].trim_end.get(), 0);
                assert_eq!(packets[1].pts.get(), 20_000_000);
            }
        }
    }

    #[test]
    fn backward_seek_and_seek_after_sized_segment_eof() {
        let clusters = [
            cluster(0, &element(&[0xa3], &block(0, 0x04, &[2, 10, 11, 12]))),
            cluster(100, &element(&[0xa3], &block(0, 0, &[20]))),
        ];
        for indexed in [false, true] {
            let mut reader = reader(fixture(&clusters, indexed, 0));
            assert_eq!(&*reader.next_packet().unwrap().unwrap().data, &[10]);
            // A seek must discard the pending second and third laced frames.
            assert_eq!(seek(&mut reader, 100_000_000).actual_ts.get(), 100_000_000);
            assert_eq!(&*reader.next_packet().unwrap().unwrap().data, &[20]);
            assert!(reader.next_packet().unwrap().is_none());
            assert!(reader.next_packet().unwrap().is_none());
            assert_eq!(seek(&mut reader, 0).actual_ts.get(), 0);
            assert_eq!(&*reader.next_packet().unwrap().unwrap().data, &[10]);
            seek(&mut reader, 100_000_000);
            seek(&mut reader, 0);
            assert_eq!(&*reader.next_packet().unwrap().unwrap().data, &[10]);
        }
    }

    #[test]
    fn failed_seek_restores_pending_laced_frames() {
        let cluster = cluster(0, &element(&[0xa3], &block(0, 0x04, &[2, 10, 11, 12])));
        let mut reader = reader(fixture(&[cluster], false, 0));
        assert_eq!(&*reader.next_packet().unwrap().unwrap().data, &[10]);
        assert!(reader.seek(
            SeekMode::Accurate,
            SeekTo::Timestamp { ts: Timestamp::new(1_000_000_000), track_id: 1 },
        ).is_err());
        assert_eq!(&*reader.next_packet().unwrap().unwrap().data, &[11]);
        assert_eq!(&*reader.next_packet().unwrap().unwrap().data, &[12]);
    }

    #[test]
    fn native_timestamp_scale_does_not_quantize_padding() {
        let tracks = HashMap::from([(1, TrackState {
            track_num: 1,
            default_frame_duration: None,
            codec_delay: MatroskaTicks::from(6_500_000),
            track_time_base: TimeBase::new(NonZero::new(1).unwrap(), NonZero::new(500).unwrap()),
            segment_time_base: TimeBase::new(NonZero::new(1).unwrap(), NonZero::new(1000).unwrap()),
        })]);
        let mut frames = VecDeque::new();
        assert!(extract_frames(
            &block(3, 0, &[1]),
            Some(crate::segment::TrackTicks::from(10)),
            Some(crate::segment::SignedMatroskaTicks::from(13_500_000)),
            SegmentTicks::from(100),
            &tracks,
            &mut frames,
        ).unwrap());
        let frame = frames.pop_front().unwrap();
        assert_eq!(frame.pts.get(), 99_500_000);
        assert_eq!(frame.dur.get(), 20_000_000);
        assert_eq!(frame.trim_end.get(), 13_500_000);
    }

    #[test]
    fn discard_padding_is_not_bounded_by_presented_block_duration() {
        let base = TimeBase::new(NonZero::new(1).unwrap(), NonZero::new(1000).unwrap());
        let tracks = HashMap::from([(1, TrackState {
            track_num: 1,
            default_frame_duration: None,
            codec_delay: MatroskaTicks::from(0),
            track_time_base: base,
            segment_time_base: base,
        })]);
        for padding in [13_500_000, -13_500_000] {
            let mut frames = VecDeque::new();
            assert!(extract_frames(
                &block(0, 0, &[1]),
                Some(crate::segment::TrackTicks::from(7)),
                Some(crate::segment::SignedMatroskaTicks::from(padding)),
                SegmentTicks::from(0), &tracks, &mut frames,
            ).unwrap());
            let frame = frames.pop_front().unwrap();
            assert_eq!(frame.dur.get(), 7_000_000);
            assert_eq!(frame.trim_start.get(), if padding < 0 { 13_500_000 } else { 0 });
            assert_eq!(frame.trim_end.get(), if padding > 0 { 13_500_000 } else { 0 });
        }
    }

    #[test]
    fn truncated_lacing_is_not_clean_eof() {
        // Two Xiph-laced frames, but the first size has no terminating byte.
        let packet = element(&[0xa3], &block(0, 0x02, &[1, 0xff]));
        let mut reader = reader(fixture(&[cluster(0, &packet)], false, 0));
        assert!(matches!(reader.next_packet(), Err(Error::IoError(error))
            if error.kind() == std::io::ErrorKind::UnexpectedEof));
    }
}
