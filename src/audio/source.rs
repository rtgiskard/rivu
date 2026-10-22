//! Container probing, decoding, seeking, and packet-accurate native-channel samples.

use super::{
    BYTES_PER_MEBIBYTE, Channel, MediaInfo,
    bitrate::{self, PacketStats},
};
use crate::model::DecoderHint;
use anyhow::{Context, Result, anyhow, bail};
use std::{
    fs::{File, OpenOptions},
    io::Seek,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
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
pub(super) const DECODE_WORK_BUDGET: usize = 64;

/// Share one admission budget across probing and all later metadata revisions.
/// Playback/probe retain tags only; artwork explicitly opts into visual bytes.
pub(crate) fn metadata_budget(visuals: bool) -> symphonia_metadata::budget::MetadataBudget {
    use symphonia::core::common::Limit;
    symphonia_metadata::budget::MetadataBudget::new(MetadataOptions::default().limit_visual_bytes(
        Limit::Maximum(if visuals {
            symphonia_metadata::budget::MAX_VISUAL_BYTES
        } else {
            0
        }),
    ))
}

/// Open before inspecting metadata so a replaced path cannot block on a FIFO.
/// Normal symlinks are followed; only the actual opened object must be regular.
pub(crate) fn open_regular_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("Opening {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("Audio source {} is not an ordinary file", path.display());
    }
    Ok(file)
}

/// Content version captured from the same file descriptor used by the decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceIdentity {
    device: u64,
    inode: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl SourceIdentity {
    pub(crate) fn from_file(file: &File) -> Result<Self> {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            bail!("Audio source is not an ordinary file");
        }
        Ok(Self::from_metadata(&metadata))
    }

    pub(crate) fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum DecodeStep<T> {
    Frames(T),
    Yield,
    Eof,
}

fn native_error(error: SymphoniaError) -> anyhow::Error {
    match error {
        SymphoniaError::Unsupported(message) => anyhow!(UnsupportedNative(message.to_string())),
        error => error.into(),
    }
}

pub(super) fn check_cancelled(cancelled: Option<&AtomicBool>) -> Result<()> {
    if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Relaxed)) {
        bail!("Waveform loading cancelled");
    }
    Ok(())
}

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeBackend {
    Native,
    Ffmpeg,
}

impl std::fmt::Display for ProbeBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Native => "native",
            Self::Ffmpeg => "ffmpeg",
        })
    }
}

pub struct ProbeResult {
    pub info: MediaInfo,
    pub probe_backend: ProbeBackend,
    pub decoder_backend: ProbeBackend,
}

pub fn probe(path: &Path, ffmpeg_enabled: bool) -> Result<MediaInfo> {
    Ok(Source::open(path, PROBE_BUFFER_LEN, DecoderHint::Auto, ffmpeg_enabled)?.info)
}

/// Probe the already opened object without looking up its original path again.
pub(crate) fn probe_file(path: &Path, mut file: File, ffmpeg_enabled: bool) -> Result<MediaInfo> {
    SourceIdentity::from_file(&file)?;
    file.rewind()
        .context("Rewinding audio source for probing")?;
    Ok(Source::open_file_with_cancel(
        path,
        file,
        PROBE_BUFFER_LEN,
        DecoderHint::Auto,
        ffmpeg_enabled,
        None,
    )?
    .info)
}

