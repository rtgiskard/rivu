// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia_core::support_format;

use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_ISOMP4;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog, RawTag, RawValue};
use symphonia_core::units::Time;

use std::collections::HashMap;
use std::io::{Seek, SeekFrom};
use std::num::NonZero;
use std::sync::Arc;

use crate::atoms::{AtomError, AtomIterator, AtomType, ReadAtom};
use crate::atoms::{FtypAtom, MetaAtom, MoofAtom, MoovAtom, SidxAtom, TrakAtom};
use crate::atoms::hdlr::HandlerType;
use crate::stream::*;

use log::{debug, info, trace, warn};

const ISOMP4_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FORMAT_ID_ISOMP4,
    short_name: "isomp4",
    long_name: "ISO Base Media File Format",
};

/// The presented span of a track's samples in media timescale units. Samples outside of it are
/// encoder delay and padding, and the reader's timestamps are relative to its start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Playable {
    start: u64,
    end: u64,
}

impl Playable {
    /// Every sample of the track.
    const ALL: Playable = Playable { start: 0, end: u64::MAX };

    /// The span of a media edit starting at a media time, with a duration in the movie timescale.
    /// A zero duration extends to the end of the track.
    fn from_edit(
        (start, dur): (u64, u64),
        movie_timescale: NonZero<u32>,
        media_timescale: NonZero<u32>,
    ) -> Self {
        let scale = u128::from(movie_timescale.get());
        let dur = (u128::from(dur) * u128::from(media_timescale.get()) + scale / 2) / scale;
        let end = match u64::try_from(dur) {
            Ok(dur) if dur > 0 => start.saturating_add(dur),
            _ => u64::MAX,
        };
        Playable { start, end }
    }

    /// The span in frames declared by an iTunes gapless tag: hexadecimal fields of which the
    /// second to fourth are the encoder delay, padding, and frame count.
    fn from_itunes_gapless(tag: &RawTag) -> Option<Self> {
        let RawValue::String(value) = &tag.value
        else {
            return None;
        };

        if !tag.key.eq_ignore_ascii_case("com.apple.iTunes:iTunSMPB") {
            return None;
        }

        let mut fields =
            value.split_ascii_whitespace().map(|field| u64::from_str_radix(field, 16).ok());
        let start = fields.nth(1)??;
        let frames = fields.nth(1)??;

        Some(Playable { start, end: start.saturating_add(frames) })
    }

    /// The presented duration of a track with the given total duration.
    fn duration(self, total: u64) -> u64 {
        total.min(self.end).saturating_sub(self.start)
    }

    /// The media timestamp of a presented timestamp.
    fn media_ts(self, ts: Timestamp) -> Timestamp {
        ts.saturating_add(Duration::new(self.start))
    }

    /// The presented timestamp of a media timestamp.
    fn presented_ts(self, ts: Timestamp) -> Timestamp {
        ts.saturating_sub(Duration::new(self.start))
    }

    /// The leading and trailing durations of a sample, at a media timestamp, that lie outside
    /// of the span.
    fn trims(self, ts: Timestamp, dur: Duration) -> (Duration, Duration) {
        let (ts, dur) = (ts.get() as u64, dur.get());
        let start = self.start.saturating_sub(ts).min(dur);
        let end = ts.saturating_add(dur).saturating_sub(self.end).min(dur - start);
        (Duration::new(start), Duration::new(end))
    }
}

pub struct TrackState {
    /// The track number.
    track_num: usize,
    /// The track ID.
    track_id: u32,
    /// The current segment.
    cur_seg: usize,
    /// The current sample index relative to the track.
    next_sample: u32,
    /// The current sample byte position relative to the start of the track.
    next_sample_pos: u64,
    /// The presented span of the track.
    playable: Playable,
    /// Opus decoded durations count frames at 48 kHz, independently of the media timescale.
    opus_timescale: Option<NonZero<u32>>,
    /// An auxiliary seek placed this track beyond its authoritative playable or physical end.
    exhausted: bool,
}

impl TrackState {
    pub fn make(
        track_num: usize,
        trak: &TrakAtom,
        timespan: &TimeSpan,
        complete_duration: Option<u64>,
        movie_timescale: NonZero<u32>,
        gapless: Option<Playable>,
    ) -> (Self, Track) {
        let mut track = Track::new(trak.tkhd.id);

        // Create the codec parameters using the sample description atom.
        if let Some(codec_params) = trak.mdia.minf.stbl.stsd.make_codec_params() {
            track.with_codec_params(codec_params);
        }

        // If the track is an audio track, and the timescale is equal to the sample rate, then
        // timestamps count frames. This is the case for almost all audio tracks. If not, there is
        // no generic, low overhead, & precise way to determine the number of frames.
        let counts_frames = matches!(
            &track.codec_params,
            Some(CodecParameters::Audio(audio))
                if audio.sample_rate == Some(timespan.timescale.get())
        );

        let opus_header = match &track.codec_params {
            Some(CodecParameters::Audio(audio)) if audio.codec == CODEC_ID_OPUS => {
                audio.extra_data.as_deref()
            }
            _ => None,
        };
        // dOps fields remain big-endian in the version-0 identification header. Edits and
        // iTunSMPB are authoritative; pre-skip is only a fallback, never an additional delay.
        let opus_start = opus_header.and_then(|header| {
            let bytes: [u8; 2] = header.get(10..12)?.try_into().ok()?;
            Some(scale_opus_frames(u64::from(u16::from_be_bytes(bytes)), timespan.timescale))
        });
        let opus_timescale = opus_header.map(|_| timespan.timescale);

        // Only a complete sample table can prove that a media edit presents every sample.
        // Fragmented mdhd and SIDX durations are estimates, not evidence against an edit.
        // All-samples edits may instead carry an iTunes gapless tag, which counts frames.
        let playable = trak
            .edts
            .as_ref()
            .and_then(|edts| edts.elst.as_ref())
            .and_then(|elst| elst.media_edit())
            .map(|edit| Playable::from_edit(edit, movie_timescale, timespan.timescale))
            .filter(|playable| {
                playable.start > 0
                    || (playable.end != u64::MAX
                        && complete_duration.is_none_or(|total| playable.end < total))
            })
            .or(gapless.filter(|_| counts_frames))
            .or_else(|| opus_start.map(|start| Playable { start, end: u64::MAX }))
            .unwrap_or(Playable::ALL);

        // Populate timing information. Encoder delay precedes the presented origin and stays
        // seekable.
        track
            .with_time_base(TimeBase::from_recip(timespan.timescale))
            .with_start_ts(playable.presented_ts(Timestamp::ZERO));

        // A finite span declares its own duration. Only a complete sample table may cap an
        // unavailable physical end; fragmented duration estimates cannot shorten the span.
        let total = if playable.end != u64::MAX {
            Some(complete_duration.unwrap_or(playable.end))
        }
        else {
            complete_duration.or(timespan.duration.map(|duration| duration.get()))
        };
        if let Some(total) = total {
            let duration = playable.duration(total);
            track.with_duration(Duration::new(duration));
            if counts_frames {
                track.with_num_frames(duration);
            }
        }

        let state = Self {
            track_num,
            track_id: trak.tkhd.id,
            cur_seg: 0,
            next_sample: 0,
            next_sample_pos: 0,
            playable,
            opus_timescale,
            exhausted: false,
        };

        (state, track)
    }

    fn packet(&self, ts: Timestamp, dur: Duration, data: Box<[u8]>) -> Result<Packet> {
        let mut playable = self.playable;
        let decoded_dur = if let Some(timescale) = self.opus_timescale {
            // MP4 may shorten the final sample's duration instead of declaring an edit end.
            // Opus still decodes its full TOC duration, so expose that excess as padding too.
            playable.end = playable.end.min((ts.get() as u64).saturating_add(dur.get()));
            Duration::new(scale_opus_frames(opus_packet_frames(&data)?, timescale))
        }
        else {
            dur
        };
        let (trim_start, trim_end) = playable.trims(ts, decoded_dur);
        let mut packet = Packet::new(
            self.track_id,
            playable.presented_ts(ts),
            decoded_dur.saturating_sub(trim_start).saturating_sub(trim_end),
            data,
        );
        packet.trim_start = trim_start;
        packet.trim_end = trim_end;
        Ok(packet)
    }
}

