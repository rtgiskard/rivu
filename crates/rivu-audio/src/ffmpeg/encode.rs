//! Offline encoding only. Playback never enters this module.
use super::*;
use crate::encoding::validate_full_scale;
use anyhow::ensure;
use rivu_protocol::model::{AudioExportEncoding, AudioExportFormat};
use std::{ffi::c_void, os::unix::ffi::OsStrExt};

struct EncodeApi {
    _swr: Library,
    find: unsafe extern "C" fn(*const c_char) -> *const AVCodec,
    guess_format:
        unsafe extern "C" fn(*const c_char, *const c_char, *const c_char) -> *const AVOutputFormat,
    output: unsafe extern "C" fn(
        *mut *mut AVFormatContext,
        *const AVOutputFormat,
        *const c_char,
        *const c_char,
    ) -> c_int,
    stream: unsafe extern "C" fn(*mut AVFormatContext, *const AVCodec) -> *mut AVStream,
    parameters: unsafe extern "C" fn(*mut AVCodecParameters, *const AVCodecContext) -> c_int,
    io_open: unsafe extern "C" fn(*mut *mut AVIOContext, *const c_char, c_int) -> c_int,
    io_close: unsafe extern "C" fn(*mut *mut AVIOContext) -> c_int,
    header: unsafe extern "C" fn(*mut AVFormatContext, *mut *mut AVDictionary) -> c_int,
    trailer: unsafe extern "C" fn(*mut AVFormatContext) -> c_int,
    write: unsafe extern "C" fn(*mut AVFormatContext, *mut AVPacket) -> c_int,
    free_format: unsafe extern "C" fn(*mut AVFormatContext),
    send: unsafe extern "C" fn(*mut AVCodecContext, *const AVFrame) -> c_int,
    receive: unsafe extern "C" fn(*mut AVCodecContext, *mut AVPacket) -> c_int,
    rescale: unsafe extern "C" fn(*mut AVPacket, AVRational, AVRational),
    packet_side_data: unsafe extern "C" fn(*mut AVPacket, AVPacketSideDataType, usize) -> *mut u8,
    frame_buffer: unsafe extern "C" fn(*mut AVFrame, c_int) -> c_int,
    frame_writable: unsafe extern "C" fn(*mut AVFrame) -> c_int,
    layout_mask: unsafe extern "C" fn(*mut AVChannelLayout, u64) -> c_int,
    layout_copy: unsafe extern "C" fn(*mut AVChannelLayout, *const AVChannelLayout) -> c_int,
    opt_double: unsafe extern "C" fn(*mut c_void, *const c_char, f64, c_int) -> c_int,
    opt_int: unsafe extern "C" fn(*mut c_void, *const c_char, i64, c_int) -> c_int,
    swr_alloc: unsafe extern "C" fn(
        *mut *mut SwrContext,
        *const AVChannelLayout,
        AVSampleFormat,
        c_int,
        *const AVChannelLayout,
        AVSampleFormat,
        c_int,
        c_int,
        *mut c_void,
    ) -> c_int,
    swr_init: unsafe extern "C" fn(*mut SwrContext) -> c_int,
    swr_convert:
        unsafe extern "C" fn(*mut SwrContext, *mut *mut u8, c_int, *mut *const u8, c_int) -> c_int,
    swr_capacity: unsafe extern "C" fn(*mut SwrContext, c_int) -> c_int,
    swr_free: unsafe extern "C" fn(*mut *mut SwrContext),
}
static API: LazyLock<std::result::Result<EncodeApi, String>> = LazyLock::new(|| {
    (|| -> Result<EncodeApi> {
        let rt = runtime()?;
        let name = format!("libswresample.so.{}", LIBSWRESAMPLE_VERSION_MAJOR);
        let swr = load_library(&[&name], "swresample")?;
        unsafe {
            let version: FVersion = symbol(&swr, b"swresample_version\0")?;
            ensure!(
                version() >> 16 == LIBSWRESAMPLE_VERSION_MAJOR,
                "Unsupported swresample ABI"
            );
            Ok(EncodeApi {
                find: symbol(&rt._codec, b"avcodec_find_encoder_by_name\0")?,
                packet_side_data: symbol(&rt._codec, b"av_packet_new_side_data\0")?,
                guess_format: symbol(&rt._format, b"av_guess_format\0")?,
                output: symbol(&rt._format, b"avformat_alloc_output_context2\0")?,
                stream: symbol(&rt._format, b"avformat_new_stream\0")?,
                parameters: symbol(&rt._codec, b"avcodec_parameters_from_context\0")?,
                io_open: symbol(&rt._format, b"avio_open\0")?,
                io_close: symbol(&rt._format, b"avio_closep\0")?,
                header: symbol(&rt._format, b"avformat_write_header\0")?,
                trailer: symbol(&rt._format, b"av_write_trailer\0")?,
                write: symbol(&rt._format, b"av_interleaved_write_frame\0")?,
                free_format: symbol(&rt._format, b"avformat_free_context\0")?,
                send: symbol(&rt._codec, b"avcodec_send_frame\0")?,
                receive: symbol(&rt._codec, b"avcodec_receive_packet\0")?,
                rescale: symbol(&rt._codec, b"av_packet_rescale_ts\0")?,
                frame_buffer: symbol(&rt._util, b"av_frame_get_buffer\0")?,
                frame_writable: symbol(&rt._util, b"av_frame_make_writable\0")?,
                layout_mask: symbol(&rt._util, b"av_channel_layout_from_mask\0")?,
                layout_copy: symbol(&rt._util, b"av_channel_layout_copy\0")?,
                opt_double: symbol(&rt._util, b"av_opt_set_double\0")?,
                swr_alloc: symbol(&swr, b"swr_alloc_set_opts2\0")?,
                swr_init: symbol(&swr, b"swr_init\0")?,
                opt_int: symbol(&rt._util, b"av_opt_set_int\0")?,
                swr_convert: symbol(&swr, b"swr_convert\0")?,
                swr_capacity: symbol(&swr, b"swr_get_out_samples\0")?,
                swr_free: symbol(&swr, b"swr_free\0")?,
                _swr: swr,
            })
        }
    })()
    .map_err(|e| format!("{e:#}"))
});
fn api() -> Result<&'static EncodeApi> {
    API.as_ref().map_err(|e| anyhow::anyhow!("{e}"))
}
fn checked(code: c_int, operation: &str) -> Result<c_int> {
    if code < 0 {
        return Err(error_text(runtime()?, code, operation));
    }
    Ok(code)
}
fn codec_name(format: AudioExportFormat) -> &'static CStr {
    match format {
        AudioExportFormat::Wav => c"pcm_f32le",
        AudioExportFormat::Flac => c"flac",
        AudioExportFormat::Opus => c"libopus",
        AudioExportFormat::Mp3 => c"libmp3lame",
    }
}
fn muxer_name(format: AudioExportFormat) -> &'static CStr {
    match format {
        AudioExportFormat::Wav => c"wav",
        AudioExportFormat::Flac => c"flac",
        AudioExportFormat::Opus => c"opus",
        AudioExportFormat::Mp3 => c"mp3",
    }
}
pub fn export_encoder_available(format: AudioExportFormat) -> bool {
    if format == AudioExportFormat::Wav {
        return true;
    }
    api().is_ok_and(|a| unsafe {
        let codec = (a.find)(codec_name(format).as_ptr());
        !codec.is_null()
            && (*codec).capabilities & ((1 << 6) | (1 << 16)) != 0
            && !(a.guess_format)(muxer_name(format).as_ptr(), ptr::null(), ptr::null()).is_null()
    })
}
fn layout(mask: u64) -> Result<AVChannelLayout> {
    let mut result = unsafe { std::mem::zeroed() };
    checked(
        unsafe { (api()?.layout_mask)(&mut result, mask) },
        "export channel layout",
    )?;
    Ok(result)
}

