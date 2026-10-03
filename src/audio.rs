use crate::analysis::{AnalysisControl, AnalysisFrame, AnalysisWorker, TapFrame};
use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender, bounded, select};
use parking_lot::RwLock;
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction, audioadapter_buffers::direct::InterleavedSlice,
};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
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
    io::MediaSourceStream,
    meta::{Metadata, MetadataOptions, StandardTag},
    units::{Time, TimeBase},
};
use symphonia::default::{get_codecs, get_probe};

type Stereo = [f32; 2];

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Option<f64>,
    pub codec: String,
    pub channels: u16,
    pub sample_rate: u32,
}

pub enum AudioCommand {
    Load {
        path: PathBuf,
        generation: u64,
        start_seconds: f64,
        paused: bool,
    },
    Pause(bool),
    Stop,
    Seek(f64),
    Volume(f32),
    Device(Option<String>),
    Analysis(bool),
    /// Set analyzer publication rate. Values outside 5..=60 fps are ignored.
    AnalysisRate(u32),
    StopAndSnapshot(Sender<(u64, f64)>),
    Shutdown,
}
#[derive(Clone, Debug)]
pub enum AudioEvent {
    Started {
        generation: u64,
        info: MediaInfo,
    },
    Progress {
        generation: u64,
        position_seconds: f64,
        listened_seconds: f64,
    },
    Ended {
        generation: u64,
    },
    Failed {
        generation: u64,
        message: String,
    },
}

