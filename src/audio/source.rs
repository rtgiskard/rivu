//! Container probing, decoding, seeking, and packet-accurate stereo samples.

use super::{BYTES_PER_MEBIBYTE, MediaInfo, Stereo};
use anyhow::{Context, Result, bail};
use std::{fs::File, path::Path};
use symphonia::core::{
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
    formats::probe::Hint,
    formats::{FormatOptions, FormatReader, SeekMode, SeekTo, Track, TrackType},
    io::{MediaSourceStream, MediaSourceStreamOptions},
    meta::{Metadata, MetadataOptions, StandardTag},
    units::{Time, TimeBase},
};
use symphonia::default::{get_codecs, get_probe};

const PROBE_BUFFER_LEN: usize = 2 * BYTES_PER_MEBIBYTE;

pub fn probe(path: &Path) -> Result<MediaInfo> {
    Ok(Source::open(path, PROBE_BUFFER_LEN)?.info)
}

fn metadata_tags(
    mut metadata: Metadata<'_>,
    track: u32,
    title: &mut Option<String>,
    artist: &mut Option<String>,
    album: &mut Option<String>,
) {
    while let Some(revision) = metadata.current() {
        let track_tags = revision
            .per_track
            .iter()
            .filter(|metadata| metadata.track_id == u64::from(track))
            .flat_map(|metadata| &metadata.metadata.tags);
        for tag in revision.media.tags.iter().chain(track_tags) {
            match tag.std.as_ref() {
                Some(StandardTag::TrackTitle(value)) => *title = Some(value.as_ref().clone()),
                Some(StandardTag::Artist(value)) => *artist = Some(value.as_ref().clone()),
                Some(StandardTag::Album(value)) => *album = Some(value.as_ref().clone()),
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
    if !(1..=2).contains(&channels) || head[18] != 0 {
        bail!(
            "This Opus channel mapping is not supported; mono/stereo mapping family 0 is supported"
        );
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

pub(super) struct Source {
    format: Box<dyn FormatReader>,
    track: u32,
    decode: Decode,
    time_base: TimeBase,
    frames: Vec<Stereo>,
    info: MediaInfo,
    seek_target: Option<f64>,
    ready: bool,
    exhausted: bool,
}
impl Source {
    pub(super) fn open(path: &Path, buffer_len: usize) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("Opening {}", path.display()))?;
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
            hint.with_extension(extension);
        }
        let mut format = get_probe().probe(
            &hint,
            MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions { buffer_len }),
            FormatOptions::default(),
            MetadataOptions::default(),
        )?;
        let default = format.default_track(TrackType::Audio).map(|track| track.id);
        let mut candidates = format.tracks().iter().collect::<Vec<_>>();
        candidates.sort_by_key(|track| Some(track.id) != default);
        let mut chosen = None;
        let mut errors = Vec::new();
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
                Err(error) => errors.push(format!("{:?}: {error}", params.codec)),
            }
        }
        let (track, params, track_time_base, decode) = chosen.with_context(|| {
            if errors.is_empty() {
                "Container has no audio track".into()
            } else {
                format!("No supported audio decoder: {}", errors.join("; "))
            }
        })?;
        let (mut title, mut artist, mut album) = (None, None, None);
        metadata_tags(
            format.metadata(),
            track,
            &mut title,
            &mut artist,
            &mut album,
        );
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
        let info = MediaInfo {
            title: title.unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }),
            artist: artist.unwrap_or_default(),
            album: album.unwrap_or_default(),
            duration: media_duration,
            codec,
            channels: params
                .channels
                .map_or(0, |channels| channels.count() as u16),
            sample_rate: rate,
        };
        let mut source = Self {
            time_base: track_time_base.unwrap_or_else(|| TimeBase::try_new(1, rate).unwrap()),
            format,
            track,
            decode,
            frames: Vec::with_capacity(8192),
            info,
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
    pub(super) fn seek(&mut self, target: f64) -> Result<()> {
        let target = self
            .info
            .duration
            .map_or(target, |duration| target.min(duration))
            .max(0.0);
        // Many demuxers reject the exclusive end timestamp. It is a valid
        // player position, but there are no samples left to request there.
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
        let preroll = if matches!(self.decode, Decode::Opus(_)) {
            (target - 0.08).max(0.0)
        } else {
            target
        };
        let sought = self.format.seek(
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

    /// Returns the primed first packet before advancing the decoder.
    pub(super) fn next_frames(&mut self) -> Result<Option<&[Stereo]>> {
        if self.ready {
            self.ready = false;
        } else if !self.read_next()? {
            return Ok(None);
        }
        Ok(Some(&self.frames))
    }

    fn read_next(&mut self) -> Result<bool> {
        if self.exhausted {
            return Ok(false);
        }
        loop {
            // Seeking can discard whole packets (including Opus preroll).
            // Never append the next packet onto samples already discarded.
            self.frames.clear();
            let Some(packet) = self.format.next_packet()? else {
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
                    self.info.channels = audio.spec().channels().count() as u16;
                    mix_audio(audio, &mut self.frames);
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
                    self.frames.extend(
                        opus.samples[leading * opus.channels..end * opus.channels]
                            .chunks_exact(opus.channels)
                            .map(|samples| [samples[0], samples[opus.channels - 1]]),
                    );
                    self.info.channels = opus.channels as u16;
                    packet_start =
                        (packet_start - opus.timestamp_delay + leading as f64 / 48_000.0).max(0.0);
                }
            }
            if let Some(duration) = self.info.duration {
                let remaining = ((duration - packet_start).max(0.0) * self.info.sample_rate as f64)
                    .round() as usize;
                self.frames.truncate(remaining);
            }
            if let Some(target) = self.seek_target {
                let discard = ((target - packet_start).max(0.0) * self.info.sample_rate as f64)
                    .round() as usize;
                if discard >= self.frames.len() {
                    continue;
                }
                self.frames.copy_within(discard.., 0);
                self.frames.truncate(self.frames.len() - discard);
                self.seek_target = None;
            }
            if !self.frames.is_empty() {
                return Ok(true);
            }
        }
    }
}

fn mix_samples<S: Sample>(audio: &AudioBuffer<S>, output: &mut Vec<Stereo>)
where
    f32: FromSample<S>,
{
    let channels = audio.spec().channels();
    if channels.count() == 1 {
        output.extend(audio.plane(0).unwrap().iter().map(|sample| {
            let value = f32::from_sample(*sample);
            [value, value]
        }));
    } else if channels.count() == 2 {
        output.extend(
            audio
                .plane(0)
                .unwrap()
                .iter()
                .zip(audio.plane(1).unwrap())
                .map(|(left, right)| [f32::from_sample(*left), f32::from_sample(*right)]),
        );
    } else {
        output.resize(audio.frames(), [0.0; 2]);
        let mut sum = [0.0_f32; 2];
        for index in 0..channels.count() {
            let position = match channels {
                Channels::Positioned(positions) => positions.iter().nth(index),
                Channels::Custom(labels) => match labels[index] {
                    ChannelLabel::Positioned(position) => Some(position),
                    _ => None,
                },
                _ => None,
            };
            let weight = match position {
                Some(Position::FRONT_LEFT) => [1.0, 0.0],
                Some(Position::FRONT_RIGHT) => [0.0, 1.0],
                Some(Position::LFE1) | Some(Position::LFE2) => [0.0, 0.0],
                Some(Position::REAR_LEFT) | Some(Position::SIDE_LEFT) => [0.707, 0.0],
                Some(Position::REAR_RIGHT) | Some(Position::SIDE_RIGHT) => [0.0, 0.707],
                _ => [0.707, 0.707],
            };
            sum[0] += weight[0];
            sum[1] += weight[1];
            for (frame, sample) in output.iter_mut().zip(audio.plane(index).unwrap()) {
                let value = f32::from_sample(*sample);
                frame[0] += value * weight[0];
                frame[1] += value * weight[1];
            }
        }
        let scale = [sum[0].max(1.0).recip(), sum[1].max(1.0).recip()];
        for frame in output {
            frame[0] *= scale[0];
            frame[1] *= scale[1];
        }
    }
}
fn mix_audio(audio: GenericAudioBufferRef<'_>, output: &mut Vec<Stereo>) {
    match audio {
        GenericAudioBufferRef::U8(audio) => mix_samples(audio, output),
        GenericAudioBufferRef::U16(audio) => mix_samples(audio, output),
        GenericAudioBufferRef::U24(audio) => mix_samples(audio, output),
        GenericAudioBufferRef::U32(audio) => mix_samples(audio, output),
        GenericAudioBufferRef::S8(audio) => mix_samples(audio, output),
        GenericAudioBufferRef::S16(audio) => mix_samples(audio, output),
        GenericAudioBufferRef::S24(audio) => mix_samples(audio, output),
        GenericAudioBufferRef::S32(audio) => mix_samples(audio, output),
        GenericAudioBufferRef::F32(audio) => mix_samples(audio, output),
        GenericAudioBufferRef::F64(audio) => mix_samples(audio, output),
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

    fn decode_remaining(source: &mut Source) -> Vec<Stereo> {
        let mut samples = Vec::new();
        while let Some(frames) = source.next_frames().unwrap() {
            samples.extend_from_slice(frames);
        }
        samples
    }

    #[test]
    fn opus_seek_discards_whole_preroll_packets_without_reusing_pcm() {
        let file = opus_file();
        let mut source = Source::open(file.path(), PROBE_BUFFER_LEN).unwrap();
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
                            let expected = f64::from(expected[0]);
                            (dot + f64::from(actual[0]) * expected, power + expected * expected)
                        });
                    // The actual window's norm is constant across candidates.
                    (candidate, dot / power.sqrt())
                })
                .max_by(|(_, left), (_, right)| left.total_cmp(right))
                .unwrap()
                .0;
            assert_eq!(best, offset, "Seek must start at the requested sample for {target}s");
        }
    }

    #[test]
    fn seek_to_duration_is_exhausted_and_can_seek_back() {
        let file = opus_file();
        let mut source = Source::open(file.path(), PROBE_BUFFER_LEN).unwrap();
        let duration = source.info().duration.unwrap();
        for target in [duration, duration + 1.0] {
            source.seek(target).unwrap();
            assert!(source.next_frames().unwrap().is_none());
            assert!(source.next_frames().unwrap().is_none());
            source.seek(0.0).unwrap();
            assert_eq!(decode_remaining(&mut source).len(), 96_000);
        }
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
}