pub fn probe_details(path: &Path, ffmpeg_enabled: bool, enhanced: bool) -> Result<ProbeResult> {
    if enhanced {
        super::ffmpeg::ensure_loaded().context("FFmpeg enhanced probing is unavailable")?;
    }
    let source = Source::open(
        path,
        PROBE_BUFFER_LEN,
        DecoderHint::Auto,
        ffmpeg_enabled || enhanced,
    )?;
    let decoder_backend = if source.uses_ffmpeg() {
        ProbeBackend::Ffmpeg
    } else {
        ProbeBackend::Native
    };
    Ok(ProbeResult {
        probe_backend: source.probe_backend,
        decoder_backend,
        info: source.info,
    })
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
fn trim_sample_count(base: TimeBase, ticks: u64, rate: u32) -> Option<u64> {
    let scaled = u128::from(ticks) * u128::from(base.numer.get()) * u128::from(rate);
    let denominator = u128::from(base.denom.get());
    if scaled % denominator != 0 {
        return None;
    }
    u64::try_from(scaled / denominator).ok()
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
    Ffmpeg(Box<super::ffmpeg::Decoder>),
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
    identity: SourceIdentity,
    format: Option<Box<dyn FormatReader>>,
    metadata_budget: symphonia_metadata::budget::MetadataBudget,
    track: u32,
    decode: Decode,
    time_base: TimeBase,
    frames: Vec<f32>,
    frame_start: f64,
    layout: Vec<Channel>,
    info: MediaInfo,
    probe_backend: ProbeBackend,
    range_start: f64,
    range_end: Option<f64>,
    seek_target: Option<f64>,
    ready: bool,
    exhausted: bool,
    packet_stats: PacketStats,
    complete_stream: bool,
    expected_samples: Option<u64>,
    mpeg_packets: bool,
    opening_fallback: Option<(PathBuf, File)>,
}
impl Source {
    pub(super) fn open(
        path: &Path,
        buffer_len: usize,
        decoder_hint: DecoderHint,
        ffmpeg_enabled: bool,
    ) -> Result<Self> {
        Self::open_with_cancel(path, buffer_len, decoder_hint, ffmpeg_enabled, None)
    }

    /// Open the container/codec without draining the first-frame packet prefix.
    /// The worker retains this source between finite `poll_open` rounds.
    pub(super) fn begin_open(
        path: &Path,
        buffer_len: usize,
        decoder_hint: DecoderHint,
        ffmpeg_enabled: bool,
    ) -> Result<Self> {
        Self::open_file_unprimed(
            path,
            open_regular_file(path)?,
            buffer_len,
            decoder_hint,
            ffmpeg_enabled,
            None,
        )
    }

    fn open_with_cancel(
        path: &Path,
        buffer_len: usize,
        decoder_hint: DecoderHint,
        ffmpeg_enabled: bool,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Self> {
        check_cancelled(cancelled)?;
        Self::open_file_with_cancel(
            path,
            open_regular_file(path)?,
            buffer_len,
            decoder_hint,
            ffmpeg_enabled,
            cancelled,
        )
    }

    fn open_file_with_cancel(
        path: &Path,
        file: File,
        buffer_len: usize,
        decoder_hint: DecoderHint,
        ffmpeg_enabled: bool,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Self> {
        let mut source = Self::open_file_unprimed(
            path,
            file,
            buffer_len,
            decoder_hint,
            ffmpeg_enabled,
            cancelled,
        )?;
        while !source.poll_open_with_cancel(cancelled)? {}
        Ok(source)
    }

    fn open_file_unprimed(
        path: &Path,
        file: File,
        buffer_len: usize,
        decoder_hint: DecoderHint,
        ffmpeg_enabled: bool,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Self> {
        check_cancelled(cancelled)?;
        match decoder_hint {
            DecoderHint::Auto => (),
            DecoderHint::Native => {
                return Self::open_native(path, buffer_len, file, cancelled);
            }
            DecoderHint::Ffmpeg => {
                if !ffmpeg_enabled {
                    bail!("This track requires FFmpeg, but the FFmpeg extension is disabled");
                }
                return Self::from_ffmpeg(super::ffmpeg::Decoder::from_file(
                    path, file, cancelled,
                )?);
            }
        }
        let input = if ffmpeg_enabled {
            super::ffmpeg::inspect_wave(path, &file)?
        } else {
            None
        };
        if input.as_ref().is_some_and(|input| !input.is_pcm()) {
            drop(file);
            return Self::from_ffmpeg(super::ffmpeg::Decoder::from_input(
                input.unwrap(),
                cancelled,
            )?);
        }
        // Native probing consumes its descriptor even when it rejects the codec.
        // Retain this exact object for Auto fallback, not its mutable pathname.
        let fallback = if ffmpeg_enabled {
            Some(
                file.try_clone()
                    .context("Retaining audio source for FFmpeg fallback")?,
            )
        } else {
            None
        };
        match Self::open_native(path, buffer_len, file, cancelled) {
            Ok(mut source) => {
                if input.is_some() {
                    source.probe_backend = ProbeBackend::Ffmpeg;
                }
                source.opening_fallback = fallback.map(|file| (path.to_owned(), file));
                Ok(source)
            }
            Err(native_error)
                if ffmpeg_enabled && native_error.downcast_ref::<UnsupportedNative>().is_some() =>
            {
                let decoded = match input {
                    Some(input) => super::ffmpeg::Decoder::from_input(input, cancelled),
                    None => super::ffmpeg::Decoder::from_file(path, fallback.unwrap(), cancelled),
                };
                decoded.and_then(Self::from_ffmpeg).with_context(|| format!("Native decoder rejected the audio ({native_error:#}); FFmpeg extension failed"))
            }
            Err(error) => Err(error),
        }
    }

    fn from_ffmpeg(decode: super::ffmpeg::Decoder) -> Result<Self> {
        let info = decode.info().clone();
        let layout = decode.layout().to_vec();
        let time_base = decode.time_base()?;
        // Container duration is only an estimate, never a decoder EOF.
        Ok(Self {
            identity: decode.identity(),
            format: None,
            metadata_budget: metadata_budget(false),
            track: 0,
            decode: Decode::Ffmpeg(Box::new(decode)),
            time_base,
            frames: Vec::new(),
            frame_start: 0.0,
            layout,
            info,
            probe_backend: ProbeBackend::Ffmpeg,
            range_start: 0.0,
            range_end: None,
            seek_target: None,
            ready: false,
            exhausted: false,
            packet_stats: PacketStats::new(),
            complete_stream: true,
            expected_samples: None,
            mpeg_packets: false,
            opening_fallback: None,
        })
    }

    fn open_native(
        path: &Path,
        buffer_len: usize,
        file: File,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Self> {
        let identity = SourceIdentity::from_file(&file)?;
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
            hint.with_extension(extension);
        }
        let mut stream =
            MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions { buffer_len });
        let compressed_summary = bitrate::probe(&mut stream, cancelled)?;
        let mut metadata_budget = metadata_budget(false);
        let mut format = match metadata_budget.with(|| {
            get_probe().probe(
                &hint,
                stream,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
        }) {
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
        let compressed_summary = compressed_summary.filter(|summary| summary.codec == params.codec);
        let mpeg_packets = params.codec == symphonia::core::codecs::audio::well_known::CODEC_ID_MP3;
        // Only exact codec/container sample counts, never estimated durations.
        let expected_samples = if pcm_bitrate(&params).is_some()
            || params.codec == symphonia::core::codecs::audio::well_known::CODEC_ID_FLAC
        {
            format
                .tracks()
                .iter()
                .find(|candidate| candidate.id == track)
                .and_then(|track| track.num_frames)
                .filter(|samples| *samples > 0)
        } else if mpeg_packets {
            compressed_summary
                .as_ref()
                .and_then(|summary| summary.expected_samples)
        } else {
            None
        };
        let mut packet_stats = PacketStats::new();
        if compressed_summary
            .as_ref()
            .is_some_and(|summary| !summary.completion_valid)
        {
            packet_stats.invalidate();
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
            bitrate_bps: pcm_bitrate(&params).or_else(|| {
                if params.codec == symphonia::core::codecs::audio::well_known::CODEC_ID_VORBIS {
                    return params
                        .extra_data
                        .as_deref()
                        .and_then(bitrate::vorbis_nominal);
                }
                compressed_summary
                    .as_ref()
                    .and_then(|summary| summary.bitrate)
            }),
            bitrate_exact: pcm_bitrate(&params).is_some(),
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
        Ok(Self {
            identity,
            time_base: track_time_base.unwrap_or_else(|| TimeBase::try_new(1, rate).unwrap()),
            format: Some(format),
            metadata_budget,
            track,
            decode,
            frames: Vec::with_capacity(8192),
            frame_start: 0.0,
            layout: Vec::new(),
            info,
            probe_backend: ProbeBackend::Native,
            range_start: 0.0,
            range_end: None,
            seek_target: None,
            ready: false,
            exhausted: false,
            packet_stats,
            complete_stream: true,
            expected_samples,
            mpeg_packets,
            opening_fallback: None,
        })
    }
    pub(super) fn restrict(&mut self, range: super::PlaybackRange) -> Result<()> {
        self.restrict_with_cancel(range, None)
    }

    fn restrict_with_cancel(
        &mut self,
        range: super::PlaybackRange,
        cancelled: Option<&AtomicBool>,
    ) -> Result<()> {
        check_cancelled(cancelled)?;
        self.complete_stream = false;
        self.packet_stats.invalidate();
        if !range.start_seconds.is_finite()
            || range.start_seconds < 0.0
            || range
                .end_seconds
                .is_some_and(|end| !end.is_finite() || end <= range.start_seconds)
        {
            bail!("Invalid audio segment bounds");
        }
        let end = range.end_seconds;
        let duration = end.or(self.info.duration);
        self.range_start = range.start_seconds;
        self.range_end = end;
        self.info.duration = duration.map(|end| (end - range.start_seconds).max(0.0));
        self.seek_with_cancel(0.0, cancelled)
    }

    pub(super) fn seek(&mut self, target: f64) -> Result<()> {
        self.seek_with_cancel(target, None)
    }

    fn seek_with_cancel(&mut self, target: f64, cancelled: Option<&AtomicBool>) -> Result<()> {
        check_cancelled(cancelled)?;
        self.complete_stream = false;
        self.packet_stats.invalidate();
        let target = target.max(0.0);
        if self
            .range_end
            .is_some_and(|end| self.range_start + target >= end)
        {
            self.frames.clear();
            self.ready = false;
            self.seek_target = None;
            self.exhausted = true;
            return Ok(());
        }
        let target = self.range_start + target;
        if let Decode::Ffmpeg(decoder) = &mut self.decode {
            decoder.seek(target, cancelled)?;
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
        let time = Time::try_from_secs_f64(preroll).context("Invalid seek time")?;
        let format = self
            .format
            .as_mut()
            .context("Native source has no format reader")?;
        // A demuxer seeks to a packet containing the anchor, but the endpoint
        // itself has no packet (notably in native FLAC). Keep the anchor inside
        // the reported span; the original target below still controls discard.
        // A short duration hint therefore never hides later samples or proves EOF.
        let time = match format
            .tracks()
            .iter()
            .find(|track| track.id == self.track)
            .and_then(|track| duration(track, self.info.sample_rate))
            .filter(|end| preroll >= *end)
        {
            Some(end) => {
                Time::try_from_secs_f64((end - 1.0 / f64::from(self.info.sample_rate)).max(0.0))
                    .context("Invalid seek anchor")?
            }
            None => time,
        };
        let sought = self.metadata_budget.with(|| {
            format.seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track),
                },
            )
        })?;
        check_cancelled(cancelled)?;
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

    /// Available only after actual demux/decoder EOF, never segment exhaustion.
    pub(super) fn completed_bitrate(&self) -> Option<u64> {
        if !self.complete_stream {
            return None;
        }
        match &self.decode {
            Decode::Ffmpeg(decoder) => decoder.completed_bitrate(),
            _ => self.packet_stats.bitrate(1, self.info.sample_rate),
        }
    }

    pub(super) fn layout(&self) -> &[Channel] {
        &self.layout
    }

    /// Returns PCM and its first sample's segment-relative decoded timestamp.
    /// The primed first packet is returned before advancing the decoder.
    pub(super) fn next_frames_with_budget(
        &mut self,
        budget: &mut usize,
    ) -> Result<DecodeStep<(&[f32], f64)>> {
        self.next_frames_with_cancel(None, budget)
    }

    fn next_frames_with_cancel(
        &mut self,
        cancelled: Option<&AtomicBool>,
        budget: &mut usize,
    ) -> Result<DecodeStep<(&[f32], f64)>> {
        check_cancelled(cancelled)?;
        if *budget == 0 {
            return Ok(DecodeStep::Yield);
        }
        if self.ready {
            *budget -= 1;
            self.ready = false;
        } else {
            match self.read_next(cancelled, budget)? {
                DecodeStep::Frames(()) => (),
                DecodeStep::Yield => return Ok(DecodeStep::Yield),
                DecodeStep::Eof => return Ok(DecodeStep::Eof),
            }
        }
        Ok(DecodeStep::Frames((&self.frames, self.frame_start)))
    }

    // Full-file test consumers deliberately drain successive finite rounds.
    #[cfg(test)]
    fn next_frames(&mut self) -> Result<Option<(&[f32], f64)>> {
        if !self.ready && !self.prime(None)? {
            return Ok(None);
        }
        self.ready = false;
        Ok(Some((&self.frames, self.frame_start)))
    }

    /// One finite first-frame round. A successful round preserves its PCM for
    /// the first playback refill; EOF before that frame is an opening failure.
    pub(super) fn poll_open(&mut self) -> Result<bool> {
        self.poll_open_with_cancel(None)
    }

    pub(super) fn disable_opening_fallback(&mut self) {
        self.opening_fallback = None;
    }

    fn poll_open_with_cancel(&mut self, cancelled: Option<&AtomicBool>) -> Result<bool> {
        if self.ready {
            return Ok(true);
        }
        let mut budget = DECODE_WORK_BUDGET;
        let step = match self.read_next(cancelled, &mut budget) {
            Err(error) if error.is::<UnsupportedNative>() && self.opening_fallback.is_some() => {
                let (path, file) = self.opening_fallback.take().unwrap();
                *self =
                    Self::from_ffmpeg(super::ffmpeg::Decoder::from_file(&path, file, cancelled)?)
                        .with_context(|| {
                        format!(
                            "Native decoder rejected the audio ({error:#}); FFmpeg extension failed"
                        )
                    })?;
                return Ok(false);
            }
            result => result?,
        };
        match step {
            DecodeStep::Frames(()) => {
                self.ready = true;
                self.opening_fallback = None;
                Ok(true)
            }
            DecodeStep::Yield => Ok(false),
            DecodeStep::Eof => bail!("Audio track contains no decodable samples"),
        }
    }

    #[cfg(test)]
    fn prime(&mut self, cancelled: Option<&AtomicBool>) -> Result<bool> {
        loop {
            let mut budget = DECODE_WORK_BUDGET;
            match self.read_next(cancelled, &mut budget)? {
                DecodeStep::Frames(()) => return Ok(true),
                DecodeStep::Yield => continue,
                DecodeStep::Eof => return Ok(false),
            }
        }
    }

    pub(super) fn identity(&self) -> SourceIdentity {
        self.identity
    }

    pub(super) fn uses_ffmpeg(&self) -> bool {
        matches!(self.decode, Decode::Ffmpeg(_))
    }

    /// Adjacent quantized packet timestamps can differ by one container tick.
    /// Sample-accurate timelines retain the half-sample coverage threshold.
    pub(super) fn waveform_timestamp_tolerance(&self) -> f64 {
        let tick = f64::from(self.time_base.numer.get()) / f64::from(self.time_base.denom.get());
        if tick > f64::from(self.info.sample_rate).recip() {
            tick
        } else {
            0.0
        }
    }

    pub(super) fn waveform(
        path: &Path,
        range: Option<super::PlaybackRange>,
        decoder_hint: DecoderHint,
        media_read_buffer_mb: u32,
        ffmpeg_enabled: bool,
        cancelled: &AtomicBool,
    ) -> Result<super::WaveformFrame> {
        check_cancelled(Some(cancelled))?;
        let buffer_len = usize::try_from(media_read_buffer_mb)
            .context("Waveform media buffer size does not fit usize")?
            .saturating_mul(super::BYTES_PER_MEBIBYTE);
        let mut source = Self::open_with_cancel(
            path,
            buffer_len,
            decoder_hint,
            ffmpeg_enabled,
            Some(cancelled),
        )?;
        check_cancelled(Some(cancelled))?;
        if let Some(range) = range {
            source.restrict_with_cancel(range, Some(cancelled))?;
        }
        source.scan_waveform(path, range, decoder_hint, cancelled)
    }

    fn scan_waveform(
        &mut self,
        path: &Path,
        range: Option<super::PlaybackRange>,
        decoder_hint: DecoderHint,
        cancelled: &AtomicBool,
    ) -> Result<super::WaveformFrame> {
        let mut frame = super::WaveformFrame::default();
        let sample_rate = self.info.sample_rate;
        let channels = self.layout.len();
        frame.select(path, range, decoder_hint, self.info.duration, sample_rate);
        let weights = super::channels::analysis_weights(&self.layout);
        frame.set_channel_weights(&weights);
        let mut end_seconds = 0.0_f64;
        loop {
            let mut budget = DECODE_WORK_BUDGET;
            let (samples, start_seconds) =
                match self.next_frames_with_cancel(Some(cancelled), &mut budget)? {
                    DecodeStep::Frames(frame) => frame,
                    DecodeStep::Yield => continue,
                    DecodeStep::Eof => break,
                };
            frame.push(start_seconds, samples, sample_rate, channels);
            let end_frame =
                (start_seconds * sample_rate as f64).round() + (samples.len() / channels) as f64;
            end_seconds = end_seconds.max(end_frame / sample_rate as f64);
        }
        check_cancelled(Some(cancelled))?;
        frame.finish_scan(end_seconds);
        Ok(frame)
    }

    // Both backends supply codec-trimmed interleaved PCM and an absolute timestamp.
    // Empty PCM means keep decoding unless the explicit segment end was reached;
    // it is neither decoder EOF nor a reason to reset the caller's work budget.
    fn retain_pcm(&mut self, mut packet_start: f64) -> bool {
        let channels = self.layout.len();
        if let Some(end) = self.range_end {
            let remaining =
                ((end - packet_start).max(0.0) * self.info.sample_rate as f64).round() as usize;
            self.exhausted = remaining <= self.frames.len() / channels;
            self.frames.truncate(remaining.saturating_mul(channels));
        }
        if let Some(target) = self.seek_target {
            let discard =
                (((target - packet_start).max(0.0) * self.info.sample_rate as f64).round()
                    as usize)
                    .saturating_mul(channels);
            if discard >= self.frames.len() {
                return false;
            }
            if discard != 0 {
                self.frames.copy_within(discard.., 0);
                self.frames.truncate(self.frames.len() - discard);
                packet_start += (discard / channels) as f64 / self.info.sample_rate as f64;
            }
            self.seek_target = None;
        }
        if self.frames.is_empty() {
            return false;
        }
        self.frame_start = packet_start - self.range_start;
        true
    }

    fn read_ffmpeg_next(
        &mut self,
        cancelled: Option<&AtomicBool>,
        budget: &mut usize,
    ) -> Result<DecodeStep<()>> {
        loop {
            check_cancelled(cancelled)?;
            let decoded = match &mut self.decode {
                Decode::Ffmpeg(decoder) => decoder.next_frames(&mut self.frames, cancelled, budget),
                _ => unreachable!(),
            };
            let packet_start = match decoded {
                Ok(DecodeStep::Frames(timestamp)) => timestamp,
                Ok(DecodeStep::Yield) => return Ok(DecodeStep::Yield),
                Ok(DecodeStep::Eof) => {
                    self.frames.clear();
                    self.exhausted = true;
                    return Ok(DecodeStep::Eof);
                }
                Err(error) => {
                    self.frames.clear();
                    return Err(error);
                }
            };
            // FFmpeg learns output rate/layout from the first decoded frame,
            // not from potentially incomplete codec parameters at open time.
            if self.layout.is_empty() {
                let Decode::Ffmpeg(decoder) = &self.decode else {
                    unreachable!()
                };
                self.layout.extend_from_slice(decoder.layout());
                self.info.channels = decoder.info().channels;
                self.info.sample_rate = decoder.info().sample_rate;
            }
            // Validate backend output before applying the shared frame-based crop.
            let channels = self.layout.len();
            if channels == 0 || !self.frames.len().is_multiple_of(channels) {
                bail!("FFmpeg decoder returned incomplete interleaved frames");
            }
            if self.retain_pcm(packet_start) {
                return Ok(DecodeStep::Frames(()));
            }
            if self.exhausted {
                return Ok(DecodeStep::Eof);
            }
        }
    }

    fn read_next(
        &mut self,
        cancelled: Option<&AtomicBool>,
        budget: &mut usize,
    ) -> Result<DecodeStep<()>> {
        self.read_next_with_reader(cancelled, budget, |format| format.next_packet())
    }

    fn read_next_with_reader(
        &mut self,
        cancelled: Option<&AtomicBool>,
        budget: &mut usize,
        read: impl FnMut(
            &mut dyn FormatReader,
        )
            -> symphonia::core::errors::Result<Option<symphonia::core::packet::Packet>>,
    ) -> Result<DecodeStep<()>> {
        let result = self.read_next_inner(cancelled, budget, read);
        if result.is_err() {
            self.complete_stream = false;
            self.packet_stats.invalidate();
        }
        result
    }

    fn read_next_inner(
        &mut self,
        cancelled: Option<&AtomicBool>,
        budget: &mut usize,
        mut read: impl FnMut(
            &mut dyn FormatReader,
        )
            -> symphonia::core::errors::Result<Option<symphonia::core::packet::Packet>>,
    ) -> Result<DecodeStep<()>> {
        check_cancelled(cancelled)?;
        if self.exhausted {
            return Ok(DecodeStep::Eof);
        }
        if matches!(self.decode, Decode::Ffmpeg(_)) {
            return self.read_ffmpeg_next(cancelled, budget);
        }
        loop {
            check_cancelled(cancelled)?;
            if *budget == 0 {
                return Ok(DecodeStep::Yield);
            }
            *budget -= 1;
            // Seeking can discard whole packets (including Opus preroll).
            // Never append the next packet onto samples already discarded.
            self.frames.clear();
            let format = self
                .format
                .as_mut()
                .context("Native source has no format reader")?;
            let packet = self.metadata_budget.with(|| read(format.as_mut()));
            check_cancelled(cancelled)?;
            let Some(packet) = packet? else {
                self.exhausted = true;
                self.packet_stats.finish(self.expected_samples);
                return Ok(DecodeStep::Eof);
            };
            if packet.track_id != self.track {
                continue;
            }
            let mut packet_start = seconds(
                self.time_base
                    .calc_time(packet.pts)
                    .context("Packet timestamp out of range")?,
            );
            let encoded_samples = match &mut self.decode {
                Decode::Native(decoder) => {
                    let audio = decoder.decode(&packet).map_err(native_error)?;
                    if audio.spec().rate() != self.info.sample_rate {
                        bail!("Sample rate changes within this track are unsupported");
                    }
                    update_layout(audio.spec().channels(), &mut self.layout)?;
                    self.info.channels = self.layout.len() as u16;
                    let represented = if self.mpeg_packets {
                        bitrate::mpeg_packet_samples(&packet.data, self.info.sample_rate)
                    } else {
                        trim_sample_count(
                            self.time_base,
                            packet.trim_start.get(),
                            self.info.sample_rate,
                        )
                        .and_then(|trim| (audio.frames() as u64).checked_add(trim))
                        .and_then(|samples| {
                            samples.checked_add(trim_sample_count(
                                self.time_base,
                                packet.trim_end.get(),
                                self.info.sample_rate,
                            )?)
                        })
                    };
                    interleave_audio(audio, &mut self.frames);
                    packet_start += self
                        .time_base
                        .calc_duration(packet.trim_start)
                        .context("Packet trim out of range")?
                        .as_secs_f64();
                    represented
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
                    self.frames.extend(
                        opus.samples[leading * opus.channels..end * opus.channels]
                            .iter()
                            .copied()
                            .map(finite_sample),
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
                    Some(count as u64)
                }
                Decode::Ffmpeg(_) => unreachable!(),
            };
            match encoded_samples {
                Some(samples) => self.packet_stats.add(packet.data.len() as u64, samples),
                None => self.packet_stats.invalidate(),
            }
            if self.retain_pcm(packet_start) {
                return Ok(DecodeStep::Frames(()));
            }
            if self.exhausted {
                return Ok(DecodeStep::Eof);
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

/// Decoder conversions clear invalid PCM before waveform, analysis and output.
/// Preserve every finite f32 value, including peaks above nominal full scale.
pub(super) fn finite_sample(sample: f32) -> f32 {
    if sample.is_finite() { sample } else { 0.0 }
}

fn interleave_samples<S: Sample>(
    audio: &AudioBuffer<S>,
    output: &mut Vec<f32>,
    convert: impl Fn(S) -> f32,
) {
    output.clear();
    output.reserve(audio.samples_interleaved());
    output.extend(audio.iter_interleaved().map(convert));
}

fn interleave_audio(audio: GenericAudioBufferRef<'_>, output: &mut Vec<f32>) {
    match audio {
        GenericAudioBufferRef::U8(audio) => interleave_samples(audio, output, f32::from_sample),
        GenericAudioBufferRef::U16(audio) => interleave_samples(audio, output, f32::from_sample),
        GenericAudioBufferRef::U24(audio) => interleave_samples(audio, output, f32::from_sample),
        GenericAudioBufferRef::U32(audio) => interleave_samples(audio, output, f32::from_sample),
        GenericAudioBufferRef::S8(audio) => interleave_samples(audio, output, f32::from_sample),
        GenericAudioBufferRef::S16(audio) => interleave_samples(audio, output, f32::from_sample),
        GenericAudioBufferRef::S24(audio) => interleave_samples(audio, output, f32::from_sample),
        GenericAudioBufferRef::S32(audio) => interleave_samples(audio, output, f32::from_sample),
        GenericAudioBufferRef::F32(audio) => interleave_samples(audio, output, finite_sample),
        GenericAudioBufferRef::F64(audio) => interleave_samples(audio, output, |sample| {
            finite_sample(f32::from_sample(sample))
        }),
    }
}

/// A real Matroska packet prefix on an unselected audio track. The selected
/// track's first PCM is deliberately beyond several opening work rounds.
#[cfg(test)]
pub(super) fn opening_prefix_file(
    prefix_packets: usize,
    selected_samples: bool,
) -> tempfile::NamedTempFile {
    use std::io::Write;

    fn element(id: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut bytes = id.to_vec();
        bytes.extend_from_slice(&((1_u64 << 56) | payload.len() as u64).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn track(number: u8) -> Vec<u8> {
        let mut entry = element(&[0xd7], &[number]);
        entry.extend(element(&[0x73, 0xc5], &[number]));
        entry.extend(element(&[0x83], &[2]));
        entry.extend(element(&[0x88], &[u8::from(number == 1)]));
        entry.extend(element(&[0x86], b"A_PCM/INT/LIT"));
        let mut audio = element(&[0xb5], &8000_f64.to_be_bytes());
        audio.extend(element(&[0x9f], &[1]));
        audio.extend(element(&[0x62, 0x64], &[16]));
        entry.extend(element(&[0xe1], &audio));
        element(&[0xae], &entry)
    }

    let mut header = element(&[0x42, 0x82], b"matroska");
    header.extend(element(&[0x42, 0x87], &[4]));
    header.extend(element(&[0x42, 0x85], &[2]));
    let mut bytes = element(&[0x1a, 0x45, 0xdf, 0xa3], &header);
    let mut info = element(&[0x2a, 0xd7, 0xb1], &1_000_000_u32.to_be_bytes());
    info.extend(element(&[0x4d, 0x80], b"rivu regression"));
    info.extend(element(&[0x57, 0x41], b"rivu regression"));
    let mut segment = element(&[0x15, 0x49, 0xa9, 0x66], &info);
    let mut tracks = track(1);
    tracks.extend(track(2));
    segment.extend(element(&[0x16, 0x54, 0xae, 0x6b], &tracks));
    let mut cluster = element(&[0xe7], &[0]);
    for _ in 0..prefix_packets {
        cluster.extend(element(&[0xa3], &[0x82, 0, 0, 0x80, 0, 0]));
    }
    if selected_samples {
        cluster.extend(element(&[0xa3], &[0x81, 0, 0, 0x80, 0, 0x40, 0, 0xc0]));
    }
    segment.extend(element(&[0x1f, 0x43, 0xb6, 0x75], &cluster));
    bytes.extend(element(&[0x18, 0x53, 0x80, 0x67], &segment));
    let mut file = tempfile::Builder::new().suffix(".mka").tempfile().unwrap();
    file.write_all(&bytes).unwrap();
    file
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use symphonia::core::{checksum::Crc32, io::Monitor};

    #[test]
    fn opening_rounds_keep_real_packet_progress_and_the_first_pcm() {
        let file = opening_prefix_file(DECODE_WORK_BUDGET * 3 + 1, true);
        let mut source =
            Source::begin_open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        for _ in 0..3 {
            assert!(!source.poll_open().unwrap());
        }
        assert!(source.poll_open().unwrap());
        assert!(
            source.poll_open().unwrap(),
            "completion must not consume primed PCM"
        );
        assert_eq!(source.info.sample_rate, 8000);
        assert_eq!(source.info.channels, 1);
        assert_eq!(decode_remaining(&mut source), [0.5, -0.5]);
    }

    #[test]
    fn opening_an_empty_selected_stream_fails_after_finite_prefix_rounds() {
        let file = opening_prefix_file(DECODE_WORK_BUDGET * 2, false);
        let mut source =
            Source::begin_open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        assert!(!source.poll_open().unwrap());
        assert!(!source.poll_open().unwrap());
        assert!(source.poll_open().is_err());
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn ffmpeg_opening_rounds_preserve_the_first_selected_pcm() {
        let file = opening_prefix_file(DECODE_WORK_BUDGET * 3 + 1, true);
        let mut source =
            Source::begin_open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Ffmpeg, true).unwrap();
        let mut rounds = 0;
        while !source.poll_open().unwrap() {
            rounds += 1;
            assert!(rounds <= 8, "opening must retain decoder progress");
        }
        assert!(rounds >= 3, "the real prefix must cross work rounds");
        assert_eq!(source.info.sample_rate, 8000);
        assert_eq!(source.info.channels, 1);
        assert_eq!(decode_remaining(&mut source), [0.5, -0.5]);
    }

    #[test]
    fn native_metadata_budget_preserves_unsynchronised_audio() {
        let directory = tempfile::tempdir().unwrap();
        for (version, tag_wide) in [(2, true), (3, true), (4, false), (4, true)] {
            let path = directory
                .path()
                .join(format!("unsync-{version}-{tag_wide}.wav"));
            crate::artwork::tests::budget_unsynchronised_wav(&path, version, tag_wide);
            assert_eq!(probe(&path, false).unwrap().sample_rate, 8_000);
            let mut source =
                Source::open(&path, PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
            assert_eq!(decode_remaining(&mut source), [0.5]);
        }
    }

    fn id3_frame_file(frame_id: &[u8; 4], payload: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        file.write_all(b"ID3\x03\0\0").unwrap();
        let size = payload.len() + 10;
        for shift in [21, 14, 7, 0] {
            file.write_all(&[((size >> shift) & 0x7f) as u8]).unwrap();
        }
        file.write_all(frame_id).unwrap();
        file.write_all(&(payload.len() as u32).to_be_bytes())
            .unwrap();
        file.write_all(&[0, 0]).unwrap();
        file.write_all(payload).unwrap();
        file.write_all(b"RIFF\x26\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0data\x02\0\0\0\0\x40").unwrap();
        file
    }

    #[test]
    fn native_metadata_budget_limits_text_fanout_without_failing_audio() {
        let file = id3_frame_file(b"TIT2", &vec![0; 256 * 1024]);
        assert_eq!(probe(file.path(), false).unwrap().sample_rate, 8_000);
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        assert_eq!(decode_remaining(&mut source), [0.5]);
    }

    #[test]
    fn native_metadata_budget_limits_nested_chapters_without_failing_audio() {
        let mut payload = Vec::new();
        for _ in 0..64 {
            let mut chapter = b"chapter\0".to_vec();
            chapter.extend_from_slice(&[0; 16]);
            chapter.extend_from_slice(&payload);
            payload = b"CHAP".to_vec();
            payload.extend_from_slice(&(chapter.len() as u32).to_be_bytes());
            payload.extend_from_slice(&[0, 0]);
            payload.extend_from_slice(&chapter);
        }
        let file = id3_frame_file(b"CHAP", &payload[10..]);
        assert_eq!(probe(file.path(), false).unwrap().sample_rate, 8_000);
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        assert_eq!(decode_remaining(&mut source), [0.5]);
    }

    #[test]
    fn native_metadata_budget_skips_large_pictures_without_losing_tags_or_audio() {
        let directory = tempfile::tempdir().unwrap();
        for version in [2, 3, 4] {
            let path = directory.path().join(format!("budget-{version}.wav"));
            let size = if version == 2 { 9 } else { 27 } * BYTES_PER_MEBIBYTE;
            crate::artwork::tests::budget_tagged_wav(
                &path,
                version,
                &[
                    (crate::artwork::DEFAULT_IMAGE, size, 3),
                    (
                        crate::artwork::DEFAULT_IMAGE,
                        crate::artwork::DEFAULT_IMAGE.len(),
                        3,
                    ),
                ],
            );
            let info = probe(&path, false).unwrap();
            assert_eq!(info.title, "Budget title");
            assert_eq!(info.sample_rate, 8_000);
            let mut source =
                Source::open(&path, PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
            assert!(!source.uses_ffmpeg());
            assert_eq!(decode_remaining(&mut source), [0.5]);
        }
    }

    #[test]
    fn native_metadata_budget_keeps_audio_after_many_picture_frames() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("many-pictures.wav");
        let pictures = vec![
            (
                crate::artwork::DEFAULT_IMAGE,
                crate::artwork::DEFAULT_IMAGE.len(),
                3
            );
            64
        ];
        crate::artwork::tests::budget_tagged_wav(&path, 4, &pictures);
        let mut source = Source::open(&path, PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        assert_eq!(source.info.title, "Budget title");
        assert_eq!(decode_remaining(&mut source), [0.5]);
    }

    #[test]
    fn native_interleave_reuses_storage_and_replaces_previous_packet() {
        use symphonia::core::audio::AudioSpec;
        let mut audio = AudioBuffer::<i16>::new(AudioSpec::new(48_000, Channels::Discrete(2)), 3);
        audio.resize(1, &[16_384, -16_384]);
        audio.resize(3, &[8_192, -8_192]);
        let mut output = Vec::with_capacity(32);
        let allocation = output.as_ptr();
        interleave_audio(GenericAudioBufferRef::S16(&audio), &mut output);
        assert_eq!(output, [0.5, -0.5, 0.25, -0.25, 0.25, -0.25]);
        audio.resize(1, &[0, 0]);
        interleave_audio(GenericAudioBufferRef::S16(&audio), &mut output);
        assert_eq!(output, [0.5, -0.5]);
        audio.clear();
        interleave_audio(GenericAudioBufferRef::S16(&audio), &mut output);
        assert!(output.is_empty());
        assert_eq!(output.as_ptr(), allocation);
    }

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

    #[test]
    fn native_complete_stream_upgrades_estimate_from_only_encoded_audio_payload() {
        let file = opus_file();
        // Independently inspect this deterministic fixture's one-packet pages:
        // exclude OpusHead/OpusTags and all Ogg framing bytes.
        let bytes = std::fs::read(file.path()).unwrap();
        let mut offset = 0;
        let mut audio_bytes = 0_u64;
        let mut audio_packets = 0_u64;
        while offset < bytes.len() {
            assert_eq!(&bytes[offset..offset + 4], b"OggS");
            let segments = bytes[offset + 26] as usize;
            let payload = bytes[offset + 27..offset + 27 + segments]
                .iter()
                .map(|size| usize::from(*size))
                .sum::<usize>();
            let sequence = u32::from_le_bytes(bytes[offset + 18..offset + 22].try_into().unwrap());
            if sequence >= 2 {
                audio_bytes += payload as u64;
                audio_packets += 1;
            }
            offset += 27 + segments + payload;
        }
        let expected = (audio_bytes * 8 * 48_000 + audio_packets * 960 / 2) / (audio_packets * 960);
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        source.info.bitrate_bps = Some(128_000); // A cached scan estimate.
        source.info.bitrate_exact = false;
        assert_eq!(
            source.completed_bitrate(),
            None,
            "primed prefix is not complete"
        );
        assert_eq!(decode_remaining(&mut source).len(), 96_000);
        assert_eq!(source.completed_bitrate(), Some(expected));
        assert_ne!(expected, 128_000);
        assert!(
            !source.info.bitrate_exact,
            "only the completion consumer upgrades metadata"
        );
    }

    fn silent_mpeg_frame(index: u8) -> Vec<u8> {
        let kbps = [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ][index as usize];
        let mut frame = vec![0; 144 * kbps * 1000 / 44_100];
        frame[..4].copy_from_slice(&[0xff, 0xfb, index << 4, 0]);
        // Zero side information/main data describes valid silent granules.
        frame
    }

    fn native_mpeg_file(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new().suffix(".mp3").tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file
    }

    #[test]
    fn native_unmarked_vbr_counts_frames_despite_excessive_demux_trim() {
        let mut bytes = Vec::new();
        for index in std::iter::repeat_n(14, 20).chain(std::iter::repeat_n(1, 80)) {
            bytes.extend_from_slice(&silent_mpeg_frame(index));
        }
        let file = native_mpeg_file(&bytes);
        // Exercise the actual demuxer's bad estimate, not a fabricated trim:
        // its high-rate prefix causes trim requests larger than whole frames.
        let stream = MediaSourceStream::new(
            Box::new(File::open(file.path()).unwrap()),
            MediaSourceStreamOptions::default(),
        );
        let mut format = get_probe()
            .probe(
                &Hint::new(),
                stream,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .unwrap();
        let mut excessive_trim = false;
        while let Some(packet) = format.next_packet().unwrap() {
            excessive_trim |= packet.trim_end.get() > 1152;
        }
        assert!(
            excessive_trim,
            "fixture must exercise uncapped estimated trim"
        );
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        assert_eq!(source.info.bitrate_bps, Some(320_000));
        assert_eq!(
            source.expected_samples, None,
            "estimated MPEG counts are not authority"
        );
        decode_remaining(&mut source);
        let samples = 100 * 1152;
        let expected = (bytes.len() as u64 * 8 * 44_100 + samples / 2) / samples;
        assert_eq!(source.completed_bitrate(), Some(expected));
        assert_ne!(expected, 320_000);
    }

    #[test]
    fn native_mpeg_summary_authority_survives_truncated_scan_estimate() {
        for id in [b"Xing", b"Info"] {
            let mut tag = silent_mpeg_frame(11);
            let audio = silent_mpeg_frame(11).repeat(8);
            tag[36..40].copy_from_slice(id);
            tag[40..44].copy_from_slice(&3_u32.to_be_bytes());
            tag[44..48].copy_from_slice(&8_u32.to_be_bytes());
            let size = (tag.len() + audio.len()) as u32;
            tag[48..52].copy_from_slice(&size.to_be_bytes());
            tag.extend_from_slice(&audio);
            for (truncate, declared_bytes, extra_byte) in [
                (false, true, false),
                (true, true, false),
                (true, false, false),
                (false, true, true),
            ] {
                let mut bytes = tag.clone();
                if !declared_bytes {
                    bytes[40..44].copy_from_slice(&1_u32.to_be_bytes());
                }
                if truncate {
                    bytes.truncate(bytes.len() - 200);
                }
                if extra_byte {
                    bytes.push(0);
                }
                let file = native_mpeg_file(&bytes);
                let mut source =
                    Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false)
                        .unwrap();
                assert_eq!(source.expected_samples, Some(8 * 1152));
                if (truncate || extra_byte) && declared_bytes {
                    assert_eq!(source.info.bitrate_bps, None);
                }
                decode_remaining(&mut source);
                let expected = if truncate || extra_byte {
                    None
                } else {
                    let samples = 8 * 1152;
                    Some((audio.len() as u64 * 8 * 44_100 + samples / 2) / samples)
                };
                assert_eq!(
                    source.completed_bitrate(),
                    expected,
                    "{id:?}, truncated={truncate}"
                );
            }
        }
    }

    #[test]
    fn native_vbri_skipped_audio_frame_is_not_complete_measurement() {
        let mut bytes = silent_mpeg_frame(9).repeat(3);
        let size = bytes.len() as u32;
        bytes[36..40].copy_from_slice(b"VBRI");
        bytes[40..42].copy_from_slice(&1_u16.to_be_bytes());
        bytes[46..50].copy_from_slice(&size.to_be_bytes());
        bytes[50..54].copy_from_slice(&3_u32.to_be_bytes());
        bytes[54..56].copy_from_slice(&1_u16.to_be_bytes());
        bytes[56..58].copy_from_slice(&1_u16.to_be_bytes());
        bytes[58..60].copy_from_slice(&4_u16.to_be_bytes());
        bytes[60..62].copy_from_slice(&3_u16.to_be_bytes());
        bytes[62..66].copy_from_slice(&size.to_be_bytes());
        let file = native_mpeg_file(&bytes);
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        assert_eq!(source.expected_samples, Some(3 * 1152));
        // Symphonia currently consumes the VBRI-bearing first audio frame as a
        // tag. Do not pretend its bytes/samples were successfully decoded.
        assert_eq!(decode_remaining(&mut source).len(), 2 * 1152 * 2);
        assert_eq!(source.completed_bitrate(), None);
    }

    #[test]
    fn seeking_ranges_and_failed_decode_never_publish_complete_statistics() {
        let file = opus_file();
        for seek in [0.0, 0.5] {
            let mut source =
                Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
            source.seek(seek).unwrap();
            decode_remaining(&mut source);
            assert_eq!(source.completed_bitrate(), None);
        }
        for end in [None, Some(0.5)] {
            let mut source =
                Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
            source
                .restrict(super::super::PlaybackRange {
                    start_seconds: 0.0,
                    end_seconds: end,
                })
                .unwrap();
            decode_remaining(&mut source);
            assert_eq!(source.completed_bitrate(), None);
        }
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        let mut budget = 64;
        assert!(
            source
                .read_next_with_reader(None, &mut budget, |format| {
                    let mut packet = format.next_packet()?.unwrap();
                    packet.data = Box::from([0xff]); // Opus code 3 without its frame-count byte.
                    Ok(Some(packet))
                })
                .is_err()
        );
        decode_remaining(&mut source);
        assert_eq!(source.completed_bitrate(), None);
    }

    #[test]
    fn native_other_stream_packets_do_not_change_selected_audio_average() {
        let file = pcm_file();
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        let mut injected = false;
        loop {
            let mut budget = 64;
            let step = source
                .read_next_with_reader(None, &mut budget, |format| {
                    if !injected {
                        injected = true;
                        return Ok(Some(symphonia::core::packet::Packet::new(
                            u32::MAX,
                            symphonia::core::units::Timestamp::ZERO,
                            symphonia::core::units::Duration::new(1),
                            vec![0; 100_000],
                        )));
                    }
                    format.next_packet()
                })
                .unwrap();
            if step == DecodeStep::Eof {
                break;
            }
        }
        assert_eq!(source.completed_bitrate(), Some(705_600));
    }

    #[track_caller]
    fn decode_remaining(source: &mut Source) -> Vec<f32> {
        let mut samples = Vec::new();
        while let Some((frames, _)) = source.next_frames().unwrap() {
            samples.extend_from_slice(frames);
        }
        samples
    }

    #[test]
    fn opus_seek_discards_whole_preroll_packets_without_reusing_pcm() {
        let file = opus_file();
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
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
    fn seek_to_explicit_segment_end_is_exhausted_and_can_seek_back() {
        let file = opus_file();
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
        source
            .restrict(super::super::PlaybackRange {
                start_seconds: 0.0,
                end_seconds: source.info().duration,
            })
            .unwrap();
        let duration = source.info().duration.unwrap();
        for target in [duration, duration + 1.0] {
            source.seek(target).unwrap();
            assert!(source.next_frames().unwrap().is_none());
            assert!(source.next_frames().unwrap().is_none());
            source.seek(0.0).unwrap();
            assert_eq!(decode_remaining(&mut source).len(), 96_000);
        }
    }

    // Real fixed-block FLAC frames, without an encoder process or binary fixture.
    // Each one-second mono block has its own constant value so seek alignment is
    // visible. The declared sample count can deliberately underestimate the data.
    fn constant_flac_file(reported_samples: u64, blocks: u8) -> tempfile::NamedTempFile {
        use symphonia::core::checksum::{Crc8Ccitt, Crc16Ansi};

        let mut file = tempfile::NamedTempFile::new().unwrap();
        let mut info = [0; 34];
        info[..2].copy_from_slice(&8000_u16.to_be_bytes());
        info[2..4].copy_from_slice(&8000_u16.to_be_bytes());
        info[10..18]
            .copy_from_slice(&((8000_u64 << 44) | (15 << 36) | reported_samples).to_be_bytes());
        file.write_all(b"fLaC\x80\0\0\x22").unwrap();
        file.write_all(&info).unwrap();
        for number in 0..blocks {
            // Fixed blocking, explicit 16-bit block length, 8 kHz, mono, 16 bits.
            let mut frame = vec![0xff, 0xf8, 0x74, 0x08, number];
            frame.extend_from_slice(&7999_u16.to_be_bytes());
            let mut crc8 = Crc8Ccitt::new(0);
            crc8.process_buf_bytes(&frame);
            frame.push(crc8.crc());
            frame.push(0); // Constant subframe, no wasted bits.
            frame.extend_from_slice(&(i16::from(number) * 1000 - 5000).to_be_bytes());
            let mut crc16 = Crc16Ansi::new(0);
            crc16.process_buf_bytes(&frame);
            frame.extend_from_slice(&crc16.crc().to_be_bytes());
            file.write_all(&frame).unwrap();
        }
        file
    }

    #[test]
    fn native_flac_physical_end_seeks_reach_eof_and_can_seek_back() {
        let file = constant_flac_file(96_000, 12);
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        assert_eq!(source.info().duration, Some(12.0));
        let reference = decode_remaining(&mut source);
        assert_eq!(reference.len(), 96_000);
        for target in [12.0, 13.0] {
            source.seek(target).unwrap();
            assert!(source.next_frames().unwrap().is_none());
            assert!(source.next_frames().unwrap().is_none());
            assert_eq!(source.completed_bitrate(), None);
            for restart in [0.0, 5.25, 12.0 - 1.0 / 8000.0] {
                source.seek(restart).unwrap();
                let offset = (restart * 8000.0).round() as usize;
                assert_eq!(decode_remaining(&mut source), reference[offset..]);
            }
        }
    }

    #[test]
    fn native_flac_underestimated_duration_keeps_seek_and_cue_samples() {
        let file = constant_flac_file(64_000, 12);
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        assert_eq!(source.info().duration, Some(8.0));
        let reference = decode_remaining(&mut source);
        assert_eq!(reference.len(), 96_000);
        for target in [7.5, 8.0, 9.25, 12.0, 13.0] {
            source.seek(target).unwrap();
            let offset = ((target * 8000.0).round() as usize).min(reference.len());
            assert_eq!(decode_remaining(&mut source), reference[offset..]);
        }
        for end_seconds in [Some(11.0), None] {
            let mut source =
                Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
            source
                .restrict(super::super::PlaybackRange {
                    start_seconds: 9.0,
                    end_seconds,
                })
                .unwrap();
            let end = (end_seconds.unwrap_or(12.0) * 8000.0) as usize;
            assert_eq!(decode_remaining(&mut source), reference[72_000..end]);
            source.seek(end_seconds.unwrap_or(12.0) - 9.0).unwrap();
            assert!(source.next_frames().unwrap().is_none());
            source.seek(0.5).unwrap();
            assert_eq!(decode_remaining(&mut source), reference[76_000..end]);
        }
    }

    #[test]
    fn native_flac_end_seek_does_not_hide_truncated_audio() {
        let file = constant_flac_file(96_000, 11);
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        // The advertised final sample is absent, not merely an exclusive endpoint.
        assert!(source.seek(12.0).is_err());
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn empty_ffmpeg_opening_fails_without_an_unbounded_prime() {
        let file = opening_prefix_file(DECODE_WORK_BUDGET * 3 + 1, false);
        let mut source =
            Source::begin_open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Ffmpeg, true).unwrap();
        for _ in 0..8 {
            match source.poll_open() {
                Ok(false) => {}
                Ok(true) => panic!("an empty selected stream must not finish opening"),
                Err(_) => return,
            }
        }
        panic!("finite packet input must reach its opening failure");
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

    fn assert_incremental_pcm_start(hint: DecoderHint, ffmpeg_enabled: bool) {
        let file = pcm_file();
        let mut reference =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        let expected = decode_remaining(&mut reference);
        let mut source =
            Source::begin_open(file.path(), PROBE_BUFFER_LEN, hint, ffmpeg_enabled).unwrap();
        while !source.poll_open().unwrap() {}
        assert_eq!(
            decode_remaining(&mut source),
            expected,
            "first-frame PCM must not be lost"
        );

        let mut source =
            Source::begin_open(file.path(), PROBE_BUFFER_LEN, hint, ffmpeg_enabled).unwrap();
        while !source.poll_open().unwrap() {}
        source
            .restrict(super::super::PlaybackRange {
                start_seconds: 0.4,
                end_seconds: Some(1.6),
            })
            .unwrap();
        source.seek(0.2).unwrap();
        let (first, timestamp) = source.next_frames().unwrap().unwrap();
        assert!((timestamp - 0.2).abs() < 1e-9);
        let mut actual = first.to_vec();
        actual.extend(decode_remaining(&mut source));
        assert_eq!(actual, expected[26_460..70_560]);
    }

    #[test]
    fn incremental_native_open_preserves_pcm_range_and_start_seek() {
        assert_incremental_pcm_start(DecoderHint::Native, false);
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn incremental_ffmpeg_open_preserves_pcm_range_and_start_seek() {
        assert_incremental_pcm_start(DecoderHint::Ffmpeg, true);
    }

    #[test]
    fn stereo_ranges_preserve_channel_pairs_and_sample_accurate_seek_timestamps() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let rate = 8_000_u32;
        let count = 8_192_u32;
        let bytes = count * 4;
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + bytes).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt \x10\0\0\0\x01\0\x02\0").unwrap();
        file.write_all(&rate.to_le_bytes()).unwrap();
        file.write_all(&(rate * 4).to_le_bytes()).unwrap();
        file.write_all(b"\x04\0\x10\0data").unwrap();
        file.write_all(&bytes.to_le_bytes()).unwrap();
        let mut expected = Vec::with_capacity(count as usize * 2);
        for value in 1..=count as i16 {
            for sample in [value, -value] {
                file.write_all(&sample.to_le_bytes()).unwrap();
                expected.push(f32::from(sample) / 32_768.0);
            }
        }
        for hint in [
            DecoderHint::Native,
            #[cfg(feature = "ffmpeg")]
            DecoderHint::Ffmpeg,
        ] {
            for (start, end) in [(257, 6_147), (4_095, 4_096)] {
                let mut source = Source::open(file.path(), PROBE_BUFFER_LEN, hint, true).unwrap();
                source
                    .restrict(super::super::PlaybackRange {
                        start_seconds: start as f64 / f64::from(rate),
                        end_seconds: Some(end as f64 / f64::from(rate)),
                    })
                    .unwrap();
                for offset in [0, 1, end - start, 0] {
                    source.seek(offset as f64 / f64::from(rate)).unwrap();
                    let mut consumed = 0;
                    let expected = &expected[(start + offset) * 2..end * 2];
                    while let Some((samples, timestamp)) = source.next_frames().unwrap() {
                        assert_eq!(samples.len() % 2, 0);
                        assert!(!samples.is_empty());
                        let time = (offset + consumed / 2) as f64 / f64::from(rate);
                        assert!((timestamp - time).abs() < 1e-9);
                        assert_eq!(samples, &expected[consumed..consumed + samples.len()]);
                        consumed += samples.len();
                    }
                    assert_eq!(consumed, expected.len());
                    assert_eq!(source.completed_bitrate(), None);
                }
            }
        }
    }

    #[test]
    fn audio_entries_reject_writerless_fifo_and_fifo_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("audio.wav");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is a live NUL-terminated path; mkfifo retains no pointer.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let alias = directory.path().join("alias.wav");
        std::os::unix::fs::symlink(&fifo, &alias).unwrap();
        let (completed, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let cancelled = AtomicBool::new(false);
            for path in [&fifo, &alias] {
                for enabled in [false, true] {
                    for hint in [DecoderHint::Auto, DecoderHint::Native, DecoderHint::Ffmpeg] {
                        if hint == DecoderHint::Ffmpeg && (!enabled || !cfg!(feature = "ffmpeg")) {
                            continue;
                        }
                        let error = Source::open(path, PROBE_BUFFER_LEN, hint, enabled)
                            .err()
                            .expect("A FIFO is not an audio source");
                        assert!(format!("{error:#}").contains("not an ordinary file"));
                        let error =
                            Source::waveform(path, None, hint, 2, enabled, &cancelled).unwrap_err();
                        assert!(format!("{error:#}").contains("not an ordinary file"));
                    }
                    let error = probe(path, enabled).err().unwrap();
                    assert!(format!("{error:#}").contains("not an ordinary file"));
                    let error = probe_details(path, enabled, false).err().unwrap();
                    assert!(format!("{error:#}").contains("not an ordinary file"));
                }
                #[cfg(feature = "ffmpeg")]
                {
                    let error = probe_details(path, true, true).err().unwrap();
                    assert!(format!("{error:#}").contains("not an ordinary file"));
                }
            }
            completed.send(()).unwrap();
        });
        // Do not join on timeout: a regression must fail rather than hang the suite.
        receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn regular_symlink_preserves_native_ffmpeg_and_waveform_source() {
        let file = pcm_file();
        let directory = tempfile::tempdir().unwrap();
        let alias = directory.path().join("alias.wav");
        std::os::unix::fs::symlink(file.path(), &alias).unwrap();
        let identity = SourceIdentity::from_file(file.as_file()).unwrap();
        let mut reference =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        let expected = decode_remaining(&mut reference);
        let cancelled = AtomicBool::new(false);
        for enabled in [false, true] {
            for hint in [DecoderHint::Auto, DecoderHint::Native, DecoderHint::Ffmpeg] {
                if hint == DecoderHint::Ffmpeg && (!enabled || !cfg!(feature = "ffmpeg")) {
                    continue;
                }
                let mut source = Source::open(&alias, PROBE_BUFFER_LEN, hint, enabled).unwrap();
                assert_eq!(source.identity(), identity);
                assert_eq!(decode_remaining(&mut source), expected);
                let frame = Source::waveform(&alias, None, hint, 2, enabled, &cancelled).unwrap();
                assert!(frame.complete);
                assert_eq!(frame.duration, Some(2.0));
            }
        }
    }

    #[test]
    fn underestimated_metadata_does_not_limit_seek_cue_or_completed_waveform() {
        let file = pcm_file();
        let cancelled = AtomicBool::new(false);
        for hint in [DecoderHint::Native, DecoderHint::Ffmpeg] {
            if hint == DecoderHint::Ffmpeg && !cfg!(feature = "ffmpeg") {
                continue;
            }
            let mut source = Source::open(file.path(), PROBE_BUFFER_LEN, hint, true).unwrap();
            source.info.duration = Some(0.25);
            source.seek(1.0).unwrap();
            assert_eq!(decode_remaining(&mut source).len(), 44_100);
            source.seek(0.0).unwrap();
            let frame = source
                .scan_waveform(file.path(), None, hint, &cancelled)
                .unwrap();
            assert!(frame.complete);
            assert_eq!(frame.duration, Some(2.0));
            assert!(frame.span_seconds >= 2.0);
            source.info.duration = Some(0.25);
            source
                .restrict(super::super::PlaybackRange {
                    start_seconds: 0.5,
                    end_seconds: Some(1.5),
                })
                .unwrap();
            assert_eq!(decode_remaining(&mut source).len(), 44_100);
        }
    }

    #[test]
    fn native_skipped_packets_yield_without_cancellation_or_eof() {
        let file = pcm_file();
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        source.seek(0.0).unwrap();
        let mut budget = 7;
        let mut packets = 0;
        let step = source
            .read_next_with_reader(None, &mut budget, |format| {
                let mut packet = format.next_packet()?.expect("fixture has more packets");
                packet.track_id = packet.track_id.wrapping_add(1);
                packets += 1;
                Ok(Some(packet))
            })
            .unwrap();
        assert_eq!(step, DecodeStep::Yield);
        assert_eq!(packets, 7);
        assert_eq!(budget, 0);
        assert!(!source.exhausted);
        source.seek(0.0).unwrap();
        let cancelled = AtomicBool::new(false);
        let frame = source
            .scan_waveform(file.path(), None, DecoderHint::Native, &cancelled)
            .unwrap();
        assert!(frame.complete);
        assert_eq!(frame.duration, Some(2.0));
    }

    fn variable_adts_file() -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new().suffix(".aac").tempfile().unwrap();
        // A zero-band mono SCE followed by ID_END is valid AAC-LC silence.
        // Padding the first hundred frames biases ADTS's sampled bitrate:
        // those frames occupy all four sample points, followed by a long tail
        // of small frames that the estimate never inspects.
        for index in 0..1100 {
            let length = if index < 100 { 2000_usize } else { 11 };
            let header = [
                0xff,
                0xf1,
                0x50,
                0x40 | ((length >> 11) as u8),
                (length >> 3) as u8,
                ((length & 7) as u8) << 5 | 0x1f,
                0xfc,
            ];
            file.write_all(&header).unwrap();
            file.write_all(&[0, 0, 0, 7]).unwrap();
            file.write_all(&vec![0; length - 11]).unwrap();
        }
        file
    }

    #[test]
    fn sampled_adts_duration_never_truncates_decoding_or_full_waveform() {
        let file = variable_adts_file();
        let actual_frames = 1100 * 1024;
        let actual_duration = actual_frames as f64 / 44_100.0;
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        assert!(source.info.duration.unwrap() < actual_duration / 2.0);
        assert_eq!(decode_remaining(&mut source).len(), actual_frames);
        let cancelled = AtomicBool::new(false);
        let waveform =
            Source::waveform(file.path(), None, DecoderHint::Native, 2, false, &cancelled).unwrap();
        assert!(waveform.complete);
        assert_eq!(waveform.duration, Some(actual_duration));
        assert!(waveform.span_seconds >= actual_duration);
        assert!(waveform.peaks.len() <= 1024);
        source
            .restrict(super::super::PlaybackRange {
                start_seconds: 3.0,
                end_seconds: Some(4.0),
            })
            .unwrap();
        assert_eq!(decode_remaining(&mut source).len(), 44_100);
        source.seek(0.5).unwrap();
        assert_eq!(decode_remaining(&mut source).len(), 22_050);
        let details = probe_details(file.path(), false, false).unwrap();
        assert!(matches!(details.decoder_backend, ProbeBackend::Native));
        assert!(details.info.duration.unwrap() < actual_duration / 2.0);
    }

    fn aac_packet_file(element: u8) -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new().suffix(".aac").tempfile().unwrap();
        // MPEG-4 ADTS, AAC-LC, 44.1 kHz mono, 16-byte frames. The raw AAC
        // element is deliberately unsupported or truncated, not a probe error.
        for _ in 0..8 {
            file.write_all(&[0xff, 0xf1, 0x50, 0x40, 0x02, 0x1f, 0xfc])
                .unwrap();
            file.write_all(&[element, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        }
        file
    }

    #[test]
    fn opening_fallback_only_follows_native_unsupported_errors() {
        let unsupported = aac_packet_file(0x40); // ID_CCE, unsupported by native AAC.
        let mut native = Source::begin_open(
            unsupported.path(),
            PROBE_BUFFER_LEN,
            DecoderHint::Native,
            true,
        )
        .unwrap();
        assert!(native.poll_open().unwrap_err().is::<UnsupportedNative>());

        let mut automatic = Source::begin_open(
            unsupported.path(),
            PROBE_BUFFER_LEN,
            DecoderHint::Auto,
            true,
        )
        .unwrap();
        assert!(!automatic.uses_ffmpeg());
        #[cfg(feature = "ffmpeg")]
        {
            assert!(!automatic.poll_open().unwrap());
            assert!(automatic.uses_ffmpeg());
        }
        #[cfg(not(feature = "ffmpeg"))]
        {
            assert!(automatic.poll_open().is_err());
            assert!(!automatic.uses_ffmpeg());
        }

        let corrupt = aac_packet_file(0);
        let mut source =
            Source::begin_open(corrupt.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, true).unwrap();
        assert!(!source.poll_open().unwrap_err().is::<UnsupportedNative>());
        assert!(!source.uses_ffmpeg());

        let mut source = Source::begin_open(
            unsupported.path(),
            PROBE_BUFFER_LEN,
            DecoderHint::Auto,
            true,
        )
        .unwrap();
        let cancelled = AtomicBool::new(true);
        assert!(source.poll_open_with_cancel(Some(&cancelled)).is_err());
        assert!(!source.uses_ffmpeg());
        #[cfg(feature = "ffmpeg")]
        {
            // Cancellation did not consume the native packet or initiate fallback.
            assert!(!source.poll_open().unwrap());
            assert!(source.uses_ffmpeg());
        }
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn opening_fallback_keeps_the_opened_object_after_path_replacement() {
        let unsupported = aac_packet_file(0x40);
        let replacement = opus_file();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.aac");
        let next = directory.path().join("replacement.opus");
        std::fs::copy(unsupported.path(), &path).unwrap();
        let file = open_regular_file(&path).unwrap();
        std::fs::copy(replacement.path(), &next).unwrap();
        std::fs::rename(next, &path).unwrap();
        // Replacing the pathname also updates the unlinked original's ctime.
        let original = SourceIdentity::from_file(&file).unwrap();
        let replacement = SourceIdentity::from_file(&open_regular_file(&path).unwrap()).unwrap();
        assert_ne!(original, replacement);
        let mut source = Source::open_file_unprimed(
            &path,
            file,
            PROBE_BUFFER_LEN,
            DecoderHint::Auto,
            true,
            None,
        )
        .unwrap();
        assert!(!source.poll_open().unwrap());
        assert!(source.uses_ffmpeg());
        assert_eq!(source.identity(), original);
    }

    #[test]
    fn decoder_hints_respect_global_disable_and_keep_native_runtime_cold() {
        const CHILD: &str = "RIVU_DECODER_HINTS_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "audio::source::tests::decoder_hints_respect_global_disable_and_keep_native_runtime_cold",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let file = pcm_file();
        let cancelled = AtomicBool::new(false);
        assert!(!super::super::ffmpeg::load_attempted());
        assert!(Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Ffmpeg, false).is_err());
        assert!(
            Source::waveform(file.path(), None, DecoderHint::Ffmpeg, 2, false, &cancelled).is_err()
        );
        assert!(!super::super::ffmpeg::load_attempted());

        let mut reference =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, false).unwrap();
        let expected = decode_remaining(&mut reference);
        let mut native =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, true).unwrap();
        assert!(!native.uses_ffmpeg());
        assert!(matches!(native.probe_backend, ProbeBackend::Native));
        assert_eq!(decode_remaining(&mut native), expected);
        native.seek(0.0).unwrap();
        assert!(!native.uses_ffmpeg());
        assert_eq!(decode_remaining(&mut native), expected);
        let envelope =
            Source::waveform(file.path(), None, DecoderHint::Native, 2, true, &cancelled).unwrap();
        assert!(envelope.complete);

        let mut unsupported = tempfile::NamedTempFile::new().unwrap();
        unsupported.write_all(&[0; 4096]).unwrap();
        let error = Source::open(
            unsupported.path(),
            PROBE_BUFFER_LEN,
            DecoderHint::Native,
            true,
        )
        .err()
        .expect("Native must reject an unsupported container without falling back");
        assert!(error.downcast_ref::<UnsupportedNative>().is_some());
        assert!(!super::super::ffmpeg::load_attempted());
    }

    #[cfg(not(feature = "ffmpeg"))]
    #[test]
    fn forced_ffmpeg_without_extension_does_not_fall_back_to_native_pcm() {
        let file = pcm_file();
        let error = Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Ffmpeg, true)
            .err()
            .expect("Forced FFmpeg must fail when the extension is not compiled");
        assert!(
            error
                .to_string()
                .contains("FFmpeg extension is not compiled")
        );
        let cancelled = AtomicBool::new(false);
        let error = Source::waveform(file.path(), None, DecoderHint::Ffmpeg, 2, true, &cancelled)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("FFmpeg extension is not compiled")
        );
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn lazy_wave_probe_preserves_native_pcm() {
        const CHILD: &str = "RIVU_LAZY_WAVE_PROBE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Other FFmpeg tests may load the process-wide runtime. Isolate the
            // cold-to-ready transition rather than depending on test order.
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "audio::source::tests::lazy_wave_probe_preserves_native_pcm",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let file = pcm_file();
        assert!(!super::super::ffmpeg::load_attempted());
        let cold = probe_details(file.path(), true, false).unwrap();
        assert!(matches!(cold.probe_backend, ProbeBackend::Native));
        assert!(matches!(cold.decoder_backend, ProbeBackend::Native));
        assert!(!super::super::ffmpeg::load_attempted());

        let mut reference =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
        let expected = decode_remaining(&mut reference);
        let enhanced = probe_details(file.path(), true, true).unwrap();
        assert!(super::super::ffmpeg::load_attempted());
        assert!(matches!(enhanced.probe_backend, ProbeBackend::Ffmpeg));
        assert!(matches!(enhanced.decoder_backend, ProbeBackend::Native));

        let mut forced_native =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Native, true).unwrap();
        assert!(matches!(forced_native.probe_backend, ProbeBackend::Native));
        assert!(!forced_native.uses_ffmpeg());
        assert_eq!(decode_remaining(&mut forced_native), expected);

        let mut ready =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, true).unwrap();
        assert!(!ready.uses_ffmpeg());
        assert_eq!(decode_remaining(&mut ready), expected);
        let disabled = probe_details(file.path(), false, false).unwrap();
        assert!(matches!(disabled.probe_backend, ProbeBackend::Native));
        assert!(matches!(disabled.decoder_backend, ProbeBackend::Native));
    }

    #[cfg(feature = "ffmpeg")]
    fn assert_ffmpeg_pcm(source: &mut Source, expected: &[f32], start_seconds: f64) {
        let rate = source.info().sample_rate as f64;
        let channels = source.layout().len();
        let mut consumed = 0;
        while let Some((samples, timestamp)) = source.next_frames().unwrap() {
            assert!(!samples.is_empty());
            assert_eq!(samples.len() % channels, 0);
            let expected_timestamp = start_seconds + (consumed / channels) as f64 / rate;
            assert!((timestamp - expected_timestamp).abs() < 1e-9);
            assert_eq!(samples, &expected[consumed..consumed + samples.len()]);
            consumed += samples.len();
        }
        assert_eq!(consumed, expected.len());
        assert!(source.next_frames().unwrap().is_none());
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn forced_ffmpeg_waveform_matches_native_pcm_cue_envelope() {
        let file = pcm_file();
        let cancelled = AtomicBool::new(false);
        for range in [
            None,
            Some(super::super::PlaybackRange {
                start_seconds: 30.0 / 75.0,
                end_seconds: Some(80.0 / 75.0),
            }),
        ] {
            let native =
                Source::waveform(file.path(), range, DecoderHint::Native, 2, true, &cancelled)
                    .unwrap();
            let forced =
                Source::waveform(file.path(), range, DecoderHint::Ffmpeg, 2, true, &cancelled)
                    .unwrap();
            assert!(super::super::ffmpeg::load_attempted());
            assert!(forced.complete);
            assert!(forced.matches(file.path(), range, DecoderHint::Ffmpeg));
            assert!(native.matches(file.path(), range, DecoderHint::Native));
            assert_eq!(forced.peaks, native.peaks);
        }
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn ffmpeg_preserves_primed_pcm_packet_timestamps_and_seek_after_eof() {
        let file = pcm_file();
        let mut native =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
        let reference = decode_remaining(&mut native);
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Ffmpeg, true).unwrap();
        assert!(source.uses_ffmpeg());
        assert!(matches!(source.probe_backend, ProbeBackend::Ffmpeg));
        assert!(super::super::ffmpeg::load_attempted());
        assert_ffmpeg_pcm(&mut source, &reference, 0.0);
        source.seek(0.2).unwrap();
        assert!(source.uses_ffmpeg());
        assert_ffmpeg_pcm(&mut source, &reference[8_820..], 0.2);
        source.seek(0.0).unwrap();
        assert!(source.uses_ffmpeg());
        assert_ffmpeg_pcm(&mut source, &reference, 0.0);
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn ffmpeg_cue_trims_primed_pcm_and_seeks_with_segment_relative_timestamps() {
        let file = pcm_file();
        let mut native =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
        let reference = decode_remaining(&mut native);
        for (start_frame, end_frame) in [(0, Some(30)), (30, Some(31)), (30, Some(80)), (80, None)]
        {
            let start = start_frame * 588;
            let end = end_frame.map_or(reference.len(), |frame| frame * 588);
            let mut source =
                Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Ffmpeg, true).unwrap();
            assert!(source.uses_ffmpeg());
            source
                .restrict(super::super::PlaybackRange {
                    start_seconds: start_frame as f64 / 75.0,
                    end_seconds: end_frame.map(|frame| frame as f64 / 75.0),
                })
                .unwrap();
            assert_ffmpeg_pcm(&mut source, &reference[start..end], 0.0);
            let offset = (end - start) / 2;
            let target = offset as f64 / 44_100.0;
            source.seek(target).unwrap();
            assert_ffmpeg_pcm(&mut source, &reference[start + offset..end], target);
            source.seek(source.info().duration.unwrap()).unwrap();
            assert!(source.next_frames().unwrap().is_none());
            source.seek(0.0).unwrap();
            assert_ffmpeg_pcm(&mut source, &reference[start..end], 0.0);
        }
    }

    #[test]
    fn linked_audio_keeps_logical_metadata_and_waveform_identity() {
        let file = pcm_file();
        let directory = tempfile::tempdir().unwrap();
        let alias = directory.path().join("Logical title.wav");
        std::os::unix::fs::symlink(file.path(), &alias).unwrap();
        let mut source = Source::open(&alias, PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
        assert_eq!(source.info().title, "Logical title");
        assert_eq!(decode_remaining(&mut source).len(), 88_200);
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let waveform =
            Source::waveform(&alias, None, DecoderHint::Auto, 2, false, &cancelled).unwrap();
        assert!(waveform.complete);
        assert!(waveform.matches(&alias, None, DecoderHint::Auto));
        assert!(!waveform.matches(file.path(), None, DecoderHint::Auto));
    }

    #[test]
    fn waveform_covers_the_full_file_and_exact_cue_boundaries() {
        let file = pcm_file();
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
        let reference = decode_remaining(&mut source);
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        for (start, end) in [
            (0_usize, None),
            (30, Some(80_usize)),
            (80, None),
            (30, Some(31)),
        ] {
            let range = (start != 0 || end.is_some()).then_some(super::super::PlaybackRange {
                start_seconds: start as f64 / 75.0,
                end_seconds: end.map(|end| end as f64 / 75.0),
            });
            let samples = &reference[start * 588..end.map_or(reference.len(), |end| end * 588)];
            let count = 1024.min(samples.len());
            let mut expected = vec![0.0_f32; count];
            for (index, sample) in samples.iter().enumerate() {
                let bin = index * count / samples.len();
                expected[bin] = expected[bin].max(sample.abs());
            }
            let actual =
                Source::waveform(file.path(), range, DecoderHint::Auto, 2, false, &cancelled)
                    .unwrap();
            assert_eq!(
                actual.peaks,
                expected.into_iter().map(Some).collect::<Vec<_>>(),
                "CUE range {start}..{end:?}"
            );
            assert!(actual.complete);
            assert!(actual.matches(file.path(), range, DecoderHint::Auto));
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
        let envelope =
            Source::waveform(file.path(), None, DecoderHint::Auto, 2, false, &cancelled).unwrap();
        assert_eq!(
            envelope.peaks,
            [
                Some(0.25),
                Some(0.5),
                Some(0.75),
                Some(0.0),
                Some(0.125),
                Some(1.0)
            ]
        );
    }

    #[test]
    fn decoded_float_pcm_clears_invalid_samples_before_consumers() {
        let samples = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1.5,
            -2.0,
            0.125,
            f64::MAX,
        ];
        const REPEATS: usize = 2048;
        for bits in [32_u16, 64] {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            let stride = bits / 8;
            let bytes = (samples.len() * REPEATS) as u32 * u32::from(stride);
            file.write_all(b"RIFF").unwrap();
            file.write_all(&(36 + bytes).to_le_bytes()).unwrap();
            file.write_all(b"WAVEfmt \x10\0\0\0\x03\0\x01\0").unwrap();
            file.write_all(&48_000_u32.to_le_bytes()).unwrap();
            file.write_all(&(48_000 * u32::from(stride)).to_le_bytes())
                .unwrap();
            file.write_all(&stride.to_le_bytes()).unwrap();
            file.write_all(&bits.to_le_bytes()).unwrap();
            file.write_all(b"data").unwrap();
            file.write_all(&bytes.to_le_bytes()).unwrap();
            for sample in samples.into_iter().cycle().take(samples.len() * REPEATS) {
                if bits == 32 {
                    file.write_all(&(sample as f32).to_le_bytes()).unwrap();
                } else {
                    file.write_all(&sample.to_le_bytes()).unwrap();
                }
            }
            for hint in [
                DecoderHint::Native,
                #[cfg(feature = "ffmpeg")]
                DecoderHint::Ffmpeg,
            ] {
                let mut source = Source::open(file.path(), PROBE_BUFFER_LEN, hint, true).unwrap();
                assert_eq!(
                    decode_remaining(&mut source),
                    [0.0, 0.0, 0.0, 1.5, -2.0, 0.125, 0.0].repeat(REPEATS),
                    "{bits}-bit {hint:?} PCM must be finite before any consumer"
                );
            }
        }
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
        let envelope =
            Source::waveform(file.path(), None, DecoderHint::Auto, 2, false, &cancelled).unwrap();
        assert_eq!(
            envelope.peaks,
            [
                Some(0.25),
                Some(1.0),
                Some(1.5),
                Some(2.0),
                Some(0.0),
                Some(0.0)
            ]
        );
        assert_eq!(envelope.max_peak, 2.0);
    }

    #[test]
    fn waveform_cancellation_precedes_opening_the_file() {
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let error = Source::waveform(Path::new(""), None, DecoderHint::Auto, 2, false, &cancelled)
            .unwrap_err();
        assert_eq!(error.to_string(), "Waveform loading cancelled");
    }

    #[test]
    fn cancellation_bounds_native_skips_empty_opus_and_seek_preroll() {
        let pcm = pcm_file();
        let opus = opus_file();
        for mode in 0..3 {
            let path = if mode == 0 { pcm.path() } else { opus.path() };
            let mut source =
                Source::open(path, PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
            source.seek(0.0).unwrap();
            match mode {
                1 => {
                    let Decode::Opus(opus) = &mut source.decode else {
                        panic!("expected native Opus");
                    };
                    opus.skip = usize::MAX;
                }
                2 => source.seek_target = Some(1.9),
                _ => {}
            }
            let cancelled = AtomicBool::new(false);
            let mut packets = 0;
            let error = source
                .read_next_with_reader(Some(&cancelled), &mut 64, |format| {
                    let mut packet = format.next_packet()?.expect("fixture has more packets");
                    packets += 1;
                    if mode == 0 {
                        packet.track_id = packet.track_id.wrapping_add(1);
                    }
                    if packets == 3 {
                        cancelled.store(true, Ordering::Relaxed);
                    }
                    Ok(Some(packet))
                })
                .unwrap_err();
            assert_eq!(packets, 3, "mode {mode} must stop inside the packet loop");
            assert_eq!(error.to_string(), "Waveform loading cancelled");
            assert!(!source.exhausted, "cancellation is not EOF");
            cancelled.store(false, Ordering::Relaxed);
            source.seek(0.0).unwrap();
            assert!(source.next_frames().unwrap().is_some());
        }
    }

    #[test]
    fn decoded_timestamps_place_cue_seeks_in_the_correct_preview_bins() {
        let file = pcm_file();
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
        let range = super::super::PlaybackRange {
            start_seconds: 30.0 / 75.0,
            end_seconds: Some(80.0 / 75.0),
        };
        source.restrict(range).unwrap();
        let rate = source.info.sample_rate;
        let channels = source.layout.len();
        let mut frame = super::super::WaveformFrame::default();
        frame.select(
            file.path(),
            Some(range),
            DecoderHint::Auto,
            source.info.duration,
            rate,
        );
        for target in [0.2, 0.0] {
            source.seek(target).unwrap();
            let (samples, timestamp) = source.next_frames().unwrap().unwrap();
            assert!((timestamp - target).abs() < 1e-9);
            frame.push(timestamp, samples, rate, channels);
            if target > 0.0 {
                let first = (target * rate as f64).round() as usize * frame.peaks.len()
                    / (frame.duration.unwrap() * rate as f64).round() as usize;
                assert!(frame.peaks[..first].iter().all(Option::is_none));
                assert!(frame.peaks[first].is_some());
            } else {
                assert!(frame.peaks[0].is_some());
            }
        }
    }

    #[test]
    fn manual_scan_without_duration_uses_real_pcm_and_finishes_at_eof() {
        let file = pcm_file();
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
        source.info.duration = None;
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let frame = source
            .scan_waveform(file.path(), None, DecoderHint::Auto, &cancelled)
            .unwrap();
        assert!(frame.complete);
        assert_eq!(frame.duration, Some(2.0));
        let bin_seconds =
            frame.bin_frames as f64 / frame.bin_count as f64 / f64::from(source.info.sample_rate);
        assert!(frame.span_seconds >= 2.0 && frame.span_seconds < 2.0 + bin_seconds);
        assert!(frame.peaks.len() <= 1024);
        assert!(frame.peaks.iter().all(Option::is_some));
        assert!(frame.peaks.iter().flatten().any(|peak| *peak >= 0.49));
    }

    #[test]
    fn continuous_playback_finalizes_known_and_unknown_cue_previews_at_eof() {
        let file = pcm_file();
        let cue = super::super::PlaybackRange {
            start_seconds: 30.0 / 75.0,
            end_seconds: Some(80.0 / 75.0),
        };
        for range in [None, Some(cue)] {
            for unknown in [false, true] {
                let mut source =
                    Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
                if let Some(range) = range {
                    source.restrict(range).unwrap();
                }
                let expected_duration = source.info.duration.unwrap();
                if unknown {
                    source.info.duration = None;
                }
                let rate = source.info.sample_rate;
                let channels = source.layout.len();
                let mut frame = super::super::WaveformFrame::default();
                frame.select(
                    file.path(),
                    range,
                    DecoderHint::Auto,
                    source.info.duration,
                    rate,
                );
                let mut coverage = super::super::waveform::WaveformCoverage::new(
                    0.0,
                    source.waveform_timestamp_tolerance(),
                );
                while let Some((samples, timestamp)) = source.next_frames().unwrap() {
                    coverage.record(timestamp, samples.len() / channels, rate);
                    frame.push(timestamp, samples, rate, channels);
                }
                coverage.finish(&mut frame);
                assert!(frame.complete);
                assert!((frame.duration.unwrap() - expected_duration).abs() < 1e-9);
                assert!(frame.span_seconds >= expected_duration);
                let bin_seconds =
                    frame.bin_frames as f64 / frame.bin_count as f64 / f64::from(rate);
                assert!(frame.span_seconds - expected_duration < bin_seconds);
                assert!(frame.peaks.iter().all(Option::is_some));
                let revision = frame.revision;
                coverage.finish(&mut frame);
                assert_eq!(frame.revision, revision);
            }
        }
    }

    // FFmpeg-generated 440 Hz AAC, 48 kHz, 0.2 s, Matroska's 1 ms timestamps.
    // Embedded so this regression needs the decoder runtime, not an ffmpeg CLI.
    #[cfg(feature = "ffmpeg")]
    #[test]
    fn ffmpeg_quantized_timestamps_complete_without_hiding_missing_pcm() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(include_bytes!(
            "../../tests/fixtures/audio/quantized-aac.mka"
        ))
        .unwrap();
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Ffmpeg, true).unwrap();
        let rate = source.info.sample_rate;
        let channels = source.layout.len();
        let mut frame = super::super::WaveformFrame::default();
        frame.select(file.path(), None, DecoderHint::Ffmpeg, None, rate);
        let tolerance = source.waveform_timestamp_tolerance();
        let mut coverage = super::super::waveform::WaveformCoverage::new(0.0, tolerance);
        let mut missing_pcm = super::super::waveform::WaveformCoverage::new(0.0, tolerance);
        let mut packet = 0;
        while let Some((samples, timestamp)) = source.next_frames().unwrap() {
            coverage.record(timestamp, samples.len() / channels, rate);
            if packet != 3 {
                missing_pcm.record(timestamp, samples.len() / channels, rate);
            }
            frame.push(timestamp, samples, rate, channels);
            packet += 1;
        }
        let mut incomplete = frame.clone();
        coverage.finish(&mut frame);
        missing_pcm.finish(&mut incomplete);
        assert!(frame.complete, "container quantization is not missing PCM");
        assert!(!incomplete.complete, "a missing AAC frame is a real gap");
        assert_eq!(frame.duration, incomplete.duration);
    }

    #[test]
    fn seeked_decoder_eof_keeps_cached_peaks_and_unknown_intervals_incomplete() {
        let file = pcm_file();
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
        source.info.duration = None;
        let rate = source.info.sample_rate;
        let channels = source.layout.len();
        let mut frame = super::super::WaveformFrame::default();
        frame.select(file.path(), None, DecoderHint::Auto, None, rate);
        let (samples, timestamp) = source.next_frames().unwrap().unwrap();
        let first_count = samples.len().min(441 * channels);
        frame.push(timestamp, &samples[..first_count], rate, channels);
        let first_peak = frame.peaks[0];
        assert!(first_peak.is_some());
        source.seek(1.0).unwrap();
        let mut coverage = super::super::waveform::WaveformCoverage::new(
            1.0,
            source.waveform_timestamp_tolerance(),
        );
        while let Some((samples, timestamp)) = source.next_frames().unwrap() {
            coverage.record(timestamp, samples.len() / channels, rate);
            frame.push(timestamp, samples, rate, channels);
        }
        coverage.finish(&mut frame);
        assert!(!frame.complete);
        assert_eq!(frame.duration, Some(2.0));
        let bin_seconds = frame.bin_frames as f64 / frame.bin_count as f64 / f64::from(rate);
        assert!(frame.span_seconds >= 2.0 && frame.span_seconds < 2.0 + bin_seconds);
        assert_eq!(frame.peaks[0], first_peak);
        let resumed_bin = (1.0 / bin_seconds) as usize;
        assert!(frame.peaks[1..resumed_bin].iter().all(Option::is_none));
        assert!(frame.peaks[resumed_bin].is_some());
    }

    #[test]
    fn cue_ranges_crop_pcm_and_seek_relative_to_track() {
        let file = pcm_file();
        let mut full =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
        assert_eq!(full.info().bitrate_bps, Some(705_600));
        assert_eq!(full.info().bits_per_sample, Some(16));
        let reference = decode_remaining(&mut full);
        for (start_frame, end_frame) in [(0_u64, Some(30_u64)), (30, Some(80)), (80, None)] {
            let start = (start_frame * 588) as usize;
            let end = end_frame.map_or(reference.len(), |frame| (frame * 588) as usize);
            let mut source =
                Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
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
        let mut source =
            Source::open(file.path(), PROBE_BUFFER_LEN, DecoderHint::Auto, false).unwrap();
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