/// Return the previous discard count while changing only the end trim. Encoder
/// delay and the two reason bytes remain owned by the encoder.
fn replace_discard_padding(packet: &mut AVPacket, padding: u32) -> Result<u32> {
    let (_, previous) = packet_trims(packet).context("Invalid encoder skip-samples metadata")?;
    let previous = u32::try_from(previous)?;
    for index in 0..packet.side_data_elems as usize {
        // packet_trims validated the side-data array and the unique trim entry.
        let side = unsafe { &mut *packet.side_data.add(index) };
        if side.type_ == AVPacketSideDataType_AV_PKT_DATA_SKIP_SAMPLES {
            unsafe { std::slice::from_raw_parts_mut(side.data.add(4), 4) }
                .copy_from_slice(&padding.to_le_bytes());
            return Ok(previous);
        }
    }
    if padding != 0 {
        let data = unsafe {
            (api()?.packet_side_data)(packet, AVPacketSideDataType_AV_PKT_DATA_SKIP_SAMPLES, 10)
        };
        ensure!(!data.is_null(), "Export packet trim allocation failed");
        let bytes = unsafe { std::slice::from_raw_parts_mut(data, 10) };
        bytes.fill(0);
        bytes[4..8].copy_from_slice(&padding.to_le_bytes());
    }
    Ok(previous)
}