pub struct AudioEngine {
    pub commands: Sender<AudioCommand>,
    pub events: Receiver<AudioEvent>,
    pub analysis: Arc<RwLock<AnalysisFrame>>,
    worker: Option<JoinHandle<()>>,
}
impl AudioEngine {
    pub fn new() -> Result<Self> {
        let (commands, receive) = bounded(64);
        let (send_events, events) = bounded(128);
        let analysis = Arc::new(RwLock::new(AnalysisFrame::default()));
        let analyzer = AnalysisWorker::new(analysis.clone())?;
        let worker = thread::Builder::new()
            .name("rivu-audio".into())
            .spawn(move || Worker::new(receive, send_events, analyzer).run())?;
        Ok(Self {
            commands,
            events,
            analysis,
            worker: Some(worker),
        })
    }
    pub fn send(&self, command: AudioCommand) -> Result<()> {
        self.commands.send(command).context("Audio worker stopped")
    }
    pub fn stop_and_snapshot(&self) -> Result<(u64, f64)> {
        let (send, receive) = bounded(1);
        self.send(AudioCommand::StopAndSnapshot(send))?;
        receive
            .recv_timeout(Duration::from_secs(5))
            .context("Audio output did not stop")
    }
}
impl Drop for AudioEngine {
    fn drop(&mut self) {
        let _ = self.commands.send(AudioCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn devices() -> Result<Vec<String>> {
    let mut names = Vec::new();
    for device in cpal::default_host().output_devices()? {
        names.push(device.description()?.name().to_owned());
    }
    names.sort();
    names.dedup();
    Ok(names)
}
fn find_device(name: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name {
        for device in host.output_devices()? {
            if device.description()?.name() == name {
                return Ok(device);
            }
        }
        bail!("Output device not found: {name}");
    }
    host.default_output_device()
        .context("No default audio output device")
}

pub fn probe(path: &Path) -> Result<MediaInfo> {
    Ok(Source::open(path)?.info)
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

struct Source {
    format: Box<dyn FormatReader>,
    track: u32,
    decode: Decode,
    time_base: TimeBase,
    frames: Vec<Stereo>,
    info: MediaInfo,
    seek_target: Option<f64>,
    ready: bool,
}
impl Source {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("Opening {}", path.display()))?;
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
            hint.with_extension(extension);
        }
        let mut format = get_probe().probe(
            &hint,
            MediaSourceStream::new(Box::new(file), Default::default()),
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
        };
        if !source.read_next()? {
            bail!("Audio track contains no decodable samples");
        }
        source.ready = true;
        Ok(source)
    }
    fn seek(&mut self, target: f64) -> Result<()> {
        let target = self
            .info
            .duration
            .map_or(target, |duration| target.min(duration))
            .max(0.0);
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
        Ok(())
    }
    fn read_next(&mut self) -> Result<bool> {
        self.frames.clear();
        loop {
            let Some(packet) = self.format.next_packet()? else {
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

struct Converter {
    sinc: Option<Async<f32>>,
    input: Vec<f32>,
    output: Vec<f32>,
    fill: usize,
    delay: usize,
    input_count: u64,
    output_count: u64,
    ratio: f64,
}
impl Converter {
    const CHUNK: usize = 1024;
    fn new(source_rate: u32, output_rate: u32) -> Result<Self> {
        let ratio = output_rate as f64 / source_rate as f64;
        let sinc = if source_rate == output_rate {
            None
        } else {
            Some(Async::new_sinc(
                ratio,
                1.0,
                &SincInterpolationParameters {
                    sinc_len: 256,
                    f_cutoff: Some(0.95),
                    interpolation: SincInterpolationType::Quadratic,
                    oversampling_factor: 128,
                    window: WindowFunction::BlackmanHarris2,
                },
                Self::CHUNK,
                2,
                FixedAsync::Input,
            )?)
        };
        let delay = sinc.as_ref().map_or(0, Resampler::output_delay);
        let output = sinc
            .as_ref()
            .map_or_else(Vec::new, |sinc| vec![0.0; 2 * sinc.output_frames_max()]);
        Ok(Self {
            sinc,
            input: vec![0.0; 2 * Self::CHUNK],
            output,
            fill: 0,
            delay,
            input_count: 0,
            output_count: 0,
            ratio,
        })
    }
    fn push(&mut self, frames: &[Stereo], output: &mut Vec<Stereo>) -> Result<()> {
        self.input_count += frames.len() as u64;
        if self.sinc.is_none() {
            output.extend_from_slice(frames);
            self.output_count += frames.len() as u64;
            return Ok(());
        }
        for frame in frames {
            self.input[2 * self.fill] = frame[0];
            self.input[2 * self.fill + 1] = frame[1];
            self.fill += 1;
            if self.fill == Self::CHUNK {
                self.process(output, usize::MAX)?;
            }
        }
        Ok(())
    }
    fn process(&mut self, output: &mut Vec<Stereo>, limit: usize) -> Result<()> {
        let input = InterleavedSlice::new(&self.input, 2, Self::CHUNK)?;
        let output_frames = self.output.len() / 2;
        let mut output_buffer = InterleavedSlice::new_mut(&mut self.output, 2, output_frames)?;
        let (_, count) = self
            .sinc
            .as_mut()
            .expect("sinc converter")
            .process_into_buffer(&input, &mut output_buffer, None)?;
        let skip = self.delay.min(count);
        self.delay -= skip;
        let end = count.min(skip.saturating_add(limit));
        output.extend(
            self.output[2 * skip..2 * end]
                .as_chunks::<2>()
                .0
                .iter()
                .copied(),
        );
        self.output_count += (end - skip) as u64;
        self.fill = 0;
        Ok(())
    }
    fn finish(&mut self, output: &mut Vec<Stereo>) -> Result<()> {
        if self.sinc.is_none() {
            return Ok(());
        }
        let target = (self.input_count as f64 * self.ratio).round() as u64;
        while self.output_count < target {
            self.input[2 * self.fill..].fill(0.0);
            self.process(output, (target - self.output_count) as usize)?;
        }
        Ok(())
    }
}

struct OutputShared {
    paused: AtomicBool,
    consumed: AtomicU64,
    volume: AtomicU32,
}
struct Output {
    stream: Option<cpal::Stream>,
    producer: HeapProd<Stereo>,
    shared: Arc<OutputShared>,
    rate: u32,
    errors: Receiver<cpal::Error>,
}
impl Output {
    fn new(
        name: Option<&str>,
        volume: f32,
        paused: bool,
        analyzer: &AnalysisWorker,
    ) -> Result<Self> {
        let device = find_device(name)?;
        let supported = device.default_output_config()?;
        let config = supported.config();
        let rate = config.sample_rate;
        if config.channels == 0 || rate == 0 {
            bail!("Invalid output device configuration");
        }
        let (producer, consumer) = HeapRb::new((rate / 4).max(2048) as usize).split();
        let shared = Arc::new(OutputShared {
            paused: AtomicBool::new(paused),
            consumed: AtomicU64::new(0),
            volume: AtomicU32::new(volume.to_bits()),
        });
        let (error_send, errors) = bounded(4);
        analyzer.reset(rate);
        let tap = analyzer.producer();
        let control = analyzer.control.clone();
        macro_rules! build {
            ($sample:ty) => {{
                let shared_callback = shared.clone();
                let mut consumer = consumer;
                let mut tap = tap;
                let channels = config.channels as usize;
                device.build_output_stream(
                    config,
                    move |data: &mut [$sample], _| {
                        render(
                            data,
                            channels,
                            &mut consumer,
                            &mut tap,
                            &shared_callback,
                            &control,
                        );
                    },
                    move |error| {
                        let _ = error_send.try_send(error);
                    },
                    None,
                )?
            }};
        }
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => build!(f32),
            cpal::SampleFormat::F64 => build!(f64),
            cpal::SampleFormat::I8 => build!(i8),
            cpal::SampleFormat::I16 => build!(i16),
            cpal::SampleFormat::I24 => build!(cpal::I24),
            cpal::SampleFormat::I32 => build!(i32),
            cpal::SampleFormat::I64 => build!(i64),
            cpal::SampleFormat::U8 => build!(u8),
            cpal::SampleFormat::U16 => build!(u16),
            cpal::SampleFormat::U24 => build!(cpal::U24),
            cpal::SampleFormat::U32 => build!(u32),
            cpal::SampleFormat::U64 => build!(u64),
            other => bail!("Unsupported device sample format: {other:?}"),
        };
        if !paused {
            stream.play()?;
        }
        Ok(Self {
            stream: Some(stream),
            producer,
            shared,
            rate,
            errors,
        })
    }
    fn heard(&self) -> f64 {
        self.shared.consumed.load(Ordering::Acquire) as f64 / self.rate as f64
    }
    fn pause(&self, paused: bool) -> Result<()> {
        self.shared.paused.store(paused, Ordering::Release);
        if let Some(stream) = &self.stream {
            if paused {
                stream.pause()?;
            } else {
                stream.play()?;
            }
        }
        Ok(())
    }
    fn close(&mut self) -> f64 {
        self.shared.paused.store(true, Ordering::Release);
        self.stream.take();
        self.heard()
    }
}
fn render<T: cpal::Sample + cpal::FromSample<f32>>(
    data: &mut [T],
    channels: usize,
    consumer: &mut HeapCons<Stereo>,
    tap: &mut HeapProd<TapFrame>,
    shared: &OutputShared,
    control: &AnalysisControl,
) {
    let paused = shared.paused.load(Ordering::Acquire);
    let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
    let analyze = control.enabled.load(Ordering::Relaxed);
    let epoch = control.epoch.load(Ordering::Relaxed);
    let mut consumed = 0;
    for output in data.chunks_exact_mut(channels) {
        let samples = if paused {
            [0.0; 2]
        } else if let Some(samples) = consumer.try_pop() {
            consumed += 1;
            if analyze {
                let _ = tap.try_push(TapFrame { samples, epoch });
            }
            samples
        } else {
            [0.0; 2]
        };
        output.fill(T::from_sample(0.0));
        if channels == 1 {
            output[0] = T::from_sample(((samples[0] + samples[1]) * 0.5 * volume).clamp(-1.0, 1.0));
        } else {
            output[0] = T::from_sample((samples[0] * volume).clamp(-1.0, 1.0));
            output[1] = T::from_sample((samples[1] * volume).clamp(-1.0, 1.0));
        }
    }
    if consumed > 0 {
        shared.consumed.fetch_add(consumed, Ordering::Release);
    }
}

struct PlaybackOptions {
    path: PathBuf,
    generation: u64,
    start: f64,
    paused: bool,
    device: Option<String>,
    volume: f32,
    prior_heard: f64,
}

struct Playback {
    source: Source,
    output: Output,
    converter: Converter,
    pending: Vec<Stereo>,
    offset: usize,
    generation: u64,
    path: PathBuf,
    base_position: f64,
    prior_heard: f64,
    eof: bool,
    paused: bool,
}
impl Playback {
    fn new(options: PlaybackOptions, analyzer: &AnalysisWorker) -> Result<Self> {
        let PlaybackOptions {
            path,
            generation,
            start,
            paused,
            device,
            volume,
            prior_heard,
        } = options;
        let mut source = Source::open(&path)?;
        if start > 0.0 {
            source.seek(start)?;
        }
        let output = Output::new(device.as_deref(), volume, paused, analyzer)?;
        let converter = Converter::new(source.info.sample_rate, output.rate)?;
        Ok(Self {
            source,
            output,
            converter,
            pending: Vec::with_capacity(16_384),
            offset: 0,
            generation,
            path,
            base_position: start,
            prior_heard,
            eof: false,
            paused,
        })
    }
    fn heard(&self) -> f64 {
        self.prior_heard + self.output.heard()
    }
    fn position(&self) -> f64 {
        let value = self.base_position + self.output.heard();
        self.source
            .info
            .duration
            .map_or(value, |duration| value.min(duration))
    }
    fn fill(&mut self) -> Result<()> {
        while self.output.producer.vacant_len() > 0 {
            if self.offset < self.pending.len() {
                let count = self
                    .output
                    .producer
                    .push_slice(&self.pending[self.offset..]);
                self.offset += count;
                if self.offset < self.pending.len() {
                    return Ok(());
                }
            }
            if self.eof {
                return Ok(());
            }
            self.pending.clear();
            self.offset = 0;
            if self.source.ready {
                self.source.ready = false;
                self.converter
                    .push(&self.source.frames, &mut self.pending)?;
            } else if self.source.read_next()? {
                self.converter
                    .push(&self.source.frames, &mut self.pending)?;
            } else {
                self.converter.finish(&mut self.pending)?;
                self.eof = true;
            }
        }
        Ok(())
    }
    fn finished(&self) -> bool {
        self.eof && self.offset == self.pending.len() && self.output.producer.occupied_len() == 0
    }
    fn close(&mut self) -> f64 {
        self.prior_heard + self.output.close()
    }
}

struct Worker {
    commands: Receiver<AudioCommand>,
    events: Sender<AudioEvent>,
    analyzer: AnalysisWorker,
    playback: Option<Playback>,
    device: Option<String>,
    volume: f32,
    wanted_analysis: bool,
    last_snapshot: (u64, f64),
    progress: Instant,
}
impl Worker {
    fn new(
        commands: Receiver<AudioCommand>,
        events: Sender<AudioEvent>,
        analyzer: AnalysisWorker,
    ) -> Self {
        Self {
            commands,
            events,
            analyzer,
            playback: None,
            device: None,
            volume: 0.7,
            wanted_analysis: false,
            last_snapshot: (0, 0.0),
            progress: Instant::now(),
        }
    }
    fn event(&self, event: AudioEvent) {
        let _ = self.events.send_timeout(event, Duration::from_secs(1));
    }
    fn stop(&mut self) -> (u64, f64) {
        self.analyzer.set_enabled(false);
        if let Some(mut playback) = self.playback.take() {
            self.last_snapshot = (playback.generation, playback.close());
        }
        self.last_snapshot
    }
    fn refresh_analysis(&self) {
        self.analyzer.set_enabled(
            self.wanted_analysis
                && self
                    .playback
                    .as_ref()
                    .is_some_and(|playback| !playback.paused),
        );
    }
    fn command(&mut self, command: AudioCommand) -> bool {
        match command {
            AudioCommand::Shutdown => {
                self.stop();
                return false;
            }
            AudioCommand::Stop => {
                self.stop();
            }
            AudioCommand::StopAndSnapshot(reply) => {
                let snapshot = self.stop();
                let _ = reply.send(snapshot);
            }
            AudioCommand::Analysis(enabled) => {
                self.wanted_analysis = enabled;
                self.refresh_analysis();
            }
            AudioCommand::AnalysisRate(rate) if (5..=60).contains(&rate) => {
                self.analyzer.set_rate(rate);
            }
            AudioCommand::AnalysisRate(_) => {}
            AudioCommand::Volume(volume) => {
                self.volume = volume;
                if let Some(playback) = &self.playback {
                    playback
                        .output
                        .shared
                        .volume
                        .store(volume.to_bits(), Ordering::Release);
                }
            }
            AudioCommand::Load {
                path,
                generation,
                start_seconds,
                paused,
            } => {
                self.stop();
                match Playback::new(
                    PlaybackOptions {
                        path,
                        generation,
                        start: start_seconds,
                        paused,
                        device: self.device.clone(),
                        volume: self.volume,
                        prior_heard: 0.0,
                    },
                    &self.analyzer,
                ) {
                    Ok(playback) => {
                        self.event(AudioEvent::Started {
                            generation,
                            info: playback.source.info.clone(),
                        });
                        self.playback = Some(playback);
                        self.refresh_analysis();
                    }
                    Err(error) => self.event(AudioEvent::Failed {
                        generation,
                        message: format!("{error:#}"),
                    }),
                }
            }
            AudioCommand::Pause(paused) => {
                let result = self.playback.as_mut().map(|playback| {
                    playback.output.pause(paused)?;
                    playback.paused = paused;
                    Ok::<_, anyhow::Error>(())
                });
                if let Some(Err(error)) = result {
                    self.fail(error);
                }
                self.refresh_analysis();
            }
            AudioCommand::Seek(seconds) => self.reopen(Some(seconds)),
            AudioCommand::Device(name) => {
                if let Err(error) = find_device(name.as_deref()) {
                    self.fail(error);
                } else {
                    self.device = name;
                    self.reopen(None);
                }
            }
        }
        true
    }
    fn reopen(&mut self, target: Option<f64>) {
        if let Some(mut old) = self.playback.take() {
            let position = target.unwrap_or_else(|| old.position());
            let heard = old.close();
            let generation = old.generation;
            match Playback::new(
                PlaybackOptions {
                    path: old.path.clone(),
                    generation,
                    start: position,
                    paused: old.paused,
                    device: self.device.clone(),
                    volume: self.volume,
                    prior_heard: heard,
                },
                &self.analyzer,
            ) {
                Ok(playback) => self.playback = Some(playback),
                Err(error) => {
                    self.last_snapshot = (generation, heard);
                    self.event(AudioEvent::Failed {
                        generation,
                        message: format!("{error:#}"),
                    });
                }
            }
            self.refresh_analysis();
        }
    }
    fn fail(&mut self, error: anyhow::Error) {
        let (generation, _) = self.stop();
        self.event(AudioEvent::Failed {
            generation,
            message: format!("{error:#}"),
        });
    }
    fn run(mut self) {
        loop {
            while let Ok(command) = self.commands.try_recv() {
                if !self.command(command) {
                    return;
                }
            }
            if self.playback.is_none()
                || self
                    .playback
                    .as_ref()
                    .is_some_and(|playback| playback.paused)
            {
                let Ok(command) = self.commands.recv() else {
                    break;
                };
                if !self.command(command) {
                    break;
                }
                continue;
            }
            let result = self.playback.as_mut().unwrap().fill();
            if let Err(error) = result {
                self.fail(error);
                continue;
            }
            if self.progress.elapsed() >= Duration::from_millis(100) {
                let playback = self.playback.as_ref().unwrap();
                self.event(AudioEvent::Progress {
                    generation: playback.generation,
                    position_seconds: playback.position(),
                    listened_seconds: playback.heard(),
                });
                self.progress = Instant::now();
            }
            if self.playback.as_ref().unwrap().finished() {
                let playback = self.playback.as_ref().unwrap();
                let generation = playback.generation;
                let position = playback.position();
                let (_, heard) = self.stop();
                self.event(AudioEvent::Progress {
                    generation,
                    position_seconds: position,
                    listened_seconds: heard,
                });
                self.event(AudioEvent::Ended { generation });
                continue;
            }
            let playback = self.playback.as_ref().unwrap();
            let errors = playback.output.errors.clone();
            select! {
                recv(self.commands) -> command => {
                    let Ok(command) = command else { break; };
                    if !self.command(command) { break; }
                }
                recv(errors) -> error => if let Ok(error) = error { self.fail(anyhow!(error)); },
                // Poll only during playback: a 250 ms ring absorbs scheduling jitter,
                // without channel wake-up locks in the real-time PCM callback.
                default(Duration::from_millis(10)) => (),
            }
        }
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn heard_frames_exclude_pause_and_underflow() {
        let analyzer =
            AnalysisWorker::new(Arc::new(RwLock::new(AnalysisFrame::default()))).unwrap();
        let mut tap = analyzer.producer();
        let (mut producer, mut consumer) = HeapRb::new(8).split();
        let shared = OutputShared {
            paused: AtomicBool::new(false),
            consumed: AtomicU64::new(0),
            volume: AtomicU32::new(1.0f32.to_bits()),
        };
        producer.push_slice(&[[0.25, -0.25]; 3]);
        let mut data = [0.0_f32; 16];
        render(
            &mut data,
            2,
            &mut consumer,
            &mut tap,
            &shared,
            &analyzer.control,
        );
        assert_eq!(shared.consumed.load(Ordering::Acquire), 3);
        assert_eq!(&data[..6], &[0.25, -0.25, 0.25, -0.25, 0.25, -0.25]);
        assert!(data[6..].iter().all(|sample| *sample == 0.0));
        producer.push_slice(&[[0.5, -0.5]; 2]);
        shared.paused.store(true, Ordering::Release);
        render(
            &mut data,
            2,
            &mut consumer,
            &mut tap,
            &shared,
            &analyzer.control,
        );
        assert_eq!(shared.consumed.load(Ordering::Acquire), 3);
        assert_eq!(consumer.occupied_len(), 2);
        assert!(data.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn resampling_preserves_duration_pitch_and_opposite_phase_stereo() {
        let input: Vec<Stereo> = (0..11_025)
            .map(|index| {
                let sample = (std::f32::consts::TAU * 440.0 * index as f32 / 44_100.0).sin() * 0.5;
                [sample, -sample]
            })
            .collect();
        let mut converter = Converter::new(44_100, 48_000).unwrap();
        let mut output = Vec::new();
        for chunk in input.chunks(137) {
            converter.push(chunk, &mut output).unwrap();
        }
        converter.finish(&mut output).unwrap();
        assert_eq!(
            output.len(),
            12_000,
            "No padding or filter delay may change the audible duration"
        );
        assert!(
            output
                .iter()
                .all(|frame| (frame[0] + frame[1]).abs() < 1e-6)
        );
        let power = |frequency: f32| {
            let mut real = 0.0;
            let mut imag = 0.0;
            for (index, frame) in output.iter().enumerate() {
                let phase = std::f32::consts::TAU * frequency * index as f32 / 48_000.0;
                real += frame[0] * phase.cos();
                imag += frame[0] * phase.sin();
            }
            real * real + imag * imag
        };
        assert!(
            power(440.0) > power(880.0) * 100.0,
            "Resampling must not change pitch"
        );
    }
}
