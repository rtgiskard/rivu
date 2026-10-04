//! Container probing, decoding, seeking, and packet-accurate native-channel samples.

use super::{BYTES_PER_MEBIBYTE, Channel, MediaInfo};
use anyhow::{Context, Result, anyhow, bail};
use std::{fs::File, path::Path};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::{
    core::{
        audio::{
            Audio, AudioBuffer, ChannelLabel, Channels, GenericAudioBufferRef, Position,
            conv::FromSample, sample::Sample,
        },
        codecs::{
            CodecParameters,
            audio::{
                AudioCodecParameters, AudioDecoder, AudioDecoderOptions, CODEC_ID_NULL_AUDIO,
                well_known::CODEC_ID_OPUS,
            },
        },
        formats::{FormatOptions, FormatReader, SeekMode, SeekTo, Track, TrackType, probe::Hint},
        io::{MediaSourceStream, MediaSourceStreamOptions},
        meta::{Metadata, MetadataOptions, StandardTag},
        units::{Time, TimeBase},
    },
    default::{get_codecs, get_probe},
};

const PROBE_BUFFER_LEN: usize = 2 * BYTES_PER_MEBIBYTE;

pub fn probe(path: &Path, ffmpeg_enabled: bool) -> Result<MediaInfo> {
    let detected = super::detector::scan(path)?;
    if let Some(detection) = detected {
        if !ffmpeg_enabled {
            let title = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            return Ok(MediaInfo {
                title,
                artist: String::new(),
                album: String::new(),
                duration: None,
                bitrate_bps: None,
                track_number: None,
                disc_number: None,
                bits_per_sample: None,
                release_date: None,
                codec: detection.codec.into(),
                channels: detection.channels.unwrap_or(0),
                sample_rate: detection.sample_rate.unwrap_or(0),
            });
        }
        return Ok(
            Source::open_with_detection(path, PROBE_BUFFER_LEN, true, Some(detection))?.info,
        );
    }
    Ok(Source::open_with_detection(path, PROBE_BUFFER_LEN, ffmpeg_enabled, None)?.info)
}

fn metadata_tags(mut metadata: Metadata<'_>, track: u32, info: &mut MediaInfo) {
    while let Some(revision) = metadata.current() {
        let track_tags = revision
            .per_track
            .iter()
            .filter(|metadata| metadata.track_id == u64::from(track))
            .flat_map(|metadata| &metadata.metadata.tags);
        for tag in revision.media.tags.iter().chain(track_tags) {
            match tag.std.as_ref() {
                Some(StandardTag::TrackTitle(value)) => info.title = value.as_ref().clone(),
                Some(StandardTag::Artist(value)) => info.artist = value.as_ref().clone(),
                Some(StandardTag::Album(value)) => info.album = value.as_ref().clone(),
                Some(StandardTag::TrackNumber(value)) => {
                    info.track_number = u32::try_from(*value).ok().filter(|value| *value > 0);
                }
                Some(StandardTag::DiscNumber(value)) => {
                    info.disc_number = u32::try_from(*value).ok().filter(|value| *value > 0);
                }
                Some(StandardTag::ReleaseDate(value) | StandardTag::RecordingDate(value))
                    if !value.trim().is_empty() =>
                {
                    info.release_date = Some(value.trim().to_owned());
                }
                Some(StandardTag::ReleaseYear(value) | StandardTag::RecordingYear(value))
                    if *value > 0 && info.release_date.is_none() =>
                {
                    info.release_date = Some(value.to_string());
                }
                _ => (),
            }
        }
        if metadata.pop().is_none() {
            break;
        }
    }
}
fn seconds(time: Time) -> f64 {
    time.as_secs_f64()
}
fn trim_samples(
    base: TimeBase,
    trim: symphonia::core::units::Duration,
    rate: u32,
) -> Result<usize> {
    let time = base
        .calc_duration(trim)
        .context("Packet trim out of range")?;
    Ok((seconds(time) * rate as f64).round().max(0.0) as usize)
}
fn duration(track: &Track, rate: u32) -> Option<f64> {
    if let (Some(duration), Some(base)) = (track.duration, track.time_base) {
        return base.calc_duration(duration).map(seconds);
    }
    track.num_frames.map(|frames| frames as f64 / rate as f64)
}

fn pcm_bitrate(params: &AudioCodecParameters) -> Option<u64> {
    use symphonia::core::codecs::audio::well_known as codec;

    // Encoded sample width, not decoded precision (e.g. G.711 decodes to 16 bits).
    // Symphonia 0.6 has no compressed-stream bitrate in AudioCodecParameters.
    let bits = match params.codec {
        codec::CODEC_ID_PCM_S8
        | codec::CODEC_ID_PCM_U8
        | codec::CODEC_ID_PCM_ALAW
        | codec::CODEC_ID_PCM_MULAW => 8_u64,
        codec::CODEC_ID_PCM_S16LE
        | codec::CODEC_ID_PCM_S16BE
        | codec::CODEC_ID_PCM_U16LE
        | codec::CODEC_ID_PCM_U16BE => 16,
        codec::CODEC_ID_PCM_S24LE
        | codec::CODEC_ID_PCM_S24BE
        | codec::CODEC_ID_PCM_U24LE
        | codec::CODEC_ID_PCM_U24BE => 24,
        codec::CODEC_ID_PCM_S32LE
        | codec::CODEC_ID_PCM_S32BE
        | codec::CODEC_ID_PCM_U32LE
        | codec::CODEC_ID_PCM_U32BE
        | codec::CODEC_ID_PCM_F32LE
        | codec::CODEC_ID_PCM_F32BE => 32,
        codec::CODEC_ID_PCM_F64LE | codec::CODEC_ID_PCM_F64BE => 64,
        _ => return None,
    };
    bits.checked_mul(u64::from(params.sample_rate?))?
        .checked_mul(params.channels.as_ref()?.count() as u64)
        .filter(|bitrate| *bitrate > 0)
}