/// Bounded conversion into canonical speaker order. No gain normalization or limiter.
pub struct ExportConverter {
    context: *mut SwrContext,
    input_channels: usize,
    output_channels: usize,
    input_rate: u32,
    output_rate: u32,
    input_frames: u64,
    output_frames: u64,
    last_frame: [f32; 32],
    boundary: Vec<f32>,
    finishing: bool,
    finished: bool,
    buffer: Vec<f32>,
}
impl ExportConverter {
    pub fn new(
        input_rate: u32,
        input_mask: u64,
        output_rate: u32,
        output_mask: u64,
    ) -> Result<Self> {
        ensure!(
            input_rate > 0
                && input_rate <= i32::MAX as u32
                && output_rate > 0
                && output_rate <= i32::MAX as u32,
            "Invalid export conversion sample rate"
        );
        ensure!(
            input_mask != 0
                && output_mask != 0
                && input_mask.count_ones() <= 32
                && output_mask.count_ones() <= 32,
            "Invalid export conversion speaker layout"
        );
        // Pin SWR's filter parameters so the EOF extension has a known bound:
        // two complete filter supports, including downsampling's wider kernel.
        let boundary_frames = if input_rate == output_rate {
            0
        } else {
            (2 * 32 * 100 * u64::from(input_rate.max(output_rate)))
                .div_ceil(97 * u64::from(output_rate))
                + 2
        };
        ensure!(
            boundary_frames <= 8192,
            "Export conversion filter exceeds bounded work"
        );
        let a = api()?;
        let input = layout(input_mask)?;
        let output = layout(output_mask)?;
        let mut this = Self {
            context: ptr::null_mut(),
            input_channels: input.nb_channels as usize,
            output_channels: output.nb_channels as usize,
            input_rate,
            output_rate,
            input_frames: 0,
            output_frames: 0,
            last_frame: [0.0; 32],
            boundary: vec![0.0; boundary_frames as usize * input.nb_channels as usize],
            finishing: false,
            finished: false,
            buffer: Vec::new(),
        };
        unsafe {
            checked(
                (a.swr_alloc)(
                    &mut this.context,
                    &output,
                    AVSampleFormat_AV_SAMPLE_FMT_FLT,
                    output_rate as c_int,
                    &input,
                    AVSampleFormat_AV_SAMPLE_FMT_FLT,
                    input_rate as c_int,
                    0,
                    ptr::null_mut(),
                ),
                "allocate export converter",
            )?;
            checked(
                (a.opt_double)(this.context.cast(), c"rematrix_maxval".as_ptr(), 1000.0, 0),
                "disable remix normalization",
            )?;
            checked(
                (a.opt_int)(this.context.cast(), c"filter_size".as_ptr(), 32, 0),
                "set export resampling filter size",
            )?;
            checked(
                (a.opt_double)(this.context.cast(), c"cutoff".as_ptr(), 0.97, 0),
                "set export resampling cutoff",
            )?;
            checked((a.swr_init)(this.context), "initialize export converter")?;
            let capacity = checked(
                (a.swr_capacity)(this.context, 8192),
                "bound export conversion buffer",
            )? as usize
                + 4096;
            ensure!(
                capacity <= 1_000_000,
                "Export conversion exceeds bounded buffer"
            );
            this.buffer.resize(capacity * this.output_channels, 0.0);
        }
        Ok(this)
    }
    /// Convert at most 8192 real input frames. Empty input is a no-op, not EOF.
    pub fn convert(&mut self, samples: &[f32]) -> Result<&[f32]> {
        ensure!(!self.finishing, "Export converter already finalized");
        ensure!(
            samples.len().is_multiple_of(self.input_channels),
            "Incomplete export conversion frame"
        );
        let frames = samples.len() / self.input_channels;
        ensure!(
            frames <= 8192,
            "Export conversion block exceeds 8192 frames"
        );
        if frames == 0 {
            return Ok(&[]);
        }
        self.input_frames = self
            .input_frames
            .checked_add(frames as u64)
            .context("Export input timeline overflow")?;
        if !self.boundary.is_empty() {
            self.last_frame[..self.input_channels]
                .copy_from_slice(&samples[samples.len() - self.input_channels..]);
        }
        let a = api()?;
        let capacity = checked(
            unsafe { (a.swr_capacity)(self.context, frames as c_int) },
            "export conversion capacity",
        )?;
        ensure!(
            capacity as usize <= self.buffer.len() / self.output_channels,
            "Export conversion exceeds its reserved buffer"
        );
        let mut out = self.buffer.as_mut_ptr().cast::<u8>();
        let mut input = samples.as_ptr().cast::<u8>();
        let count = checked(
            unsafe {
                (a.swr_convert)(
                    self.context,
                    &mut out,
                    capacity,
                    &mut input,
                    frames as c_int,
                )
            },
            "convert export audio",
        )?;
        self.output_frames = self
            .output_frames
            .checked_add(count as u64)
            .context("Export output timeline overflow")?;
        ensure!(
            self.output_frames <= self.target_frames()?,
            "Export converter exceeded the input timeline"
        );
        let output = &self.buffer[..count as usize * self.output_channels];
        ensure!(
            output.iter().all(|x| x.is_finite()),
            "Conversion produced non-finite samples"
        );
        Ok(output)
    }
    /// Emit exactly ceil(real_input_frames * output_rate / input_rate) frames in
    /// total. The half-open source interval therefore never becomes an empty
    /// export merely because it is shorter than one destination sample period.
    fn target_frames(&self) -> Result<u64> {
        u64::try_from(
            (u128::from(self.input_frames) * u128::from(self.output_rate))
                .div_ceil(u128::from(self.input_rate)),
        )
        .context("Export target timeline overflow")
    }