fn scale_opus_frames(frames: u64, timescale: NonZero<u32>) -> u64 {
    (frames * u64::from(timescale.get()) + 24_000) / 48_000
}

/// The decoded duration from the Opus TOC (RFC 6716, section 3.1).
fn opus_packet_frames(data: &[u8]) -> Result<u64> {
    let Some(&toc) = data.first() else {
        return decode_error("isomp4: empty opus packet");
    };
    let config = toc >> 3;
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][usize::from(config & 3)],
        12..=15 => [480, 960][usize::from(config & 1)],
        _ => 120 << (config & 3),
    };
    let count = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => match data.get(1) {
            Some(count) => u64::from(count & 0x3f),
            None => return decode_error("isomp4: missing opus frame count"),
        },
    };
    let frames = frame * count;
    if frames == 0 || frames > 5760 {
        return decode_error("isomp4: invalid opus packet duration");
    }
    Ok(frames)
}

/// Whether the loaded segments provide a sample, need extending, or cover every playable span.
enum NextSample {
    Sample(NextSampleInfo),
    NeedSegment,
    EndOfSpans,
}

/// Information regarding the next sample.
#[derive(Debug)]
struct NextSampleInfo {
    /// The track number of the next sample.
    track_num: usize,
    /// The timestamp of the next sample.
    ts: Timestamp,
    /// The timestamp expressed in seconds.
    time: Time,
    /// The duration of the next sample.
    dur: Duration,
    /// The segment containing the next sample.
    seg_idx: usize,
}

/// Information regarding a sample.
#[derive(Debug)]
struct SampleDataInfo {
    /// The position of the sample in the track.
    pos: u64,
    /// The length of the sample.
    len: u32,
}

/// A representation of time, defining a duration relative to a specific frequency
#[derive(Debug)]
pub struct TimeSpan {
    pub timescale: NonZero<u32>,
    /// An optional duration estimate; absent for fragmented tracks with no duration declaration.
    pub duration: Option<Duration>,
}

impl Default for TimeSpan {
    fn default() -> Self {
        Self { timescale: NonZero::new(1).expect("1 is non-zero"), duration: None }
    }
}

impl TimeSpan {
    pub fn new(timescale: NonZero<u32>, duration: Duration) -> Self {
        TimeSpan { timescale, duration: Some(duration) }
    }
}

/// ISO Base Media File Format (MP4, M4A, MOV, etc.) demultiplexer.
///
/// `IsoMp4Reader` implements a demuxer for the ISO Base Media File Format.
pub struct IsoMp4Reader<'s> {
    iter: AtomIterator<MediaSourceStream<'s>>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    /// Segments of the movie. Sorted in ascending order by sequence number.
    segs: Vec<Box<dyn StreamSegment>>,
    /// State tracker for each track.
    track_states: Vec<TrackState>,
    /// Optional, movie extends atom used for fragmented streams.
    moov: Arc<MoovAtom>,
    /// Physical atom EOF (or a complete non-fragmented sample table), never a duration estimate.
    segments_ended: bool,
}