fn source_bits_per_sample(params: &AudioCodecParameters) -> Option<u32> {
    use symphonia::core::codecs::audio::well_known as codec;

    let lossless = matches!(
        params.codec,
        codec::CODEC_ID_FLAC
            | codec::CODEC_ID_ALAC
            | codec::CODEC_ID_WAVPACK
            | codec::CODEC_ID_MONKEYS_AUDIO
            | codec::CODEC_ID_TTA
    );
    if pcm_bitrate(params).is_some() || lossless {
        params.bits_per_sample.filter(|bits| *bits > 0)
    } else {
        None
    }
}

struct OpusDecoder {
    decoder: opus::Decoder,
    samples: Vec<f32>,
    channels: usize,
    initial_skip: usize,
    skip: usize,
    timestamp_delay: f64,
}
enum Decode {
    Native(Box<dyn AudioDecoder>),
    Opus(OpusDecoder),
    Ffmpeg(super::ffmpeg::Decoder),
}
fn decoder(params: &AudioCodecParameters, timestamps_include_delay: bool) -> Result<Decode> {
    if params.codec != CODEC_ID_OPUS {
        return Ok(Decode::Native(
            get_codecs().make_audio_decoder(params, &AudioDecoderOptions::default())?,
        ));
    }
    let head = params
        .extra_data
        .as_deref()
        .context("Opus stream has no identification header")?;
    if head.len() < 19 || &head[..8] != b"OpusHead" {
        bail!("Invalid Opus identification header");
    }
    let channels = head[9] as usize;
    if channels == 0 || (head[18] == 0 && channels > 2) {
        bail!("Invalid Opus channel count for its mapping family");
    }
    if head[18] != 0 {
        return Err(anyhow!(UnsupportedNative(
            "Native Opus supports mono/stereo mapping family 0".into()
        )));
    }
    // MP4 dOps uses big-endian fields and version 0; Ogg/Matroska OpusHead
    // uses little-endian fields and version 1.
    let be = head[8] == 0;
    let preskip = if be {
        u16::from_be_bytes([head[10], head[11]])
    } else {
        u16::from_le_bytes([head[10], head[11]])
    } as usize;
    let gain = if be {
        i16::from_be_bytes([head[16], head[17]])
    } else {
        i16::from_le_bytes([head[16], head[17]])
    };
    let mut decoder = opus::Decoder::new(
        48_000,
        if channels == 1 {
            opus::Channels::Mono
        } else {
            opus::Channels::Stereo
        },
    )?;
    decoder.set_gain(gain as i32)?;
    // Symphonia 0.6's Ogg mapper records header delay on Track but does not
    // apply Opus preskip to packets. Apply it here, overlapping rather than
    // adding any packet trim. Matroska already subtracts CodecDelay from PTS.
    Ok(Decode::Opus(OpusDecoder {
        decoder,
        samples: vec![0.0; 5760 * channels],
        channels,
        initial_skip: preskip,
        skip: preskip,
        timestamp_delay: if timestamps_include_delay {
            preskip as f64 / 48_000.0
        } else {
            0.0
        },
    }))
}

#[derive(Debug)]
struct UnsupportedNative(String);
impl std::fmt::Display for UnsupportedNative {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for UnsupportedNative {}

pub(super) struct Source {
    format: Option<Box<dyn FormatReader>>,
    track: u32,
    decode: Decode,
    time_base: TimeBase,
    frames: Vec<f32>,
    layout: Vec<Channel>,
    info: MediaInfo,
    range_start: f64,
    range_end: Option<f64>,
    seek_target: Option<f64>,
    ready: bool,
    exhausted: bool,
}
impl Source {
    pub(super) fn open(path: &Path, buffer_len: usize, ffmpeg_enabled: bool) -> Result<Self> {
        let detected = super::detector::scan(path)?;
        Self::open_with_detection(path, buffer_len, ffmpeg_enabled, detected)
    }

    fn open_with_detection(
        path: &Path,
        buffer_len: usize,
        ffmpeg_enabled: bool,
        detected: Option<super::detector::Detection>,
    ) -> Result<Self> {
        if detected.is_some() {
            if !ffmpeg_enabled {
                bail!("Compressed audio requires the FFmpeg extension decoder");
            }
            return Self::open_ffmpeg(path);
        }
        match Self::open_native(path, buffer_len) {
            Ok(source) => Ok(source),
            Err(native_error) if ffmpeg_enabled && native_error.downcast_ref::<UnsupportedNative>().is_some() => {
                Self::open_ffmpeg(path).with_context(|| format!("Native decoder rejected the audio ({native_error:#}); FFmpeg extension failed"))
            }
            Err(error) => Err(error),
        }
    }