    /// Return the entire real tail once; subsequent calls return an empty slice.
    /// Extend the last input frame before the first flush, allowing even a
    /// one-frame source to initialize SWR's filter. Extension is not source time:
    /// only the remaining target frames are returned, never synthetic padding.
    /// No input is ever submitted after flushing, and a missing tail is an error.
    pub fn finish(&mut self) -> Result<&[f32]> {
        if self.finished {
            return Ok(&[]);
        }
        ensure!(
            !self.finishing,
            "Export converter failed during finalization"
        );
        self.finishing = true;
        let target = self.target_frames()?;
        let remaining = usize::try_from(
            target
                .checked_sub(self.output_frames)
                .context("Export converter exceeded the target timeline")?,
        )?;
        ensure!(
            remaining <= self.buffer.len() / self.output_channels,
            "Export conversion tail exceeds its reserved buffer"
        );
        let a = api()?;
        let mut produced = 0;
        if remaining > 0 && !self.boundary.is_empty() {
            for frame in self.boundary.chunks_exact_mut(self.input_channels) {
                frame.copy_from_slice(&self.last_frame[..self.input_channels]);
            }
            let mut input = self.boundary.as_ptr().cast::<u8>();
            let mut out = self.buffer.as_mut_ptr().cast::<u8>();
            produced = checked(
                unsafe {
                    (a.swr_convert)(
                        self.context,
                        &mut out,
                        remaining as c_int,
                        &mut input,
                        (self.boundary.len() / self.input_channels) as c_int,
                    )
                },
                "extend export resampling boundary",
            )? as usize;
        }
        // Every successful iteration advances the bounded remaining tail. SWR
        // may retain synthetic extension after this; it is deliberately dropped.
        while produced < remaining {
            let mut out = unsafe {
                self.buffer
                    .as_mut_ptr()
                    .add(produced * self.output_channels)
            }
            .cast::<u8>();
            let count = checked(
                unsafe {
                    (a.swr_convert)(
                        self.context,
                        &mut out,
                        (remaining - produced) as c_int,
                        ptr::null_mut(),
                        0,
                    )
                },
                "flush export resampling tail",
            )? as usize;
            ensure!(
                count > 0,
                "Export conversion ended before its target frame count"
            );
            produced += count;
        }
        let output = &self.buffer[..remaining * self.output_channels];
        ensure!(
            output.iter().all(|x| x.is_finite()),
            "Conversion produced non-finite samples"
        );
        self.output_frames = target;
        self.finished = true;
        Ok(output)
    }
}
impl Drop for ExportConverter {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            unsafe { (a.swr_free)(&mut self.context) };
        }
    }
}