impl<'s> IsoMp4Reader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let _scope = symphonia_metadata::budget::Scope::new(Default::default());
        // To get to beginning of the atom.
        mss.seek_buffered_rel(-4);

        let is_seekable = mss.is_seekable();

        let mut ftyp = None;
        let mut moov = None;

        // Get the total length of the stream, if possible.
        let total_len = if is_seekable {
            let pos = mss.pos();
            let len = mss.seek(SeekFrom::End(0))?;
            mss.seek(SeekFrom::Start(pos))?;
            info!("stream is seekable with len={len} bytes.");
            Some(len)
        }
        else {
            None
        };

        let mut metadata = opts.external_data.metadata.unwrap_or_default();

        // Parse all atoms if the stream is seekable, otherwise parse all atoms up-to the mdat atom.
        let mut it = AtomIterator::new(mss, total_len);
        // Maps each track id to its cumulative duration (TimeSpan) as parsed from the segment
        // index.
        let mut sidx_timespans: HashMap<u32, TimeSpan> = HashMap::new();

        while let Some(header) = it.next_header()? {
            // Top-level atoms.
            match header.atom_type() {
                AtomType::FileType => {
                    ftyp = Some(it.read_atom::<FtypAtom>()?);
                }
                AtomType::Movie => {
                    moov = Some(it.read_atom::<MoovAtom>()?);
                }
                AtomType::SegmentIndex => {
                    let sidx = it.read_atom::<SidxAtom>()?;

                    // Calculate the total duration, per track, from the segment index atoms.
                    let sidx_timespan = sidx_timespans
                        .entry(sidx.reference_id)
                        .or_insert(TimeSpan::new(sidx.timescale, Duration::ZERO));

                    if sidx_timespan.timescale != sidx.timescale {
                        return unsupported_error(
                            "isomp4: different sidx timescale for the same track",
                        );
                    }

                    // Accumulate the index estimate without overflowing its own clock.
                    let duration = sidx_timespan
                        .duration
                        .unwrap_or(Duration::ZERO)
                        .checked_add(Duration::new(sidx.total_duration))
                        .ok_or(Error::DecodeError("isomp4: sidx total duration overflow"))?;
                    sidx_timespan.duration = Some(duration);
                }
                AtomType::MediaData | AtomType::MovieFragment => {
                    // The mdat atom contains the codec bitstream data. For fragmented streams, a
                    // moof + mdat pair is required. If the ftyp and moov atoms have been read, then
                    // the top-level atom scan can exit here and begin playback immediately as an
                    // optimization. If not, then the scan must continue.
                    //
                    // The scan must also exit if the source is unseekable because in that case
                    // the format reader cannot skip past these atoms without dropping packets.
                    let is_playable = moov.is_some() && ftyp.is_some();

                    if is_playable || !is_seekable {
                        if !is_playable {
                            warn!("mp4 is not streamable.");
                        }
                        break;
                    }
                }
                AtomType::Meta => {
                    // Read the metadata atom and append it to the log.
                    let mut meta = it.read_atom::<MetaAtom>()?;

                    if let Some(rev) = meta.take_metadata() {
                        metadata.push(rev);
                    }
                }
                AtomType::Free => (),
                AtomType::Skip => (),
                _ => {
                    info!("skipping top-level atom: {:?}.", header.atom_type());
                }
            }
        }

        if ftyp.is_none() {
            return unsupported_error("isomp4: missing ftyp atom");
        }

        if moov.is_none() {
            return unsupported_error("isomp4: missing moov atom");
        }

        // If the top-level atom scan iterated across the entire source (e.g., if moov was the last
        // atom), then the iterator must return to the first moof or mdat atom. This is only
        // possible if the source is seekable. If it's not, then the media will be effectively
        // unplayable.
        if is_seekable && it.pending().is_none() {
            let mut mss = it.into_inner();
            mss.seek(SeekFrom::Start(0))?;

            it = AtomIterator::new(mss, total_len);

            while let Some(header) = it.next_header()? {
                if let AtomType::MovieFragment | AtomType::MediaData = header.atom_type() {
                    break;
                }
            }
        }

        // Fragments (moof + mdat pairs) are streamed. So if the pending atom is a moof, seek the
        // iterator to the start of the moof atom.
        if let Some(atom) = it.pending() {
            if atom.atom_type() == AtomType::MovieFragment {
                it.seek_atom_start()?;
            }
        }

        let mut moov = moov.expect("moov is checked for None above");

        if moov.is_fragmented() {
            if !sidx_timespans.is_empty() {
                info!("stream is segmented with a segment index.");
            }
            else {
                info!("stream is segmented without a segment index.");
            }
        }

        let mut gapless = None;

        if let Some(rev) = moov.take_metadata() {
            // The iTunes gapless tag describes the only audio track of a file.
            let mut sound =
                moov.traks.iter().filter(|trak| trak.mdia.hdlr.handler_type == HandlerType::Sound);

            if sound.next().is_some() && sound.next().is_none() {
                gapless =
                    rev.media.tags.iter().find_map(|tag| Playable::from_itunes_gapless(&tag.raw));
            }

            metadata.push(rev);
        }

        // Create a track and track state for each Track (trak) atom.
        let mut tracks = Vec::with_capacity(moov.traks.len());
        let mut track_states = Vec::with_capacity(moov.traks.len());

        for (t, trak) in moov.traks.iter().enumerate() {
            // Sample timestamps and edits always use the media clock. SIDX only contributes
            // a non-authoritative duration estimate, rescaled before it enters that clock.
            let timescale = trak.mdia.mdhd.timescale;
            let duration = if moov.is_fragmented() {
                match sidx_timespans.get(&trak.tkhd.id) {
                    Some(index) => {
                        let duration = index.duration.expect("sidx durations are accumulated");
                        let scaled = u128::from(duration.get())
                            * u128::from(timescale.get())
                            / u128::from(index.timescale.get());
                        u64::try_from(scaled)
                            .map_err(|_| Error::DecodeError("isomp4: scaled sidx duration overflow"))?
                    }
                    None => trak.mdia.mdhd.duration,
                }
            }
            else {
                trak.mdia.minf.stbl.stts.total_duration
            };
            let timespan = if moov.is_fragmented() && duration == 0 {
                TimeSpan { timescale, duration: None }
            }
            else {
                TimeSpan::new(timescale, Duration::new(duration))
            };

            let complete_duration =
                (!moov.is_fragmented()).then_some(trak.mdia.minf.stbl.stts.total_duration);
            let (track_state, track) = TrackState::make(
                t, trak, &timespan, complete_duration, moov.mvhd.timescale, gapless,
            );

            tracks.push(track);
            track_states.push(track_state);
        }

        // The number of tracks specified in the moov atom must match the number in the mvex atom.
        if let Some(mvex) = &moov.mvex {
            if mvex.trexs.len() != moov.traks.len() {
                return decode_error("isomp4: mvex and moov track number mismatch");
            }
        }

        // The moov atom will be shared among all segments and the demuxer using an Arc.
        let moov = Arc::new(moov);

        let segs: Vec<Box<dyn StreamSegment>> = vec![Box::new(MoovSegment::new(moov.clone()))];

        // Populate media information.
        let mut media_info = MediaInfo::new();
        media_info.with_time_base(TimeBase::from_recip(moov.mvhd.timescale));
        media_info.with_duration(Duration::new(moov.mvhd.duration));

        Ok(IsoMp4Reader {
            iter: it, media_info, tracks, metadata, track_states, segs, moov,
            segments_ended: false,
        })
    }

    /// Idempotently gets information regarding the next sample of the media stream. This function
    /// selects the next sample with the lowest timestamp of all tracks.
    fn next_sample_info(&self) -> Result<NextSample> {
        let mut earliest = None;
        let mut all_spans_ended = !self.track_states.is_empty();

        // TODO: Consider returning samples based on lowest byte position in the track instead of
        // timestamp. This may be important if video tracks are ever decoded (i.e., DTS vs. PTS).

        'tracks: for (state, track) in self.track_states.iter().zip(&self.tracks) {
            if state.exhausted {
                continue;
            }
            // Get the timebase of the track used to calculate the presentation time.
            let tb = track.time_base.expect("track always created with a timebase");

            // Get the next timestamp for the next sample of the current track. The next sample may
            // be in a future segment.
            for (seg_idx_delta, seg) in self.segs[state.cur_seg..].iter().enumerate() {
                // Try to get the timestamp for the next sample of the track from the segment.
                if let Some(timing) = seg.sample_timing(state.track_num, state.next_sample)? {
                    // Keep priming and packets crossing the end, but never request the payload
                    // of a packet wholly beyond a known finite edit/gapless span.
                    if state.playable.end != u64::MAX && timing.ts >= state.playable.end {
                        continue 'tracks;
                    }
                    all_spans_ended = false;
                    // Calculate the presentation time using the timestamp.
                    let Some(ts) = timing.ts.try_into().ok()
                    else {
                        return Ok(NextSample::NeedSegment);
                    };

                    let Some(sample_time) = tb.calc_time(ts)
                    else {
                        return Ok(NextSample::NeedSegment);
                    };

                    // Compare the presentation time of the sample from this track to other tracks,
                    // and select the track with the earliest presentation time.
                    match earliest {
                        Some(NextSampleInfo { time, .. }) if time <= sample_time => {
                            // Earliest is less than or equal to the track's next sample
                            // presentation time. No need to update earliest.
                        }
                        _ => {
                            // Earliest was either None, or greater than the track's next sample
                            // presentation time. Update earliest.
                            earliest = Some(NextSampleInfo {
                                track_num: state.track_num,
                                ts,
                                time: sample_time,
                                dur: Duration::from(timing.dur),
                                seg_idx: seg_idx_delta + state.cur_seg,
                            });
                        }
                    }

                    // The next sample was found; future segments cannot change this choice.
                    continue 'tracks;
                }
            }

            // A fragment may end exactly at (or across) the playable end without containing a
            // following hidden packet. The consumed sample index and known media range suffice
            // to stop before reading another moof/mdat, without a seek-sensitive EOF flag.
            let last_seg = self.segs.last().expect("always at least one segment from moov atom");
            all_spans_ended &= state.playable.end != u64::MAX
                && state.next_sample >= last_seg.track_sample_range(state.track_num).end
                && last_seg.track_ts_range(state.track_num).end >= state.playable.end;
        }

        Ok(match earliest {
            Some(info) => NextSample::Sample(info),
            None if all_spans_ended => NextSample::EndOfSpans,
            None => NextSample::NeedSegment,
        })
    }

    fn consume_next_sample(&mut self, info: &NextSampleInfo) -> Result<Option<SampleDataInfo>> {
        // Get the track state.
        let track = &mut self.track_states[info.track_num];

        // Get the segment associated with the sample.
        let seg = &self.segs[info.seg_idx];

        // Get the sample data descriptor.
        let sample_data_desc = seg.sample_data(track.track_num, track.next_sample, false)?;

        // The sample base position in the sample data descriptor remains constant if the sample
        // followed immediately after the previous sample. In this case, the track state's
        // next_sample_pos is the position of the current sample. If the base position has jumped,
        // then the base position is the position of current the sample.
        let pos = if sample_data_desc.base_pos > track.next_sample_pos {
            sample_data_desc.base_pos
        }
        else {
            track.next_sample_pos
        };

        // Advance the track's current segment to the next sample's segment.
        track.cur_seg = info.seg_idx;

        // Advance the track's next sample number and position.
        track.next_sample += 1;
        track.next_sample_pos = pos + u64::from(sample_data_desc.size);

        Ok(Some(SampleDataInfo { pos, len: sample_data_desc.size }))
    }

    fn try_read_more_segments(&mut self) -> Result<bool> {
        if self.segments_ended {
            return Ok(false);
        }

        // If all tracks ended in the last segment, then do not try to read anymore segments.
        //
        // Note, there will always be one segment because the moov atom was converted into a segment
        // when the reader was instantiated.
        let last_seg = self.segs.last().expect("always at least one segment from moov atom");
        if last_seg.all_tracks_ended() {
            self.segments_ended = true;
            return Ok(false);
        }

        // Packet reads and cached-segment seeks are independent of the atom scan. Restore
        // its anchor before extending the segment table, including during forward seeks.
        self.iter.resume_atom_iteration()?;

        // Continue iterating over atoms until a segment (a moof + mdat atom pair) is found. All
        // other atoms will be ignored.
        loop {
            let header = match self.iter.next_header() {
                Ok(Some(header)) => header,
                Ok(None) => break,
                // If fragmented, an EOF is the only way to truly detect the end of stream.
                Err(AtomError::Other(Error::IoError(err)))
                    if self.moov.is_fragmented()
                        && err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                // Passthrough other errors.
                Err(err) => return Err(err.into()),
            };

            match header.atom_type() {
                AtomType::MediaData => {
                    return Ok(true);
                }
                AtomType::MovieFragment => {
                    let moof = self.iter.read_atom::<MoofAtom>()?;

                    // A moof segment can only be created if the media is fragmented.
                    if self.moov.is_fragmented() {
                        // Get the last segment.
                        let last_seg =
                            self.segs.last().expect("always at least one segment from moov atom");

                        // Create a new segment for the moof atom.
                        let seg = MoofSegment::new(moof, self.moov.clone(), last_seg.as_ref());

                        // Segments should have a monotonic sequence number.
                        if seg.sequence_num() <= last_seg.sequence_num() {
                            warn!("moof fragment has a non-monotonic sequence number.");
                        }

                        // Push the segment.
                        self.segs.push(Box::new(seg));
                    }
                    else {
                        return decode_error("isomp4: moof atom present without mvex atom");
                    }
                }
                _ => {
                    trace!("skipping atom: {:?}.", header.atom_type());
                }
            }
        }

        // If no atoms were returned above, then the end-of-stream has been reached.
        self.segments_ended = true;
        Ok(false)
    }

    fn seek_track_by_time(&mut self, track_num: usize, time: Time) -> Result<SeekedTo> {
        // Convert time to timestamp for the track.
        if let Some(track) = self.tracks.get(track_num) {
            let tb = track.time_base.expect("track always created with a timebase");
            let ts = tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;
            self.seek_track_by_ts(track_num, ts)
        }
        else {
            seek_error(SeekErrorKind::Unseekable)
        }
    }

    fn seek_track_by_ts(&mut self, track_num: usize, ts: Timestamp) -> Result<SeekedTo> {
        debug!("seeking track_num={track_num} to frame_ts={ts}");

        struct SeekLocation {
            seg_idx: usize,
            sample_num: u32,
        }

        // Can only seek to 0 or positive media timestamps.
        let playable = self.track_states[track_num].playable;
        let media_ts = playable.media_ts(ts);

        if media_ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let mut seg_skip = 0;

        let seek_loc = 'locate: loop {
            // Iterate over all segments and attempt to find the segment and sample number that
            // contains the desired timestamp. Skip segments already examined.
            for (seg_idx, seg) in self.segs.iter().enumerate().skip(seg_skip) {
                if let Some(sample_num) = seg.ts_sample(track_num, media_ts.get() as u64)? {
                    break 'locate SeekLocation { seg_idx, sample_num };
                }

                // Mark the segment as examined.
                seg_skip = seg_idx + 1;
            }

            // Otherwise, try to read more segments from the stream.
            if !self.try_read_more_segments()? {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        };

        let seg = &self.segs[seek_loc.seg_idx];

        // Get the sample timing.
        let timing = match seg.sample_timing(track_num, seek_loc.sample_num)? {
            Some(t) => t,
            None => return seek_error(SeekErrorKind::OutOfRange),
        };

        // Try to convert the sample timing to a timestamp.
        let actual_ts = match Timestamp::try_from(timing.ts) {
            Ok(ts) => playable.presented_ts(ts),
            _ => return seek_error(SeekErrorKind::OutOfRange),
        };

        // Get the sample information.
        let data_desc = seg.sample_data(track_num, seek_loc.sample_num, true)?;

        // Update the track's next sample information to point to the seeked sample.
        let track = &mut self.track_states[track_num];

        track.cur_seg = seek_loc.seg_idx;
        track.next_sample = seek_loc.sample_num;
        track.exhausted = false;
        track.next_sample_pos = data_desc.base_pos
            + match data_desc.offset {
                Some(o) => o,
                None => return seek_error(SeekErrorKind::OutOfRange),
            };

        debug!(
            "seeked track_num={} (track_id={}) to packet_ts={} (delta={})",
            track_num,
            track.track_id,
            actual_ts,
            actual_ts.saturating_delta(ts),
        );

        Ok(SeekedTo { track_id: track.track_id, required_ts: ts, actual_ts })
    }
    /// Only finite edits and fully discovered sample tables can authoritatively end a track.
    fn auxiliary_target_is_past_end(&self, track_num: usize, ts: Timestamp) -> bool {
        let state = &self.track_states[track_num];
        let media_ts = state.playable.media_ts(ts);
        if media_ts.is_negative() {
            return false;
        }
        let media_ts = media_ts.get() as u64;
        if state.playable.end != u64::MAX && media_ts >= state.playable.end {
            return true;
        }
        let last = self.segs.last().expect("always at least one segment from moov atom");
        (self.segments_ended || last.all_tracks_ended())
            && media_ts >= last.track_ts_range(track_num).end
    }

    fn exhaust_track(&mut self, track_num: usize) {
        let last = self.segs.last().expect("always at least one segment from moov atom");
        let state = &mut self.track_states[track_num];
        state.cur_seg = self.segs.len() - 1;
        state.next_sample = last.track_sample_range(track_num).end;
        state.next_sample_pos = 0;
        state.exhausted = true;
    }

    fn seek_auxiliary_tracks(&mut self, selected: usize, time: Time) -> Result<()> {
        for index in 0..self.tracks.len() {
            if index == selected {
                continue;
            }
            let track = &self.tracks[index];
            let base = track.time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
            let ts = base.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;
            // Another track need not have the selected track's encoder lead-in.
            let ts = ts.max(track.start_ts);
            if self.auxiliary_target_is_past_end(index, ts) {
                self.exhaust_track(index);
                continue;
            }
            match self.seek_track_by_ts(index, ts) {
                Ok(_) => (),
                Err(Error::SeekError(SeekErrorKind::OutOfRange))
                    if self.auxiliary_target_is_past_end(index, ts) =>
                {
                    // The forward scan discovered physical EOF. Explicitly discard the old
                    // cursor rather than returning stale auxiliary packets after a valid seek.
                    self.exhaust_track(index);
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

}

impl Scoreable for IsoMp4Reader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl ProbeableFormat<'_> for IsoMp4Reader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(IsoMp4Reader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            ISOMP4_FORMAT_INFO,
            &["mp4", "m4a", "m4p", "m4b", "m4r", "m4v", "mov"],
            &["video/mp4", "audio/m4a"],
            &[b"ftyp"] // Top-level atoms
        )]
    }
}