    fn open_ffmpeg(path: &Path) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("Opening {}", path.display()))?;
        let decode = super::ffmpeg::Decoder::open(&path)?;
        let info = decode.info().clone();
        let layout = decode.layout().to_vec();
        if info.sample_rate == 0 || layout.is_empty() {
            bail!("FFmpeg decoder returned incomplete audio metadata");
        }
        let duration = info.duration;
        let mut source = Self {
            format: None,
            track: 0,
            decode: Decode::Ffmpeg(decode),
            time_base: TimeBase::try_new(1, info.sample_rate).unwrap(),
            frames: Vec::with_capacity(8192 * layout.len()),
            layout,
            info,
            range_start: 0.0,
            range_end: duration,
            seek_target: None,
            ready: false,
            exhausted: false,
        };
        if !source.read_next()? {
            bail!("FFmpeg audio stream contains no samples");
        }
        source.ready = true;
        Ok(source)
    }

    fn open_native(path: &Path, buffer_len: usize) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("Opening {}", path.display()))?;
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
            hint.with_extension(extension);
        }
        let mut format = match get_probe().probe(
            &hint,
            MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions { buffer_len }),
            FormatOptions::default(),
            MetadataOptions::default(),
        ) {
            Ok(format) => format,
            Err(SymphoniaError::Unsupported(error)) => {
                return Err(anyhow!(UnsupportedNative(error.to_string())));
            }
            Err(error) => return Err(error.into()),
        };
        let default = format.default_track(TrackType::Audio).map(|track| track.id);
        let mut candidates = format.tracks().iter().collect::<Vec<_>>();
        candidates.sort_by_key(|track| Some(track.id) != default);
        let mut chosen = None;
        let mut errors = Vec::new();
        let mut invalid = None;
        for track in candidates {
            let Some(params) = track.codec_params.as_ref().and_then(CodecParameters::audio) else {
                continue;
            };
            if params.codec == CODEC_ID_NULL_AUDIO {
                continue;
            }
            match decoder(params, format.format_info().short_name != "matroska") {
                Ok(decode) => {
                    chosen = Some((track.id, params.clone(), track.time_base, decode));
                    break;
                }
                Err(error)
                    if error.is::<UnsupportedNative>()
                        || error.downcast_ref::<SymphoniaError>().is_some_and(|error| {
                            matches!(error, SymphoniaError::Unsupported(_))
                        }) =>
                {
                    errors.push(format!("{:?}: {error}", params.codec));
                }
                Err(error) => {
                    invalid.get_or_insert_with(|| {
                        error.context(format!("Opening {:?} audio", params.codec))
                    });
                }
            }
        }
        let (track, params, track_time_base, decode) = chosen.ok_or_else(|| {
            invalid.unwrap_or_else(|| {
                anyhow!(UnsupportedNative(if errors.is_empty() {
                    "Container has no supported native audio track".into()
                } else {
                    format!("No supported native audio decoder: {}", errors.join("; "))
                }))
            })
        })?;
        let is_opus = params.codec == CODEC_ID_OPUS;
        let rate = if is_opus {
            48_000
        } else {
            params
                .sample_rate
                .context("Audio track has no sample rate")?
        };
        if rate == 0 {
            bail!("Audio track has zero sample rate");
        }
        let codec = if is_opus {
            "Opus".into()
        } else {
            get_codecs()
                .get_audio_decoder(params.codec)
                .map(|codec| codec.codec.info.short_name.to_uppercase())
                .unwrap_or_else(|| format!("{:?}", params.codec))
        };
        let mut media_duration = format
            .tracks()
            .iter()
            .find(|candidate| candidate.id == track)
            .and_then(|track| duration(track, rate))
            .or_else(|| {
                let info = format.media_info();
                info.time_base?.calc_duration(info.duration?).map(seconds)
            });
        if let Decode::Opus(opus) = &decode {
            media_duration =
                media_duration.map(|value| (value - opus.initial_skip as f64 / 48_000.0).max(0.0));
        }
        let mut info = MediaInfo {
            title: path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            artist: String::new(),
            album: String::new(),
            duration: media_duration,
            bitrate_bps: pcm_bitrate(&params),
            track_number: None,
            disc_number: None,
            bits_per_sample: source_bits_per_sample(&params),
            release_date: None,
            codec,
            channels: params
                .channels
                .map_or(0, |channels| channels.count() as u16),
            sample_rate: rate,
        };
        metadata_tags(format.metadata(), track, &mut info);
        let mut source = Self {
            time_base: track_time_base.unwrap_or_else(|| TimeBase::try_new(1, rate).unwrap()),
            format: Some(format),
            track,
            decode,
            frames: Vec::with_capacity(8192),
            layout: Vec::new(),
            info,
            range_start: 0.0,
            range_end: media_duration,
            seek_target: None,
            ready: false,
            exhausted: false,
        };
        if !source.read_next()? {
            bail!("Audio track contains no decodable samples");
        }
        source.ready = true;
        Ok(source)
    }
    pub(super) fn restrict(&mut self, range: super::PlaybackRange) -> Result<()> {
        if !range.start_seconds.is_finite()
            || range.start_seconds < 0.0
            || range
                .end_seconds
                .is_some_and(|end| !end.is_finite() || end <= range.start_seconds)
        {
            bail!("Invalid audio segment bounds");
        }
        let end = match (range.end_seconds, self.range_end) {
            (Some(end), Some(duration)) => {
                if end > duration + 1.0 / 75.0 {
                    bail!("Audio segment ends beyond the source duration");
                }
                Some(end.min(duration))
            }
            (end, duration) => end.or(duration),
        };
        if end.is_some_and(|end| range.start_seconds >= end) {
            bail!("Audio segment starts at or beyond the source end");
        }
        self.range_start = range.start_seconds;
        self.range_end = end;
        self.info.duration = end.map(|end| end - range.start_seconds);
        self.seek(0.0)
    }

    pub(super) fn seek(&mut self, target: f64) -> Result<()> {
        let target = self
            .info
            .duration
            .map_or(target, |duration| target.min(duration))
            .max(0.0);
        if self
            .info
            .duration
            .is_some_and(|duration| target >= duration)
        {
            self.frames.clear();
            self.ready = false;
            self.seek_target = None;
            self.exhausted = true;
            return Ok(());
        }
        let target = self.range_start + target;
        if let Decode::Ffmpeg(decoder) = &mut self.decode {
            decoder.seek(target)?;
            self.frames.clear();
            self.ready = false;
            self.seek_target = Some(target);
            self.exhausted = false;
            return Ok(());
        }
        let preroll = if matches!(self.decode, Decode::Opus(_)) {
            (target - 0.08).max(0.0)
        } else {
            target
        };
        let sought = self
            .format
            .as_mut()
            .context("Native source has no format reader")?
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::try_from_secs_f64(preroll).context("Invalid seek time")?,
                    track_id: Some(self.track),
                },
            )?;
        match &mut self.decode {
            Decode::Native(decoder) => decoder.reset(),
            Decode::Opus(opus) => {
                opus.decoder.reset_state()?;
                opus.skip = if sought.actual_ts.get() <= 0 {
                    opus.initial_skip
                } else {
                    0
                };
            }
            Decode::Ffmpeg(_) => unreachable!(),
        }
        self.frames.clear();
        self.ready = false;
        self.seek_target = Some(target);
        self.exhausted = false;
        Ok(())
    }
    pub(super) fn info(&self) -> &MediaInfo {
        &self.info
    }

    pub(super) fn layout(&self) -> &[Channel] {
        &self.layout
    }

    /// Returns the primed first packet before advancing the decoder.
    pub(super) fn next_frames(&mut self) -> Result<Option<&[f32]>> {
        if self.ready {
            self.ready = false;
        } else if !self.read_next()? {
            return Ok(None);
        }
        Ok(Some(&self.frames))
    }

    pub(super) fn uses_ffmpeg(&self) -> bool {
        matches!(self.decode, Decode::Ffmpeg(_))
    }

    pub(super) fn waveform(
        path: &Path,
        range: Option<super::PlaybackRange>,
        media_read_buffer_mb: u32,
        points: usize,
        ffmpeg_enabled: bool,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<f32>> {
        let check_cancelled = || -> Result<()> {
            if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                bail!("Waveform loading cancelled");
            }
            Ok(())
        };
        check_cancelled()?;
        if points == 0 {
            bail!("Waveform point count must be positive");
        }
        let buffer_len = usize::try_from(media_read_buffer_mb)
            .context("Waveform media buffer size does not fit usize")?
            .saturating_mul(super::BYTES_PER_MEBIBYTE);
        let mut source = Self::open(path, buffer_len, ffmpeg_enabled)?;
        check_cancelled()?;
        if let Some(range) = range {
            source.restrict(range)?;
        }
        let duration = source
            .info
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .context("Audio track has no finite duration for waveform")?;
        let total_frames = (duration * source.info.sample_rate as f64).round().max(1.0) as usize;
        // Tiny clips must not alternate real samples with artificial empty bins.
        let points = points.min(total_frames);
        let mut envelope = vec![0.0_f32; points];
        let mut frame_index = 0usize;
        let mut point = 0usize;
        let mut boundary = total_frames.div_ceil(points);
        let channels = source.layout().len();
        loop {
            check_cancelled()?;
            let Some(frames) = source.next_frames()? else {
                break;
            };
            check_cancelled()?;
            let mut offset = 0;
            while offset < frames.len() {
                let packet_frames = (frames.len() - offset) / channels;
                let count = if point + 1 == points {
                    packet_frames
                } else {
                    packet_frames.min(boundary - frame_index)
                };
                let end = offset + count * channels;
                // Keep peaks from every channel (including antiphase stereo),
                // not a downmix. Reuse decoder storage and divide once per bin,
                // rather than allocating packet copies or dividing per sample.
                envelope[point] = frames[offset..end]
                    .iter()
                    .filter(|sample| sample.is_finite())
                    .fold(envelope[point], |peak, sample| peak.max(sample.abs()));
                offset = end;
                frame_index += count;
                if frame_index == boundary && point + 1 < points {
                    point += 1;
                    boundary = ((point as u128 + 1) * total_frames as u128)
                        .div_ceil(points as u128) as usize;
                }
            }
        }
        check_cancelled()?;
        if frame_index == 0 {
            envelope.clear();
        }
        Ok(envelope)
    }

    fn read_ffmpeg_next(&mut self) -> Result<bool> {
        loop {
            self.frames.clear();
            let Some((samples, packet_start)) = (match &mut self.decode {
                Decode::Ffmpeg(decoder) => decoder.next_frames()?,
                _ => unreachable!(),
            }) else {
                self.exhausted = true;
                return Ok(false);
            };
            self.frames.extend_from_slice(samples);
            let channels = self.layout.len();
            if channels == 0 || self.frames.len() % channels != 0 {
                bail!("FFmpeg decoder returned incomplete interleaved frames");
            }
            if let Some(end) = self.range_end {
                let remaining =
                    ((end - packet_start).max(0.0) * self.info.sample_rate as f64).round() as usize;
                self.exhausted = remaining <= self.frames.len() / channels;
                self.frames.truncate(remaining.saturating_mul(channels));
            }
            if let Some(target) = self.seek_target {
                let discard = (((target - packet_start).max(0.0) * self.info.sample_rate as f64)
                    .round() as usize)
                    .saturating_mul(channels);
                if discard >= self.frames.len() {
                    if self.exhausted {
                        return Ok(false);
                    }
                    continue;
                }
                self.frames.copy_within(discard.., 0);
                self.frames.truncate(self.frames.len() - discard);
                self.seek_target = None;
            }
            if !self.frames.is_empty() {
                return Ok(true);
            }
            if self.exhausted {
                return Ok(false);
            }
        }
    }

    fn read_next(&mut self) -> Result<bool> {
        if self.exhausted {
            return Ok(false);
        }
        if matches!(self.decode, Decode::Ffmpeg(_)) {
            return self.read_ffmpeg_next();
        }
        loop {
            // Seeking can discard whole packets (including Opus preroll).
            // Never append the next packet onto samples already discarded.
            self.frames.clear();
            let format = self
                .format
                .as_mut()
                .context("Native source has no format reader")?;
            let Some(packet) = format.next_packet()? else {
                self.exhausted = true;
                return Ok(false);
            };
            if packet.track_id != self.track {
                continue;
            }
            let mut packet_start = seconds(
                self.time_base
                    .calc_time(packet.pts)
                    .context("Packet timestamp out of range")?,
            );
            match &mut self.decode {
                Decode::Native(decoder) => {
                    let audio = decoder.decode(&packet)?;
                    if audio.spec().rate() != self.info.sample_rate {
                        bail!("Sample rate changes within this track are unsupported");
                    }
                    update_layout(audio.spec().channels(), &mut self.layout)?;
                    self.info.channels = self.layout.len() as u16;
                    interleave_audio(audio, &mut self.frames);
                    packet_start += self
                        .time_base
                        .calc_duration(packet.trim_start)
                        .context("Packet trim out of range")?
                        .as_secs_f64();
                }
                Decode::Opus(opus) => {
                    let count =
                        opus.decoder
                            .decode_float(&packet.data, &mut opus.samples, false)?;
                    let leading = trim_samples(self.time_base, packet.trim_start, 48_000)?
                        .max(opus.skip.min(count))
                        .min(count);
                    opus.skip = opus.skip.saturating_sub(count);
                    let end = count
                        .saturating_sub(trim_samples(self.time_base, packet.trim_end, 48_000)?)
                        .max(leading);
                    self.frames.extend_from_slice(
                        &opus.samples[leading * opus.channels..end * opus.channels],
                    );
                    if self.layout.is_empty() {
                        self.layout = if opus.channels == 1 {
                            vec![Channel::FrontCenter]
                        } else {
                            vec![Channel::FrontLeft, Channel::FrontRight]
                        };
                    }
                    self.info.channels = opus.channels as u16;
                    packet_start =
                        (packet_start - opus.timestamp_delay + leading as f64 / 48_000.0).max(0.0);
                }
                Decode::Ffmpeg(_) => unreachable!(),
            }
            if let Some(end) = self.range_end {
                let remaining =
                    ((end - packet_start).max(0.0) * self.info.sample_rate as f64).round() as usize;
                self.exhausted = remaining <= self.frames.len() / self.layout.len();
                self.frames
                    .truncate(remaining.saturating_mul(self.layout.len()));
            }
            if let Some(target) = self.seek_target {
                let discard = (((target - packet_start).max(0.0) * self.info.sample_rate as f64)
                    .round() as usize)
                    .saturating_mul(self.layout.len());
                if discard >= self.frames.len() {
                    if self.exhausted {
                        return Ok(false);
                    }
                    continue;
                }
                self.frames.copy_within(discard.., 0);
                self.frames.truncate(self.frames.len() - discard);
                self.seek_target = None;
            }
            if !self.frames.is_empty() {
                return Ok(true);
            }
            if self.exhausted {
                return Ok(false);
            }
        }
    }
}