/// One muxer/encoder, owned exclusively by the offline export worker.
pub struct AudioEncoder {
    context: *mut AVCodecContext,
    output: *mut AVFormatContext,
    stream: *mut AVStream,
    frame: *mut AVFrame,
    packet: *mut AVPacket,
    // MP3 muxers historically kept only the final packet's discard count.
    // Hold one packet so split tail trims can be emitted once, at true EOF.
    mp3_packet: *mut AVPacket,
    mp3_padding: u32,
    mp3_pending: bool,
    conversion: *mut SwrContext,
    pending: Vec<f32>,
    used: usize,
    channels: usize,
    pts: i64,
    integer_bits: Option<u16>,
    noise: crate::encoding::PcmDither,
    finished: bool,
}
impl AudioEncoder {
    pub fn new(path: &Path, rate: u32, mask: u64, options: &AudioExportEncoding) -> Result<Self> {
        options.validate_stream(rate, mask.count_ones() as u16)?;
        ensure!(
            options.format != AudioExportFormat::Wav,
            "WAV is encoded by hound"
        );
        let a = api()?;
        let rt = runtime()?;
        let codec = unsafe { (a.find)(codec_name(options.format).as_ptr()) };
        ensure!(!codec.is_null(), "Requested export encoder is unavailable");
        let mut this = Self {
            context: ptr::null_mut(),
            output: ptr::null_mut(),
            stream: ptr::null_mut(),
            frame: ptr::null_mut(),
            packet: ptr::null_mut(),
            mp3_packet: ptr::null_mut(),
            mp3_padding: 0,
            mp3_pending: false,
            conversion: ptr::null_mut(),
            pending: Vec::new(),
            used: 0,
            channels: mask.count_ones() as usize,
            pts: 0,
            integer_bits: (options.format == AudioExportFormat::Flac).then_some(options.pcm_bits),
            noise: crate::encoding::PcmDither::default(),
            finished: false,
        };
        let path = CString::new(path.as_os_str().as_bytes())?;
        let muxer = muxer_name(options.format);
        unsafe {
            checked(
                (a.output)(&mut this.output, ptr::null(), muxer.as_ptr(), ptr::null()),
                "allocate export muxer",
            )?;
            ensure!(!this.output.is_null(), "Export muxer allocation failed");
            this.context = (rt.f.alloc_codec)(codec);
            ensure!(!this.context.is_null(), "Export codec allocation failed");
            let ctx = &mut *this.context;
            ctx.sample_rate = rate as c_int;
            ctx.time_base = AVRational {
                num: 1,
                den: rate as c_int,
            };
            ctx.sample_fmt = match options.format {
                AudioExportFormat::Flac if options.pcm_bits == 16 => {
                    AVSampleFormat_AV_SAMPLE_FMT_S16
                }
                AudioExportFormat::Flac => AVSampleFormat_AV_SAMPLE_FMT_S32,
                AudioExportFormat::Mp3 => AVSampleFormat_AV_SAMPLE_FMT_FLTP,
                _ => AVSampleFormat_AV_SAMPLE_FMT_FLT,
            };
            ctx.ch_layout = layout(mask)?;
            ctx.bit_rate = i64::from(options.bitrate_kbps) * 1000;
            if options.format == AudioExportFormat::Flac {
                ctx.bits_per_raw_sample = i32::from(options.pcm_bits);
                ctx.compression_level = i32::from(options.compression);
            }
            // AVFMT_GLOBALHEADER / AV_CODEC_FLAG_GLOBAL_HEADER.
            if (*(*this.output).oformat).flags & 0x40 != 0 {
                ctx.flags |= 1 << 22;
            }
            checked(
                (rt.f.codec_open)(this.context, codec, ptr::null_mut()),
                "open export encoder",
            )?;
            ensure!(
                ctx.sample_rate == rate as c_int
                    && ctx.ch_layout.nb_channels == this.channels as c_int,
                "Encoder changed the requested sample rate or channel layout"
            );
            if matches!(
                options.format,
                AudioExportFormat::Opus | AudioExportFormat::Mp3
            ) {
                ensure!(
                    ctx.bit_rate == i64::from(options.bitrate_kbps) * 1000,
                    "Encoder changed the requested bitrate; choose an explicitly supported value"
                );
            }
            // All supported encoders must accept a short final frame; never pad audio silently.
            ensure!(
                (*codec).capabilities & ((1 << 6) | (1 << 16)) != 0,
                "Encoder cannot preserve a short final audio frame"
            );
            let frame_size = if ctx.frame_size > 0 {
                ctx.frame_size as usize
            } else {
                4096
            };
            ensure!(
                frame_size <= 65536 && this.channels <= 32,
                "Encoder frame exceeds export buffer limit"
            );
            this.pending.resize(frame_size * this.channels, 0.0);
            this.stream = (a.stream)(this.output, ptr::null());
            ensure!(!this.stream.is_null(), "Export stream allocation failed");
            (*this.stream).time_base = ctx.time_base;
            checked(
                (a.parameters)((*this.stream).codecpar, this.context),
                "copy export codec parameters",
            )?;
            checked(
                (a.io_open)(&mut (*this.output).pb, path.as_ptr(), 2),
                "open export temporary file",
            )?;
            checked(
                (a.header)(this.output, ptr::null_mut()),
                "write export header",
            )?;
            this.frame = (rt.f.frame_alloc)();
            this.packet = (rt.f.packet_alloc)();
            if options.format == AudioExportFormat::Mp3 {
                this.mp3_packet = (rt.f.packet_alloc)();
                ensure!(
                    !this.mp3_packet.is_null(),
                    "Export MP3 packet allocation failed"
                );
            }
            ensure!(
                !this.frame.is_null() && !this.packet.is_null(),
                "Export frame allocation failed"
            );
            (*this.frame).format = ctx.sample_fmt as c_int;
            (*this.frame).sample_rate = rate as c_int;
            (*this.frame).nb_samples = frame_size as c_int;
            checked(
                (a.layout_copy)(&mut (*this.frame).ch_layout, &ctx.ch_layout),
                "copy export channel layout",
            )?;
            checked(
                (a.frame_buffer)(this.frame, 0),
                "allocate export frame buffer",
            )?;
            if this.integer_bits.is_none() && ctx.sample_fmt != AVSampleFormat_AV_SAMPLE_FMT_FLT {
                checked(
                    (a.swr_alloc)(
                        &mut this.conversion,
                        &ctx.ch_layout,
                        ctx.sample_fmt,
                        rate as c_int,
                        &ctx.ch_layout,
                        AVSampleFormat_AV_SAMPLE_FMT_FLT,
                        rate as c_int,
                        0,
                        ptr::null_mut(),
                    ),
                    "allocate sample conversion",
                )?;
                checked(
                    (a.swr_init)(this.conversion),
                    "initialize sample conversion",
                )?;
            }
        }
        Ok(this)
    }
    pub fn write(&mut self, mut samples: &[f32]) -> Result<()> {
        ensure!(!self.finished, "Encoder already finalized");
        ensure!(
            samples.len().is_multiple_of(self.channels),
            "Incomplete encoder frame"
        );
        validate_full_scale(samples)?;
        while !samples.is_empty() {
            let count = samples.len().min(self.pending.len() - self.used);
            self.pending[self.used..self.used + count].copy_from_slice(&samples[..count]);
            self.used += count;
            samples = &samples[count..];
            if self.used == self.pending.len() {
                self.encode_pending()?;
            }
        }
        Ok(())
    }
    fn encode_pending(&mut self) -> Result<()> {
        if self.used == 0 {
            return Ok(());
        }
        let a = api()?;
        unsafe {
            checked((a.frame_writable)(self.frame), "make export frame writable")?;
            let count = (self.used / self.channels) as c_int;
            (*self.frame).nb_samples = count;
            (*self.frame).pts = self.pts;
            if let Some(bits) = self.integer_bits {
                if bits == 16 {
                    let output = std::slice::from_raw_parts_mut(
                        (*self.frame).data[0].cast::<i16>(),
                        self.used,
                    );
                    for (out, &sample) in output.iter_mut().zip(&self.pending[..self.used]) {
                        *out = self.noise.quantize(sample, bits) as i16;
                    }
                } else {
                    let output = std::slice::from_raw_parts_mut(
                        (*self.frame).data[0].cast::<i32>(),
                        self.used,
                    );
                    for (out, &sample) in output.iter_mut().zip(&self.pending[..self.used]) {
                        // FLAC drops the low eight bits; quantize before left alignment.
                        *out = self.noise.quantize(sample, bits) << 8;
                    }
                }
            } else if self.conversion.is_null() {
                // The opened codec accepts our interleaved f32 representation.
                // Only `used` samples belong to a short final frame.
                ptr::copy_nonoverlapping(
                    self.pending.as_ptr(),
                    (*self.frame).data[0].cast::<f32>(),
                    self.used,
                );
            } else {
                let mut input = self.pending.as_ptr().cast::<u8>();
                let converted = checked(
                    (a.swr_convert)(
                        self.conversion,
                        (*self.frame).extended_data,
                        count,
                        &mut input,
                        count,
                    ),
                    "convert encoder sample format",
                )?;
                ensure!(
                    converted == count,
                    "Encoder sample conversion buffered unexpected samples"
                );
            }
            checked((a.send)(self.context, self.frame), "send export frame")?;
            self.pts += i64::from(count);
            self.used = 0;
        }
        self.drain(false)
    }
    fn drain(&mut self, flushing: bool) -> Result<()> {
        let a = api()?;
        loop {
            let code = unsafe { (a.receive)(self.context, self.packet) };
            if code == ERR_EOF {
                if self.mp3_pending {
                    replace_discard_padding(unsafe { &mut *self.mp3_packet }, self.mp3_padding)?;
                    self.mux_packet(self.mp3_packet)?;
                    self.mp3_pending = false;
                }
                return Ok(());
            }
            if code == ERR_EAGAIN {
                ensure!(!flushing, "Encoder requested input after flush");
                return Ok(());
            }
            checked(code, "receive encoded packet")?;
            if !self.mp3_packet.is_null() {
                let padding = replace_discard_padding(unsafe { &mut *self.packet }, 0)?;
                self.mp3_padding = self
                    .mp3_padding
                    .checked_add(padding)
                    .context("MP3 encoder discard padding overflow")?;
                if self.mp3_pending {
                    self.mux_packet(self.mp3_packet)?;
                }
                std::mem::swap(&mut self.packet, &mut self.mp3_packet);
                self.mp3_pending = true;
            } else {
                self.mux_packet(self.packet)?;
            }
        }
    }
    fn mux_packet(&mut self, packet: *mut AVPacket) -> Result<()> {
        let a = api()?;
        let rt = runtime()?;
        unsafe {
            (a.rescale)(packet, (*self.context).time_base, (*self.stream).time_base);
            (*packet).stream_index = (*self.stream).index;
            let written = (a.write)(self.output, packet);
            (rt.f.packet_unref)(packet);
            checked(written, "mux encoded packet")?;
        }
        Ok(())
    }
    pub fn finish(&mut self) -> Result<()> {
        ensure!(!self.finished, "Encoder already finalized");
        self.encode_pending()?;
        let a = api()?;
        checked(
            unsafe { (a.send)(self.context, ptr::null()) },
            "flush export encoder",
        )?;
        self.drain(true)?;
        checked(unsafe { (a.trailer)(self.output) }, "write export trailer")?;
        checked(
            unsafe { (a.io_close)(&mut (*self.output).pb) },
            "close export output",
        )?;
        self.finished = true;
        Ok(())
    }
}
impl Drop for AudioEncoder {
    fn drop(&mut self) {
        if let (Ok(a), Ok(rt)) = (api(), runtime()) {
            unsafe {
                (a.swr_free)(&mut self.conversion);
                (rt.f.frame_free)(&mut self.frame);
                (rt.f.packet_free)(&mut self.packet);
                (rt.f.packet_free)(&mut self.mp3_packet);
                (rt.f.free_codec)(&mut self.context);
                if !self.output.is_null() {
                    if !(*self.output).pb.is_null() {
                        (a.io_close)(&mut (*self.output).pb);
                    }
                    (a.free_format)(self.output);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PcmBudget, SourceRenderer};

    // The FFmpeg verification lane opts in to all three export codecs. Other
    // builds can have a decoder runtime without SWR or the optional encoders.
    fn export_capability(available: bool, capability: impl std::fmt::Display) -> bool {
        if !available {
            assert!(
                std::env::var_os("RIVU_TEST_REQUIRE_FFMPEG_EXPORT").is_none(),
                "missing required FFmpeg export capability: {capability}"
            );
            eprintln!("skipping unavailable FFmpeg export capability: {capability}");
        }
        available
    }

    fn encoder_available(format: AudioExportFormat) -> bool {
        export_capability(export_encoder_available(format), format_args!("{format:?}"))
    }

    fn stereo_roundtrip(
        path: &Path,
        format: AudioExportFormat,
        bits: u16,
        samples: &[f32],
        chunk: usize,
    ) -> crate::PcmAudio {
        let options = AudioExportEncoding {
            format,
            pcm_bits: bits,
            ..Default::default()
        };
        let mut encoder = AudioEncoder::new(path, 48_000, 3, &options).unwrap();
        for block in samples.chunks(chunk) {
            encoder.write(block).unwrap();
        }
        encoder.finish().unwrap();
        drop(encoder);
        let cancel = AtomicBool::new(false);
        let renderer = SourceRenderer::open(
            path,
            None,
            rivu_format::decoder_hint::DecoderHint::Ffmpeg,
            1,
            true,
            &cancel,
        )
        .unwrap();
        assert_eq!(renderer.format().sample_rate, 48_000);
        assert_eq!(
            &*renderer.format().layout,
            &[Channel::FrontLeft, Channel::FrontRight]
        );
        renderer
            .render_pcm(PcmBudget::new(8 * 1024 * 1024), &cancel)
            .unwrap()
    }
    #[test]
    fn flac_integer_quantization_is_unbiased_and_block_invariant() {
        if !encoder_available(AudioExportFormat::Flac) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        for bits in [16, 24] {
            let scale = (1_u32 << (bits - 1)) as f32;
            let samples = (0..65_536)
                .map(|i| [0.0, 0.25 / scale, -0.25 / scale, 0.0][i % 4])
                .collect::<Vec<_>>();
            let render = |chunk_size: usize| {
                let path = directory.path().join(format!("{bits}-{chunk_size}.flac"));
                let pcm =
                    stereo_roundtrip(&path, AudioExportFormat::Flac, bits, &samples, chunk_size);
                let mut decoded = vec![0.0; samples.len()];
                assert_eq!(pcm.frames(), (samples.len() / 2) as u64);
                assert_eq!(pcm.read_frames(0, &mut decoded), samples.len() / 2);
                decoded
            };
            let decoded = render(samples.len());
            assert_eq!(decoded, render(74));
            let mean_error = decoded
                .iter()
                .zip(&samples)
                .map(|(&output, &input)| f64::from(output - input) * f64::from(scale))
                .sum::<f64>()
                / samples.len() as f64;
            assert!(
                mean_error.abs() < 0.02,
                "{bits}-bit FLAC bias: {mean_error} LSB"
            );
            assert!(decoded.iter().zip(&samples).all(|(&output, &input)| {
                (output * scale).fract() == 0.0 && ((output - input) * scale).abs() <= 1.5
            }));
        }
    }

    #[test]
    fn supported_encoders_preserve_short_tails_and_channel_identity() {
        for (format, bits) in [
            (AudioExportFormat::Flac, 16),
            (AudioExportFormat::Flac, 24),
            (AudioExportFormat::Opus, 32),
            (AudioExportFormat::Mp3, 32),
        ] {
            if !encoder_available(format) {
                continue;
            }
            for frames in [1, 31, 46, 47, 48, 1151, 1152, 1153, 1198, 1199, 1200, 48013] {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("encoded.audio");
                let samples = (0..frames)
                    .flat_map(|i| {
                        let value = (i as f32 * 0.071).sin() * 0.25;
                        [value, -value * 0.5]
                    })
                    .collect::<Vec<_>>();
                let pcm = stereo_roundtrip(&path, format, bits, &samples, 74);
                assert_eq!(
                    pcm.frames(),
                    frames as u64,
                    "{format:?} trimmed/padded a short tail"
                );
                if format == AudioExportFormat::Flac {
                    let mut decoded = vec![0.0; samples.len()];
                    assert_eq!(pcm.read_frames(0, &mut decoded), frames);
                    let scale = (1_u32 << (bits - 1)) as f32;
                    assert!(
                        decoded
                            .iter()
                            .all(|&sample| (sample * scale).fract() == 0.0)
                    );
                    assert!(
                        decoded
                            .iter()
                            .zip(&samples)
                            .all(|(&a, &b)| (a - b).abs() <= 2.0 / scale),
                        "FLAC did not preserve dithered integer PCM precision"
                    );
                }
            }
        }
    }

    #[test]
    fn converter_drains_resampling_tail_and_does_not_normalize_stereo_to_mono() {
        if !export_capability(api().is_ok(), "converter runtime") {
            return;
        }
        let mut converter = ExportConverter::new(44_100, 3, 48_000, 3).unwrap();
        let mut frames = 0;
        for _ in 0..100 {
            let output = converter.convert(&[0.125; 882]).unwrap();
            frames += output.len() / 2;
        }
        frames += converter.finish().unwrap().len() / 2;
        assert_eq!(frames, 48_000);
        let mut mono = ExportConverter::new(48_000, 3, 48_000, 4).unwrap();
        let output = mono.convert(&[0.5, 0.5, 0.5, 0.5]).unwrap();
        assert_eq!(output.len(), 2);
        assert!(
            output
                .iter()
                .all(|&s| (s - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6)
        );
    }

    fn converted_chunks(
        input_rate: u32,
        output_rate: u32,
        input: &[f32],
        chunk: usize,
    ) -> Vec<f32> {
        let mut converter = ExportConverter::new(input_rate, 3, output_rate, 3).unwrap();
        let mut output = Vec::new();
        assert!(converter.convert(&[]).unwrap().is_empty());
        for samples in input.chunks(chunk * 2) {
            output.extend_from_slice(converter.convert(samples).unwrap());
        }
        output.extend_from_slice(converter.finish().unwrap());
        let target =
            (input.len() as u64 / 2 * u64::from(output_rate)).div_ceil(u64::from(input_rate));
        assert_eq!(output.len() as u64 / 2, target);
        assert_eq!(converter.input_frames, input.len() as u64 / 2);
        assert_eq!(converter.output_frames, target);
        assert!(converter.finish().unwrap().is_empty());
        assert!(converter.convert(&[0.125, -0.25]).is_err());
        output
    }

    #[test]
    fn converter_preserves_constant_sources_at_filter_and_block_boundaries() {
        if !export_capability(api().is_ok(), "converter runtime") {
            return;
        }
        for (input_rate, output_rate) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (8_000, 48_000),
            (48_000, 8_000),
            (48_000, 48_000),
        ] {
            for frames in [
                0, 1, 2, 15, 16, 17, 31, 32, 33, 34, 35, 63, 64, 65, 197, 198, 199, 8191, 8192,
                8193,
            ] {
                let input = [0.125, -0.25].repeat(frames);
                for chunk in [1, 17, 8192] {
                    let output = converted_chunks(input_rate, output_rate, &input, chunk);
                    assert_eq!(output.is_empty(), frames == 0);
                    assert!(
                        output.as_chunks::<2>().0.iter().all(|frame| {
                            (frame[0] - 0.125).abs() < 1e-5 && (frame[1] + 0.25).abs() < 1e-5
                        }),
                        "{input_rate}->{output_rate}, {frames} frames, block {chunk}"
                    );
                }
            }
        }
    }

    #[test]
    fn converter_output_is_independent_of_input_partitioning() {
        if !export_capability(api().is_ok(), "converter runtime") {
            return;
        }
        let input = (0..16385)
            .flat_map(|i| {
                [
                    (i as f32 * 0.071).sin() * 0.25,
                    (i as f32 * 0.031).cos() * 0.125,
                ]
            })
            .collect::<Vec<_>>();
        for (input_rate, output_rate) in [(44_100, 48_000), (48_000, 44_100)] {
            let expected = converted_chunks(input_rate, output_rate, &input, 8192);
            for chunk in [1, 31, 8191] {
                let actual = converted_chunks(input_rate, output_rate, &input, chunk);
                assert_eq!(actual.len(), expected.len());
                assert!(
                    actual
                        .iter()
                        .zip(&expected)
                        .all(|(a, b)| (a - b).abs() < 1e-6)
                );
            }
        }
    }

    #[test]
    fn converter_rejects_a_missing_tail_instead_of_publishing_it() {
        if !export_capability(api().is_ok(), "converter runtime") {
            return;
        }
        let mut converter = ExportConverter::new(44_100, 3, 48_000, 3).unwrap();
        converter.convert(&[0.125, -0.25]).unwrap();
        // Simulate lost buffered source frames without replacing SWR: the real
        // resampler must exhaust before this deliberately inconsistent timeline.
        converter.input_frames += 8192;
        let error = converter.finish().unwrap_err();
        assert!(error.to_string().contains("before its target frame count"));
        assert!(converter.finish().is_err());
        assert!(converter.convert(&[0.125, -0.25]).is_err());
    }

    #[test]
    fn lossy_encoder_rejects_overload_and_nonfinite_input() {
        if !encoder_available(AudioExportFormat::Opus) {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let options = AudioExportEncoding {
            format: AudioExportFormat::Opus,
            ..Default::default()
        };
        let mut encoder =
            AudioEncoder::new(&directory.path().join("out.opus"), 48_000, 4, &options).unwrap();
        assert!(encoder.write(&[1.01]).is_err());
        assert!(encoder.write(&[f32::NAN]).is_err());
    }
    #[test]
    fn split_mp3_discard_padding_is_emitted_once_without_changing_encoder_delay() {
        let mut total = 0_u32;
        let mut published_padding = Vec::new();
        // A one-sample MPEG-1 stream has 1105 samples of decoder/encoder delay;
        // its 1198-sample tail is split across two 1152-sample packets by libavcodec.
        for (index, discard) in [46_u32, 1152].into_iter().enumerate() {
            let delay = if index == 0 { 1105_u32 } else { 0 };
            let mut bytes = [0_u8; 10];
            bytes[..4].copy_from_slice(&delay.to_le_bytes());
            bytes[4..8].copy_from_slice(&discard.to_le_bytes());
            bytes[8..].copy_from_slice(&[1, 2]);
            let mut side: AVPacketSideData = unsafe { std::mem::zeroed() };
            side.data = bytes.as_mut_ptr();
            side.size = bytes.len();
            side.type_ = AVPacketSideDataType_AV_PKT_DATA_SKIP_SAMPLES;
            let mut packet: AVPacket = unsafe { std::mem::zeroed() };
            packet.side_data = &mut side;
            packet.side_data_elems = 1;
            total += replace_discard_padding(&mut packet, 0).unwrap();
            if index == 1 {
                replace_discard_padding(&mut packet, total).unwrap();
            }
            assert_eq!(&bytes[..4], &delay.to_le_bytes());
            assert_eq!(&bytes[8..], &[1, 2]);
            published_padding.push(u32::from_le_bytes(bytes[4..8].try_into().unwrap()));
        }
        assert_eq!(published_padding, [0, 1198]);
        // Both overwrite-only muxers and accumulating muxers now publish the same
        // trim total; neither drops the first packet's end trim nor double-counts it.
        let old_muxer = *published_padding.last().unwrap();
        let new_muxer = published_padding.iter().sum::<u32>();
        assert_eq!(old_muxer, new_muxer);
        assert_eq!(2 * 1152 - 1105 - old_muxer, 1);
    }
}