impl FormatReader for IsoMp4Reader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &ISOMP4_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        // Get the index of the track with the next-nearest (minimum) timestamp.
        let next_sample_info = loop {
            // Using the current set of segments, try to get the next sample info.
            match self.next_sample_info()? {
                NextSample::Sample(info) => break info,
                NextSample::EndOfSpans => return Ok(None),
                NextSample::NeedSegment => (),
            }

            // No more segments. If the stream is unseekable, it may be the case that there are
            // more segments coming. If the stream is seekable it might be fragmented and no
            // segments are found in the moov atom. Iterate atoms until a new segment is found
            // or the end-of-stream is reached
            if !self.try_read_more_segments()? {
                return Ok(None);
            }
        };

        // Get the position and length information of the next sample.
        let sample_info = match self.consume_next_sample(&next_sample_info)? {
            Some(s) => s,
            None => return decode_error("isomp4: failed to get next sample data"),
        };

        let data =
            self.iter.read_raw_boxed_slice_exact(sample_info.pos, sample_info.len as usize)?;

        self.track_states[next_sample_info.track_num]
            .packet(next_sample_info.ts, next_sample_info.dur, data)
            .map(Some)
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if self.tracks.is_empty() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        match to {
            SeekTo::Timestamp { ts, track_id } => {
                // The seek timestamp is in timebase units specific to the selected track. Get the
                // selected track and use the timebase to convert the timestamp into time units so
                // that the other tracks can be seeked.
                if let Some((track_num, track)) =
                    self.tracks.iter().enumerate().find(|(_, track)| track.id == track_id)
                {
                    // Convert to time units.
                    let tb = match track.time_base {
                        Some(tb) => tb,
                        None => return seek_error(SeekErrorKind::Unseekable),
                    };
                    let time =
                        tb.calc_time(ts).ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;

                    self.seek_auxiliary_tracks(track_num, time)?;

                    // Seek the primary track and return the result.
                    self.seek_track_by_ts(track_num, ts)
                }
                else {
                    seek_error(SeekErrorKind::InvalidTrack)
                }
            }
            SeekTo::Time { time, track_id } => {
                // If provided, find the track number of the track with the desired track_id, or
                // default to the first track.
                let track_num = match track_id {
                    Some(id) => self
                        .tracks
                        .iter()
                        .position(|track| track.id == id)
                        .ok_or(Error::SeekError(SeekErrorKind::InvalidTrack))?,
                    None => 0,
                };

                self.seek_auxiliary_tracks(track_num, time)?;

                // Seek the primary track and return the result.
                self.seek_track_by_time(track_num, time)
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

impl ReadAtom for MediaSourceStream<'_> {}

impl From<AtomError> for Error {
    fn from(value: AtomError) -> Self {
        // Map all atom iteration errors to decode errors.
        let msg = match value {
            AtomError::InvalidAtomSize => "isomp4: invalid atom size",
            AtomError::InvalidUtf8 => "isomp4: invalid utf-8 string",
            AtomError::MaximumDepthReached => "isomp4: maximum recursion depth reached",
            AtomError::NoParentAtom => "isomp4: no parent atom",
            AtomError::NoPendingAtom => "isomp4: no atom pending read",
            AtomError::Overrun => "isomp4: overrun while reading atom",
            AtomError::SeekOutOfRange => "isomp4: out-of-bounds seek for a non-seekable stream",
            AtomError::UnexpectedEndOfAtom => "isomp4: unexpected end of atom",
            AtomError::UnexpectedPosition => "isomp4: unexpected position",
            AtomError::UnexpectedUnknownSizeAtom => "isomp4: unknown size atom has sized parent",
            AtomError::UnexpectedReadOperation => "isomp4: unexpected read operation",
            AtomError::UnknownAtomSize => "isomp4: unknown atom size",
            AtomError::Other(err) => return err,
        };
        Error::DecodeError(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: NonZero<u32> = NonZero::new(44_100).unwrap();

    fn trims(playable: Playable, ts: i64, dur: u64) -> (u64, u64) {
        let (start, end) = playable.trims(Timestamp::new(ts), Duration::new(dur));
        (start.get(), end.get())
    }

    #[test]
    fn samples_are_trimmed_to_the_presented_span() {
        let playable = Playable { start: 1024, end: 14_254 };

        // Entirely before, across the start, inside, across the end, and entirely after.
        assert_eq!(trims(playable, 0, 1024), (1024, 0));
        assert_eq!(trims(playable, 512, 1024), (512, 0));
        assert_eq!(trims(playable, 1024, 1024), (0, 0));
        assert_eq!(trims(playable, 13_312, 1024), (0, 82));
        assert_eq!(trims(playable, 14_336, 1024), (0, 1024));

        // A sample longer than the span is trimmed on both sides.
        assert_eq!(trims(Playable { start: 100, end: 300 }, 0, 1024), (100, 724));
        assert_eq!(trims(Playable::ALL, 0, 1024), (0, 0));
    }

    #[test]
    fn timestamps_and_duration_are_relative_to_the_presented_span() {
        let playable = Playable { start: 1024, end: 14_254 };

        assert_eq!(playable.presented_ts(Timestamp::new(0)), Timestamp::new(-1024));
        assert_eq!(playable.media_ts(Timestamp::new(-1024)), Timestamp::new(0));
        assert_eq!(playable.media_ts(Timestamp::new(4410)), Timestamp::new(5434));

        assert_eq!(playable.duration(14_336), 13_230);
        assert_eq!(playable.duration(8000), 6976);
        assert_eq!(playable.duration(512), 0);
        assert_eq!(Playable::ALL.duration(14_336), 14_336);
    }

    #[test]
    fn edit_durations_are_scaled_from_the_movie_timescale() {
        let movie = NonZero::new(1000).unwrap();

        assert_eq!(
            Playable::from_edit((1024, 13_230), RATE, RATE),
            Playable { start: 1024, end: 14_254 }
        );
        assert_eq!(
            Playable::from_edit((1024, 300), movie, RATE),
            Playable { start: 1024, end: 14_254 }
        );
        // A zero duration, and one beyond the representable range, extend to the end.
        assert_eq!(Playable::from_edit((1024, 0), movie, RATE).end, u64::MAX);
        assert_eq!(Playable::from_edit((1024, u64::MAX), movie, RATE).end, u64::MAX);
    }

    #[test]
    fn itunes_gapless_tag_declares_delay_and_frame_count() {
        let tag = |key: &str, value: &str| RawTag::new(key, value);
        let key = "com.apple.iTunes:iTunSMPB";

        assert_eq!(
            Playable::from_itunes_gapless(&tag(
                key,
                " 00000000 00000400 00000052 00000000000033AE 00000000"
            )),
            Some(Playable { start: 1024, end: 14_254 })
        );
        assert_eq!(Playable::from_itunes_gapless(&tag(key, " 00000000 00000400 00000052")), None);
        assert_eq!(
            Playable::from_itunes_gapless(&tag(key, " 00000000 0000040g 00000052 000033AE")),
            None
        );
        assert_eq!(
            Playable::from_itunes_gapless(&tag(
                "com.apple.iTunes:iTunNORM",
                " 00000000 00000400 00000052 000033AE"
            )),
            None
        );
        assert_eq!(Playable::from_itunes_gapless(&RawTag::new(key, 1024_u32)), None);
    }
}

#[cfg(test)]
mod tail_cutoff_tests {
    use super::*;
    use std::ops::Range;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fixture_reader() -> IsoMp4Reader<'static> {
        let bytes = include_bytes!("../../../crates/rivu-audio/tests/fixtures/aac-smpb-two.m4a");
        let stream = MediaSourceStream::new(
            Box::new(std::io::Cursor::new(bytes.as_slice())),
            MediaSourceStreamOptions::default(),
        );
        let mut reader = IsoMp4Reader::try_new(stream, FormatOptions::default()).unwrap();
        for state in &mut reader.track_states {
            state.playable = Playable::ALL;
        }
        reader
    }

    fn drain(reader: &mut IsoMp4Reader<'_>) -> Vec<Packet> {
        let mut packets = Vec::new();
        while let Some(packet) = reader.next_packet().unwrap() {
            packets.push(packet);
        }
        packets
    }

    #[test]
    fn finite_span_skips_only_hidden_tail_payload_and_keeps_other_tracks() {
        let mut baseline = fixture_reader();
        let complete = drain(&mut baseline);
        let id = baseline.tracks[0].id;
        let first_track: Vec<_> = complete.iter().filter(|packet| packet.track_id == id).collect();
        assert!(first_track.len() > 4);
        let start = first_track[1].pts.get() as u64;
        let boundary = first_track[3];
        let end = boundary.pts.get() as u64 + boundary.dur.get() / 2;
        let mut reader = fixture_reader();
        reader.track_states[0].playable = Playable { start, end };
        let presented = drain(&mut reader);

        let expected: Vec<_> = complete
            .iter()
            .filter(|packet| packet.track_id != id || (packet.pts.get() as u64) < end)
            .collect();
        assert_eq!(presented.len(), expected.len());
        for (actual, original) in presented.iter().zip(&expected) {
            assert_eq!(actual.track_id, original.track_id);
            assert_eq!(actual.data, original.data);
        }
        let priming = presented.iter().find(|packet| packet.track_id == id).unwrap();
        assert_eq!(priming.pts, Timestamp::new(-(start as i64)));
        assert_eq!(priming.trim_start, first_track[0].dur);
        let last = presented.iter().rev().find(|packet| packet.track_id == id).unwrap();
        assert_eq!(last.data, boundary.data);
        assert_eq!(last.trim_end.get(), boundary.dur.get() - boundary.dur.get() / 2);

        // Exact encoded packet/byte savings, not presented PCM duration or an I/O buffer count.
        let hidden: Vec<_> = complete
            .iter()
            .filter(|packet| packet.track_id == id && (packet.pts.get() as u64) >= end)
            .collect();
        assert!(!hidden.is_empty());
        assert_eq!(complete.len() - presented.len(), hidden.len());
        let bytes = |packets: &[Packet]| packets.iter().map(|packet| packet.data.len()).sum::<usize>();
        assert_eq!(
            bytes(&complete) - bytes(&presented),
            hidden.iter().map(|packet| packet.data.len()).sum::<usize>()
        );
    }

    #[test]
    fn unbounded_span_ignores_estimated_track_duration() {
        let expected = drain(&mut fixture_reader());
        let mut reader = fixture_reader();
        for track in &mut reader.tracks {
            track.with_duration(Duration::new(1));
        }
        let packets = drain(&mut reader);
        assert_eq!(packets.len(), expected.len());
        for (actual, expected) in packets.iter().zip(&expected) {
            assert_eq!(actual.track_id, expected.track_id);
            assert_eq!(actual.data, expected.data);
        }
    }

    #[test]
    fn span_eof_allows_backward_seek_and_failed_seek_keeps_cutoff() {
        let mut reader = fixture_reader();
        for (state, track) in reader.track_states.iter_mut().zip(&mut reader.tracks) {
            state.playable = Playable { start: 1024, end: 3584 };
            track.with_start_ts(Timestamp::new(-1024));
        }
        let initial = drain(&mut reader);
        let id = reader.tracks[0].id;
        assert!(reader.seek_track_by_ts(0, Timestamp::new(i64::MAX)).is_err());
        assert!(reader.next_packet().unwrap().is_none());
        reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(-1024), track_id: id },
            )
            .unwrap();
        let replayed = drain(&mut reader);
        assert_eq!(initial.len(), replayed.len());
        for (initial, replayed) in initial.iter().zip(&replayed) {
            assert_eq!(initial.track_id, replayed.track_id);
            assert_eq!(initial.pts, replayed.pts);
            assert_eq!(initial.data, replayed.data);
            assert_eq!(initial.trim_start, replayed.trim_start);
            assert_eq!(initial.trim_end, replayed.trim_end);
        }
    }

    // A loaded two-track fragment with two 1024-unit samples per track. Like MoofSegment,
    // it cannot declare physical EOF; the following atom must not be inspected at span EOF.
    struct OpenFragment {
        payload_requests: Arc<AtomicUsize>,
    }

    impl StreamSegment for OpenFragment {
        fn sequence_num(&self) -> u32 { 1 }
        fn all_tracks_ended(&self) -> bool { false }
        fn track_sample_range(&self, _track: usize) -> Range<u32> { 0..2 }
        fn track_ts_range(&self, _track: usize) -> Range<u64> { 0..2048 }
        fn sample_timing(&self, _track: usize, sample: u32) -> Result<Option<SampleTiming>> {
            Ok((sample < 2).then_some(SampleTiming { ts: u64::from(sample) * 1024, dur: 1024 }))
        }
        fn ts_sample(&self, _track: usize, ts: u64) -> Result<Option<u32>> {
            Ok((ts < 2048).then_some((ts / 1024) as u32))
        }
        fn sample_data(&self, track: usize, sample: u32, _offset: bool) -> Result<SampleDataDesc> {
            self.payload_requests.fetch_add(1, Ordering::Relaxed);
            Ok(SampleDataDesc {
                base_pos: 8 + track as u64 * 8 + u64::from(sample) * 4,
                offset: Some(0),
                size: 4,
            })
        }
    }

    #[test]
    fn all_fragment_spans_end_without_parsing_following_moof() {
        for end in [1024, 1536, 2048] {
            let mut reader = fixture_reader();
            let payload_requests = Arc::new(AtomicUsize::new(0));
            reader.segs = vec![Box::new(OpenFragment { payload_requests: payload_requests.clone() })];
            for state in &mut reader.track_states {
                state.playable = Playable { start: 0, end };
            }
            // Valid mdat followed by an incomplete moof: parsing past the declared spans fails.
            let mut bytes = vec![0, 0, 0, 24, b'm', b'd', b'a', b't'];
            bytes.extend(0_u8..16);
            bytes.extend_from_slice(&[0, 0, 0, 8, b'm', b'o', b'o', b'f']);
            let len = bytes.len() as u64;
            let stream = MediaSourceStream::new(
                Box::new(std::io::Cursor::new(bytes)),
                MediaSourceStreamOptions::default(),
            );
            reader.iter = AtomIterator::new(stream, Some(len));
            let header = reader.iter.next_header().map_err(Error::from).unwrap().unwrap();
            assert_eq!(header.atom_type(), AtomType::MediaData);
            let packets = drain(&mut reader);
            let count = if end == 1024 { 2 } else { 4 };
            assert_eq!(packets.len(), count);
            assert_eq!(payload_requests.load(Ordering::Relaxed), count);
            assert_eq!(packets.iter().map(|packet| packet.data.len()).sum::<usize>(), count * 4);
            assert!(reader.next_packet().unwrap().is_none());
            assert_eq!(reader.segs.len(), 1);
        }
    }
}

#[cfg(test)]
mod opus_tests {
    #[test]
    fn auxiliary_track_without_lead_in_does_not_block_negative_seek() {
        for by_timestamp in [false, true] {
            let bytes = include_bytes!("../../../crates/rivu-audio/tests/fixtures/aac-smpb-two.m4a");
            let stream = MediaSourceStream::new(
                Box::new(std::io::Cursor::new(bytes.as_slice())),
                MediaSourceStreamOptions::default(),
            );
            let mut reader = IsoMp4Reader::try_new(stream, FormatOptions::default()).unwrap();
            // Model a 1024-frame edit on the first track; the second begins at zero.
            reader.track_states[0].playable = Playable { start: 1024, end: u64::MAX };
            reader.tracks[0].with_start_ts(Timestamp::new(-1024));
            let id = reader.tracks[0].id;
            let target = if by_timestamp {
                SeekTo::Timestamp { ts: Timestamp::new(-1024), track_id: id }
            } else {
                SeekTo::Time { time: Time::try_from_secs_f64(-0.01).unwrap(), track_id: Some(id) }
            };
            let result = reader.seek(SeekMode::Accurate, target).unwrap();
            assert_eq!(result.actual_ts, Timestamp::new(-1024));
            let packet = reader.next_packet().unwrap().unwrap();
            assert_eq!(packet.track_id, id);
            assert_eq!(packet.pts, Timestamp::new(-1024));
        }
    }

    use super::*;

    const RATE: NonZero<u32> = NonZero::new(48_000).unwrap();

    fn fixture_movie(pre_skip: u16) -> MoovAtom {
        let mut bytes = include_bytes!("../../../crates/rivu-audio/tests/fixtures/opus-edit.mp4").to_vec();
        let dops = bytes.windows(4).position(|bytes| bytes == b"dOps").unwrap();
        bytes[dops + 6..dops + 8].copy_from_slice(&pre_skip.to_be_bytes());
        let len = bytes.len() as u64;
        let stream = MediaSourceStream::new(
            Box::new(std::io::Cursor::new(bytes)),
            MediaSourceStreamOptions::default(),
        );
        let mut atoms = AtomIterator::new(stream, Some(len));
        loop {
            let header = match atoms.next_header() {
                Ok(Some(header)) => header,
                Ok(None) => break,
                Err(_) => panic!("failed to read fixture atom header"),
            };
            if header.atom_type() == AtomType::Movie {
                return match atoms.read_atom::<MoovAtom>() {
                    Ok(movie) => movie,
                    Err(_) => panic!("failed to read fixture movie"),
                };
            }
        }
        panic!("fixture has no movie");
    }

    #[test]
    fn dops_preskip_is_fallback_not_an_additional_delay() {
        // A header delay different from the edit must neither replace nor add to it.
        let mut movie = fixture_movie(1200);
        let trak = &mut movie.traks[0];
        let timespan = TimeSpan::new(RATE, Duration::new(9600));
        let (edited, track) =
            TrackState::make(0, trak, &timespan, Some(9600), movie.mvhd.timescale, None);
        assert_eq!(edited.playable.start, 312);
        assert_eq!(track.start_ts, Timestamp::new(-312));

        trak.edts = None;
        let (state, track) =
            TrackState::make(0, trak, &timespan, Some(9600), movie.mvhd.timescale, None);
        assert_eq!(state.playable.start, 1200);
        assert_eq!(track.start_ts, Timestamp::new(-1200));
        assert_eq!(track.duration, Some(Duration::new(8400)));
        assert_eq!(track.num_frames, Some(8400));
        let packet = state.packet(Timestamp::ZERO, Duration::new(960), vec![0xf8].into()).unwrap();
        assert_eq!(packet.pts, track.start_ts);
        assert_eq!(packet.trim_start, Duration::new(960));
        assert_eq!(packet.dur, Duration::ZERO);

        let gapless = Playable { start: 900, end: 9000 };
        let (state, track) =
            TrackState::make(0, trak, &timespan, Some(9600), movie.mvhd.timescale, Some(gapless));
        assert_eq!(state.playable, gapless);
        assert_eq!(track.start_ts, Timestamp::new(-900));
        assert_eq!(track.duration, Some(Duration::new(8100)));
    }

    #[test]
    fn shortened_sample_and_edit_end_trim_decoded_opus_padding_once() {
        let mut movie = fixture_movie(312);
        let trak = &mut movie.traks[0];
        trak.edts = None;
        let timespan = TimeSpan::new(RATE, Duration::new(9912));
        let (mut state, _) =
            TrackState::make(0, trak, &timespan, Some(9912), movie.mvhd.timescale, None);
        // The final TOC declares 960 decoded frames, but stts presents only 312.
        let packet = state.packet(Timestamp::new(9600), Duration::new(312), vec![0xf8].into()).unwrap();
        assert_eq!(packet.pts, Timestamp::new(9288));
        assert_eq!(packet.dur, Duration::new(312));
        assert_eq!(packet.trim_start, Duration::ZERO);
        assert_eq!(packet.trim_end, Duration::new(648));

        // An edit declaring the same endpoint does not double-count the padding.
        state.playable.end = 9912;
        let packet = state.packet(Timestamp::new(9600), Duration::new(312), vec![0xf8].into()).unwrap();
        assert_eq!(packet.trim_end, Duration::new(648));
        // An earlier edit end is authoritative over the shortened sample duration.
        state.playable.end = 9800;
        let packet = state.packet(Timestamp::new(9600), Duration::new(312), vec![0xf8].into()).unwrap();
        assert_eq!(packet.dur, Duration::new(200));
        assert_eq!(packet.trim_end, Duration::new(760));
        // Leading and trailing trims can occur in the same packet.
        state.playable = Playable { start: 312, end: 700 };
        let packet = state.packet(Timestamp::ZERO, Duration::new(700), vec![0xf8].into()).unwrap();
        assert_eq!(packet.trim_start, Duration::new(312));
        assert_eq!(packet.trim_end, Duration::new(260));
        assert_eq!(packet.dur, Duration::new(388));
    }

    #[test]
    fn opus_toc_durations_cover_frame_modes_and_full_six_bit_count() {
        for (toc, frames) in [(0, 480), (24, 2880), (96, 480), (104, 960), (128, 120), (248, 960), (249, 1920), (250, 1920)] {
            assert_eq!(opus_packet_frames(&[toc]).unwrap(), frames);
        }
        assert_eq!(opus_packet_frames(&[131, 48]).unwrap(), 5760);
        assert_eq!(opus_packet_frames(&[131, 0x80 | 48]).unwrap(), 5760);
        assert!(opus_packet_frames(&[]).is_err());
        assert!(opus_packet_frames(&[131]).is_err());
        assert!(opus_packet_frames(&[131, 0]).is_err());
        assert!(opus_packet_frames(&[131, 49]).is_err());
        assert_eq!(scale_opus_frames(1200, NonZero::new(24_000).unwrap()), 600);
    }
}

#[cfg(test)]
mod fragmented_boundary_tests {
    use super::*;

    struct TestTrack<'a> {
        rate: u32,
        sample_duration: u32,
        samples: &'a [u32],
        edit: Option<(u32, u32)>,
        declared_duration: u32,
    }

    fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn words(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|value| value.to_be_bytes()).collect()
    }

    fn children(mut bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
        std::iter::from_fn(move || {
            if bytes.is_empty() {
                return None;
            }
            let len = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
            let (child, rest) = bytes.split_at(len);
            bytes = rest;
            Some(child)
        })
    }

    fn child<'a>(parent: &'a [u8], kind: &[u8; 4]) -> &'a [u8] {
        children(&parent[8..]).find(|child| &child[4..8] == kind).unwrap()
    }

    // Reuse genuine AAC sample descriptions, but build real empty initialization tables and
    // moof/trun/mdat pairs. No replacement StreamSegment or manually installed reader state.
    fn reader(tracks: &[TestTrack<'_>], index: Option<(u32, u32)>) -> IsoMp4Reader<'static> {
        let source = include_bytes!("../../../crates/rivu-audio/tests/fixtures/aac-smpb-two.m4a");
        let original = children(source).find(|item| &item[4..8] == b"moov").unwrap();
        let mut mvhd = child(original, b"mvhd").to_vec();
        assert_eq!(mvhd[8], 0);
        mvhd[20..24].copy_from_slice(&1000_u32.to_be_bytes());
        mvhd[24..28].fill(0);
        let originals: Vec<_> = children(&original[8..])
            .filter(|item| &item[4..8] == b"trak")
            .collect();
        let mut movie = mvhd;
        let mut extends = Vec::new();
        let mut ids = Vec::new();
        for (spec, original) in tracks.iter().zip(&originals) {
            let tkhd = child(original, b"tkhd");
            assert_eq!(tkhd[8], 0);
            let id = u32::from_be_bytes(tkhd[20..24].try_into().unwrap());
            ids.push(id);
            let mdia = child(original, b"mdia");
            let mut mdhd = child(mdia, b"mdhd").to_vec();
            assert_eq!(mdhd[8], 0);
            mdhd[20..24].copy_from_slice(&spec.rate.to_be_bytes());
            mdhd[24..28].copy_from_slice(&spec.declared_duration.to_be_bytes());
            let minf = child(mdia, b"minf");
            let mut table = child(child(minf, b"stbl"), b"stsd").to_vec();
            table.extend(atom(b"stts", &words(&[0, 0])));
            table.extend(atom(b"stsc", &words(&[0, 0])));
            table.extend(atom(b"stsz", &words(&[0, 0, 0])));
            table.extend(atom(b"stco", &words(&[0, 0])));
            let mut media_info: Vec<u8> = children(&minf[8..])
                .filter(|item| &item[4..8] != b"stbl")
                .flatten().copied().collect();
            media_info.extend(atom(b"stbl", &table));
            mdhd.extend_from_slice(child(mdia, b"hdlr"));
            mdhd.extend(atom(b"minf", &media_info));
            let mut track = tkhd.to_vec();
            if let Some((start, duration)) = spec.edit {
                track.extend(atom(b"edts", &atom(b"elst", &words(&[
                    0, 1, duration, start, 0x0001_0000,
                ]))));
            }
            track.extend(atom(b"mdia", &mdhd));
            movie.extend(atom(b"trak", &track));
            extends.extend(atom(b"trex", &words(&[0, id, 1, spec.sample_duration, 3, 0])));
        }
        assert_eq!(ids.len(), tracks.len());
        movie.extend(atom(b"mvex", &extends));
        let mut bytes = children(source).find(|item| &item[4..8] == b"ftyp").unwrap().to_vec();
        bytes.extend(atom(b"moov", &movie));
        if let Some((scale, duration)) = index {
            bytes.extend(atom(b"sidx", &words(&[0, ids[0], scale, 0, 0, 1, 0, duration, 0])));
        }
        let fragments = tracks.iter().map(|track| track.samples.len()).max().unwrap();
        for fragment in 0..fragments {
            let make_moof = |mut offset: u32| {
                let mut body = atom(b"mfhd", &words(&[0, fragment as u32 + 1]));
                for (spec, id) in tracks.iter().zip(&ids) {
                    let count = spec.samples.get(fragment).copied().unwrap_or(0);
                    let mut traf = atom(b"tfhd", &words(&[0x0002_0000, *id]));
                    traf.extend(atom(b"trun", &words(&[1, count, offset])));
                    body.extend(atom(b"traf", &traf));
                    offset += count * 3;
                }
                atom(b"moof", &body)
            };
            let moof_len = make_moof(0).len() as u32;
            bytes.extend(make_moof(moof_len + 8));
            let mut payload = Vec::new();
            for (spec, id) in tracks.iter().zip(&ids) {
                for sample in 0..spec.samples.get(fragment).copied().unwrap_or(0) {
                    payload.extend_from_slice(&[*id as u8, fragment as u8, sample as u8]);
                }
            }
            bytes.extend(atom(b"mdat", &payload));
        }
        let stream = MediaSourceStream::new(
            Box::new(std::io::Cursor::new(bytes)),
            MediaSourceStreamOptions::default(),
        );
        IsoMp4Reader::try_new(stream, FormatOptions::default()).unwrap()
    }

    fn track<'a>(samples: &'a [u32], edit: Option<(u32, u32)>) -> TestTrack<'a> {
        TestTrack { rate: 1000, sample_duration: 1000, samples, edit, declared_duration: 0 }
    }

    fn drain(reader: &mut IsoMp4Reader<'_>) -> Vec<Packet> {
        let mut packets = Vec::new();
        while let Some(packet) = reader.next_packet().unwrap() {
            packets.push(packet);
        }
        packets
    }

    fn seek(reader: &mut IsoMp4Reader<'_>, ts: i64, by_time: bool) -> Result<SeekedTo> {
        let track_id = reader.tracks()[0].id;
        let to = if by_time {
            SeekTo::Time { time: Time::try_from_secs_f64(ts as f64 / 1000.0).unwrap(),
                track_id: Some(track_id) }
        }
        else {
            SeekTo::Timestamp { ts: Timestamp::new(ts), track_id }
        };
        reader.seek(SeekMode::Accurate, to)
    }

    fn assert_replay(actual: &[Packet], expected: &[Packet]) {
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.track_id, expected.track_id);
            assert_eq!(actual.pts, expected.pts);
            assert_eq!(actual.dur, expected.dur);
            assert_eq!(actual.trim_start, expected.trim_start);
            assert_eq!(actual.trim_end, expected.trim_end);
            assert_eq!(actual.data, expected.data);
        }
    }

    #[test]
    fn differing_sidx_clock_does_not_change_media_timestamps_or_edit_end() {
        let mut reader = reader(&[TestTrack {
            rate: 48_000, sample_duration: 960, samples: &[26, 26],
            edit: Some((312, 1000)), declared_duration: 0,
        }], Some((1000, 1000)));
        assert_eq!(reader.tracks()[0].time_base, Some(TimeBase::from_recip(NonZero::new(48_000).unwrap())));
        assert_eq!(reader.tracks()[0].start_ts, Timestamp::new(-312));
        assert_eq!(reader.tracks()[0].duration, Some(Duration::new(48_000)));
        let packets = drain(&mut reader);
        assert_eq!(packets.len(), 51);
        assert_eq!(packets[0].pts, Timestamp::new(-312));
        assert_eq!(packets[0].trim_start, Duration::new(312));
        let last = packets.last().unwrap();
        assert_eq!(last.pts, Timestamp::new(47_688));
        assert_eq!(last.dur, Duration::new(312));
        assert_eq!(last.trim_end, Duration::new(648));
        assert_eq!(last.pts.get() + last.dur.get() as i64, 48_000);
    }

    #[test]
    fn finite_zero_origin_edit_survives_unknown_fragmented_duration() {
        let mut reader = reader(&[TestTrack {
            rate: 48_000, sample_duration: 960, samples: &[2, 1],
            edit: Some((0, 25)), declared_duration: 0,
        }], None);
        assert_eq!(reader.tracks()[0].duration, Some(Duration::new(1200)));
        let packets = drain(&mut reader);
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].pts, Timestamp::ZERO);
        assert_eq!(packets[1].pts, Timestamp::new(960));
        assert_eq!(packets[1].dur, Duration::new(240));
        assert_eq!(packets[1].trim_end, Duration::new(720));
        assert!(reader.next_packet().unwrap().is_none());
    }

    #[test]
    fn finite_zero_origin_edit_is_authoritative_over_fragmented_duration_estimates() {
        // Both mdhd and SIDX estimates can be shorter than (or equal to) the edit. Neither
        // proves an all-samples edit or justifies clipping its declared presented duration.
        for index in [None, Some((1000, 20)), Some((1000, 30))] {
            let mut reader = reader(&[TestTrack {
                rate: 48_000, sample_duration: 960, samples: &[2, 1],
                edit: Some((0, 30)), declared_duration: 960,
            }], index);
            assert_eq!(reader.tracks()[0].duration, Some(Duration::new(1440)));
            let packets = drain(&mut reader);
            assert_eq!(packets.len(), 2);
            assert_eq!(packets[0].pts, Timestamp::ZERO);
            assert_eq!(packets[0].dur, Duration::new(960));
            assert_eq!(packets[1].pts, Timestamp::new(960));
            assert_eq!(packets[1].dur, Duration::new(480));
            assert_eq!(packets[1].trim_end, Duration::new(480));
            assert_eq!(packets[1].pts.get() + packets[1].dur.get() as i64, 1440);
            assert!(reader.next_packet().unwrap().is_none());
        }
    }

    #[test]
    fn auxiliary_shortened_edit_exhausts_and_backward_seek_reactivates() {
        for by_time in [false, true] {
            let mut reader = reader(&[
                track(&[50, 50], None), track(&[50, 50], Some((10_000, 90_000))),
            ], None);
            seek(&mut reader, 0, by_time).unwrap();
            let expected = drain(&mut reader);
            seek(&mut reader, 0, by_time).unwrap();
            reader.next_packet().unwrap().unwrap();
            let result = seek(&mut reader, 95_000, by_time).unwrap();
            assert_eq!(result.actual_ts, Timestamp::new(95_000));
            let tail = drain(&mut reader);
            assert_eq!(tail.len(), 5);
            assert!(tail.iter().all(|packet| packet.track_id == result.track_id));
            seek(&mut reader, 0, by_time).unwrap();
            assert_replay(&drain(&mut reader), &expected);
        }
    }

    #[test]
    fn auxiliary_physical_end_exhausts_without_failing_selected_seek() {
        for by_time in [false, true] {
            let mut reader = reader(&[track(&[50, 50], None), track(&[10, 10], None)], None);
            // This seek must discover physical EOF rather than rely on a declared duration.
            let result = seek(&mut reader, 95_000, by_time).unwrap();
            let tail = drain(&mut reader);
            assert_eq!(tail.len(), 5);
            assert!(tail.iter().all(|packet| packet.track_id == result.track_id));
            seek(&mut reader, 0, by_time).unwrap();
            let replayed = drain(&mut reader);
            assert_eq!(replayed.len(), 120);
            assert!(replayed.iter().any(|packet| packet.track_id != result.track_id));
        }
    }

    #[test]
    fn auxiliary_duration_estimate_is_not_physical_eof() {
        let mut auxiliary = track(&[50, 50], None);
        auxiliary.declared_duration = 1000;
        let mut reader = reader(&[track(&[50, 50], None), auxiliary], None);
        seek(&mut reader, 95_000, false).unwrap();
        let tail = drain(&mut reader);
        assert_eq!(tail.len(), 10);
        assert_eq!(tail.iter().filter(|packet| packet.track_id == reader.tracks()[0].id).count(), 5);
    }

    #[test]
    fn fragmented_physical_eof_then_backward_seek_replays_identical_packets() {
        for by_time in [false, true] {
            let mut reader = reader(&[track(&[3, 2], None), track(&[2, 1], None)], None);
            let expected = drain(&mut reader);
            assert_eq!(expected.len(), 8);
            assert!(reader.next_packet().unwrap().is_none());
            seek(&mut reader, 0, by_time).unwrap();
            assert_replay(&drain(&mut reader), &expected);
        }
    }

    #[test]
    fn failed_forward_fragment_scan_then_backward_seek_replays_identical_packets() {
        for by_time in [false, true] {
            let specs = [track(&[3, 2], None), track(&[2, 1], None)];
            let expected = drain(&mut reader(&specs, None));
            let mut reader = reader(&specs, None);
            reader.next_packet().unwrap().unwrap();
            assert!(matches!(seek(&mut reader, 95_000, by_time),
                Err(Error::SeekError(SeekErrorKind::OutOfRange))));
            seek(&mut reader, 0, by_time).unwrap();
            assert_replay(&drain(&mut reader), &expected);
        }
    }
}