fn update_layout(channels: &Channels, layout: &mut Vec<Channel>) -> Result<()> {
    let count = channels.count();
    if count == 0 {
        bail!("Decoded audio has no channels");
    }
    let initial = layout.is_empty();
    if !initial && layout.len() != count {
        bail!("Channel layout changes within this track are unsupported");
    }
    for index in 0..count {
        let position = match channels {
            Channels::Positioned(positions) => positions.iter().nth(index),
            Channels::Custom(labels) => match labels[index] {
                ChannelLabel::Positioned(position) => Some(position),
                _ => None,
            },
            Channels::Discrete(1) => Some(Position::FRONT_CENTER),
            Channels::Discrete(2) => Some(if index == 0 {
                Position::FRONT_LEFT
            } else {
                Position::FRONT_RIGHT
            }),
            _ => None,
        };
        let channel = position
            .filter(|position| position.bits().count_ones() == 1)
            .and_then(|position| Channel::from_standard_index(position.bits().trailing_zeros()))
            .context("Audio has an unsupported or unspecified speaker position; refusing to guess its routing")?;
        if initial {
            layout.push(channel);
        } else if layout[index] != channel {
            bail!("Channel layout changes within this track are unsupported");
        }
    }
    Ok(())
}

fn interleave_samples<S: Sample>(audio: &AudioBuffer<S>, output: &mut Vec<f32>)
where
    f32: FromSample<S>,
{
    let channels = audio.spec().channels().count();
    output.resize(audio.frames() * channels, 0.0);
    for channel in 0..channels {
        for (frame, sample) in output
            .chunks_exact_mut(channels)
            .zip(audio.plane(channel).unwrap())
        {
            frame[channel] = f32::from_sample(*sample);
        }
    }
}

fn interleave_audio(audio: GenericAudioBufferRef<'_>, output: &mut Vec<f32>) {
    match audio {
        GenericAudioBufferRef::U8(audio) => interleave_samples(audio, output),
        GenericAudioBufferRef::U16(audio) => interleave_samples(audio, output),
        GenericAudioBufferRef::U24(audio) => interleave_samples(audio, output),
        GenericAudioBufferRef::U32(audio) => interleave_samples(audio, output),
        GenericAudioBufferRef::S8(audio) => interleave_samples(audio, output),
        GenericAudioBufferRef::S16(audio) => interleave_samples(audio, output),
        GenericAudioBufferRef::S24(audio) => interleave_samples(audio, output),
        GenericAudioBufferRef::S32(audio) => interleave_samples(audio, output),
        GenericAudioBufferRef::F32(audio) => interleave_samples(audio, output),
        GenericAudioBufferRef::F64(audio) => interleave_samples(audio, output),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use symphonia::core::{checksum::Crc32, io::Monitor};

    // A real, seekable Ogg/Opus file without external tools or binary fixtures.
    fn opus_file() -> tempfile::NamedTempFile {
        fn page(file: &mut File, sequence: u32, flags: u8, granule: u64, packet: &[u8]) {
            let segments = packet.len() / 255 + 1;
            let mut page = Vec::with_capacity(27 + segments + packet.len());
            page.extend_from_slice(b"OggS\0");
            page.push(flags);
            page.extend_from_slice(&granule.to_le_bytes());
            page.extend_from_slice(&1_u32.to_le_bytes());
            page.extend_from_slice(&sequence.to_le_bytes());
            page.extend_from_slice(&[0; 4]);
            page.push(segments as u8);
            page.extend(std::iter::repeat_n(255, segments - 1));
            page.push((packet.len() % 255) as u8);
            page.extend_from_slice(packet);
            let mut crc = Crc32::new(0);
            crc.process_buf_bytes(&page);
            page[22..26].copy_from_slice(&crc.crc().to_le_bytes());
            file.write_all(&page).unwrap();
        }

        let mut file = tempfile::Builder::new().suffix(".opus").tempfile().unwrap();
        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Audio).unwrap();
        let preskip = encoder.get_lookahead().unwrap() as u16;
        let mut header = [0_u8; 19];
        header[..8].copy_from_slice(b"OpusHead");
        header[8] = 1;
        header[9] = 1;
        header[10..12].copy_from_slice(&preskip.to_le_bytes());
        header[12..16].copy_from_slice(&48_000_u32.to_le_bytes());
        page(file.as_file_mut(), 0, 2, 0, &header);
        page(file.as_file_mut(), 1, 0, 0, b"OpusTags\0\0\0\0\0\0\0\0");
        let total = 96_000 + usize::from(preskip);
        let packets = total.div_ceil(960);
        let mut encoded = [0_u8; 4000];
        // Nonperiodic input gives cross-correlation a unique sample alignment,
        // unlike a tone whose adjacent periods can look almost identical.
        let mut noise = 0x1234_5678_u32;
        for index in 0..packets {
            let input: [f32; 960] = std::array::from_fn(|offset| {
                let sample = index * 960 + offset;
                if sample >= 96_000 {
                    0.0
                } else {
                    noise ^= noise << 13;
                    noise ^= noise >> 17;
                    noise ^= noise << 5;
                    noise as i32 as f32 / i32::MAX as f32 * 0.5
                }
            });
            let count = encoder.encode_float(&input, &mut encoded).unwrap();
            page(
                file.as_file_mut(),
                index as u32 + 2,
                if index + 1 == packets { 4 } else { 0 },
                ((index + 1) * 960).min(total) as u64,
                &encoded[..count],
            );
        }
        file
    }

    fn decode_remaining(source: &mut Source) -> Vec<f32> {
        let mut samples = Vec::new();
        while let Some(frames) = source.next_frames().unwrap() {
            samples.extend_from_slice(frames);
        }
        samples
    }

    #[test]
    fn opus_seek_discards_whole_preroll_packets_without_reusing_pcm() {
        let file = opus_file();
        let mut source = Source::open(file.path(), PROBE_BUFFER_LEN, false).unwrap();
        let reference = decode_remaining(&mut source);
        assert_eq!(reference.len(), 96_000);
        for target in [1.0, 0.0, 1.137] {
            source.seek(target).unwrap();
            let actual = decode_remaining(&mut source);
            let offset = (target * 48_000.0).round() as usize;
            assert_eq!(actual.len(), reference.len() - offset);
            // Resetting Opus and decoding preroll does not reproduce every
            // adaptive decoder state from uninterrupted playback. Compare
            // temporal alignment, not a pinned waveform-error tolerance.
            // Include the entire 80 ms preroll in the candidate range so that
            // an early packet prefix cannot masquerade as the requested audio.
            let window = &actual[..960];
            let first = offset.saturating_sub(3840);
            let last = (offset + 3840).min(reference.len() - window.len());
            let best = (first..=last)
                .map(|candidate| {
                    let (dot, power) = window
                        .iter()
                        .zip(&reference[candidate..candidate + window.len()])
                        .fold((0.0_f64, 0.0_f64), |(dot, power), (actual, expected)| {
                            let expected = f64::from(*expected);
                            (
                                dot + f64::from(*actual) * expected,
                                power + expected * expected,
                            )
                        });
                    // The actual window's norm is constant across candidates.
                    (candidate, dot / power.sqrt())
                })
                .max_by(|(_, left), (_, right)| left.total_cmp(right))
                .unwrap()
                .0;
            assert_eq!(
                best, offset,
                "Seek must start at the requested sample for {target}s"
            );
        }
    }

    #[test]
    fn seek_to_duration_is_exhausted_and_can_seek_back() {
        let file = opus_file();
        let mut source = Source::open(file.path(), PROBE_BUFFER_LEN, false).unwrap();
        let duration = source.info().duration.unwrap();
        for target in [duration, duration + 1.0] {
            source.seek(target).unwrap();
            assert!(source.next_frames().unwrap().is_none());
            assert!(source.next_frames().unwrap().is_none());
            source.seek(0.0).unwrap();
            assert_eq!(decode_remaining(&mut source).len(), 96_000);
        }
    }

    fn pcm_file() -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let samples = 88_200_u32;
        let bytes = samples * 2;
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&(36 + bytes).to_le_bytes());
        header.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0");
        header.extend_from_slice(&44_100_u32.to_le_bytes());
        header.extend_from_slice(&88_200_u32.to_le_bytes());
        header.extend_from_slice(b"\x02\0\x10\0data");
        header.extend_from_slice(&bytes.to_le_bytes());
        file.write_all(&header).unwrap();
        for index in 0..samples {
            let value = ((index * 37) % 32_768) as i16 - 16_384;
            file.write_all(&value.to_le_bytes()).unwrap();
        }
        file
    }

    #[test]
    fn waveform_covers_the_full_file_and_exact_cue_boundaries() {
        let file = pcm_file();
        let mut source = Source::open(file.path(), PROBE_BUFFER_LEN, false).unwrap();
        let reference = decode_remaining(&mut source);
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        for (start, end, points) in [
            (0_usize, None, 17_usize),
            (30, Some(80_usize), 97),
            (80, None, 1600),
            (30, Some(31), 1600),
        ] {
            let range = (start != 0 || end.is_some()).then_some(super::super::PlaybackRange {
                start_seconds: start as f64 / 75.0,
                end_seconds: end.map(|end| end as f64 / 75.0),
            });
            let samples = &reference[start * 588..end.map_or(reference.len(), |end| end * 588)];
            let count = points.min(samples.len());
            let mut expected = vec![0.0_f32; count];
            for (index, sample) in samples.iter().enumerate() {
                let bin = index * count / samples.len();
                expected[bin] = expected[bin].max(sample.abs());
            }
            let actual = Source::waveform(file.path(), range, 2, points, false, &cancelled).unwrap();
            assert_eq!(actual, expected, "CUE range {start}..{end:?}, {points} points");
        }
    }

    #[test]
    fn waveform_keeps_channel_peaks_and_does_not_pad_short_clips() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let frames: [[i16; 2]; 6] = [
            [8192, -8192],
            [0, 16384],
            [-24576, 0],
            [0, 0],
            [4096, -4096],
            [0, i16::MIN],
        ];
        let bytes = frames.len() as u32 * 4;
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&(36 + bytes).to_le_bytes());
        header.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x02\0");
        header.extend_from_slice(&48_000_u32.to_le_bytes());
        header.extend_from_slice(&192_000_u32.to_le_bytes());
        header.extend_from_slice(b"\x04\0\x10\0data");
        header.extend_from_slice(&bytes.to_le_bytes());
        file.write_all(&header).unwrap();
        for frame in frames {
            for sample in frame {
                file.write_all(&sample.to_le_bytes()).unwrap();
            }
        }
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let envelope = Source::waveform(file.path(), None, 2, 1600, false, &cancelled).unwrap();
        assert_eq!(envelope, [0.25, 0.5, 0.75, 0.0, 0.125, 1.0]);
    }

    #[test]
    fn waveform_preserves_float_peaks_above_full_scale() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let samples = [0.25_f32, -1.0, 1.5, -2.0, f32::NAN, f32::INFINITY];
        let bytes = samples.len() as u32 * 4;
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + bytes).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt \x10\0\0\0\x03\0\x01\0").unwrap();
        file.write_all(&48_000_u32.to_le_bytes()).unwrap();
        file.write_all(&192_000_u32.to_le_bytes()).unwrap();
        file.write_all(b"\x04\0\x20\0data").unwrap();
        file.write_all(&bytes.to_le_bytes()).unwrap();
        for sample in samples {
            file.write_all(&sample.to_le_bytes()).unwrap();
        }
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let envelope = Source::waveform(file.path(), None, 2, 1600, false, &cancelled).unwrap();
        assert_eq!(envelope, [0.25, 1.0, 1.5, 2.0, 0.0, 0.0]);
    }

    #[test]
    fn waveform_cancellation_precedes_opening_the_file() {
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let error = Source::waveform(Path::new(""), None, 2, 1600, false, &cancelled).unwrap_err();
        assert_eq!(error.to_string(), "Waveform loading cancelled");
    }

    #[test]
    fn cue_ranges_crop_pcm_and_seek_relative_to_track() {
        let file = pcm_file();
        let mut full = Source::open(file.path(), PROBE_BUFFER_LEN, false).unwrap();
        assert_eq!(full.info().bitrate_bps, Some(705_600));
        assert_eq!(full.info().bits_per_sample, Some(16));
        let reference = decode_remaining(&mut full);
        for (start_frame, end_frame) in [(0_u64, Some(30_u64)), (30, Some(80)), (80, None)] {
            let start = (start_frame * 588) as usize;
            let end = end_frame.map_or(reference.len(), |frame| (frame * 588) as usize);
            let mut source = Source::open(file.path(), PROBE_BUFFER_LEN, false).unwrap();
            source
                .restrict(super::super::PlaybackRange {
                    start_seconds: start_frame as f64 / 75.0,
                    end_seconds: end_frame.map(|frame| frame as f64 / 75.0),
                })
                .unwrap();
            assert!(
                (source.info().duration.unwrap() - (end - start) as f64 / 44_100.0).abs() < 1e-9
            );
            assert_eq!(decode_remaining(&mut source), reference[start..end]);
            source.seek(0.2).unwrap();
            assert_eq!(decode_remaining(&mut source), reference[start + 8_820..end]);
            source.seek(source.info().duration.unwrap()).unwrap();
            assert!(source.next_frames().unwrap().is_none());
            source.seek(0.0).unwrap();
            assert_eq!(decode_remaining(&mut source), reference[start..end]);
        }
    }

    #[test]
    fn cue_opus_preroll_and_end_remain_inside_the_segment() {
        let file = opus_file();
        let mut source = Source::open(file.path(), PROBE_BUFFER_LEN, false).unwrap();
        source
            .restrict(super::super::PlaybackRange {
                start_seconds: 1.0,
                end_seconds: Some(1.5),
            })
            .unwrap();
        assert_eq!(source.info().duration, Some(0.5));
        assert_eq!(source.info().bitrate_bps, None);
        assert_eq!(source.info().bits_per_sample, None);
        assert_eq!(decode_remaining(&mut source).len(), 24_000);
        source.seek(0.25).unwrap();
        assert_eq!(decode_remaining(&mut source).len(), 12_000);
        source.seek(0.5).unwrap();
        assert!(source.next_frames().unwrap().is_none());
    }

    #[test]
    fn opus_headers_preserve_mono_stereo_preskip_and_container_timing() {
        for channels in [1, 2] {
            for mp4 in [false, true] {
                let mut header = vec![0; 19];
                header[..8].copy_from_slice(b"OpusHead");
                header[8] = if mp4 { 0 } else { 1 };
                header[9] = channels;
                header[10..12].copy_from_slice(&if mp4 {
                    312u16.to_be_bytes()
                } else {
                    312u16.to_le_bytes()
                });
                let mut params = AudioCodecParameters::new();
                params
                    .for_codec(CODEC_ID_OPUS)
                    .with_extra_data(header.into_boxed_slice());
                let Decode::Opus(opus) = decoder(&params, true).unwrap() else {
                    panic!("Opus")
                };
                assert_eq!(opus.channels, usize::from(channels));
                assert_eq!(opus.initial_skip, 312);
                assert_eq!(opus.skip, 312);
                assert_eq!(opus.timestamp_delay, 312.0 / 48_000.0);
                let Decode::Opus(matroska) = decoder(&params, false).unwrap() else {
                    panic!("Opus")
                };
                assert_eq!(matroska.skip, 312);
                assert_eq!(matroska.timestamp_delay, 0.0);
            }
        }
    }

    #[test]
    fn packet_trims_use_the_container_time_base() {
        let duration = symphonia::core::units::Duration::new(10);
        assert_eq!(
            trim_samples(TimeBase::try_new(1, 1000).unwrap(), duration, 48_000).unwrap(),
            480
        );
        assert_eq!(
            trim_samples(TimeBase::try_new(1, 48_000).unwrap(), duration, 48_000).unwrap(),
            10
        );
    }
    #[test]
    fn unsupported_opus_mapping_is_not_a_corrupt_header() {
        let mut header = vec![0; 27];
        header[..8].copy_from_slice(b"OpusHead");
        header[8] = 1;
        header[9] = 6;
        header[18] = 1;
        header[19] = 4;
        header[20] = 2;
        header[21..].copy_from_slice(&[0, 4, 1, 2, 3, 5]);
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_OPUS)
            .with_extra_data(header.clone().into_boxed_slice());
        assert!(
            decoder(&params, true)
                .err()
                .unwrap()
                .is::<UnsupportedNative>()
        );

        // Family zero cannot describe six channels, so retrying another
        // decoder must not turn a malformed native stream into "unsupported".
        header[18] = 0;
        params.with_extra_data(header.into_boxed_slice());
        assert!(
            !decoder(&params, true)
                .err()
                .unwrap()
                .is::<UnsupportedNative>()
        );
    }
}
