//! Runtime-loaded FFmpeg audio backend.
//!
//! ABI declarations are generated from development headers into Cargo OUT_DIR.
//! No FFmpeg symbol is linked by the executable.

use super::{Channel, MediaInfo};
use anyhow::{Context, Result, bail};
use libloading::Library;
use std::{
    ffi::{CStr, CString, c_char, c_int},
    path::Path,
    ptr,
    sync::{Arc, Mutex, OnceLock},
};

#[cfg(not(all(target_arch = "x86_64", target_endian = "little")))]
compile_error!("runtime FFmpeg ABI bindings are only validated for little-endian x86_64");
#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code
)]
mod abi {
    include!(concat!(env!("OUT_DIR"), "/ffmpeg_bindings.rs"));
}
use abi::*;

const NOPTS: i64 = i64::MIN;
const ERR_EAGAIN: c_int = -11;
const ERR_EOF: c_int = -541_478_725;

// Function pointers are copied out of Symbols while the owning Library stays in Runtime.
type FOpenInput = unsafe extern "C" fn(
    *mut *mut AVFormatContext,
    *const c_char,
    *const AVInputFormat,
    *mut *mut AVDictionary,
) -> c_int;
type FFindInfo = unsafe extern "C" fn(*mut AVFormatContext, *mut *mut AVDictionary) -> c_int;
type FCloseInput = unsafe extern "C" fn(*mut *mut AVFormatContext);
type FReadFrame = unsafe extern "C" fn(*mut AVFormatContext, *mut AVPacket) -> c_int;
type FSeekFile = unsafe extern "C" fn(*mut AVFormatContext, c_int, i64, i64, i64, c_int) -> c_int;
type FPacketAlloc = unsafe extern "C" fn() -> *mut AVPacket;
type FPacketFree = unsafe extern "C" fn(*mut *mut AVPacket);
type FPacketUnref = unsafe extern "C" fn(*mut AVPacket);
type FFindDecoder = unsafe extern "C" fn(AVCodecID) -> *const AVCodec;
type FCodecIterate = unsafe extern "C" fn(*mut *mut std::ffi::c_void) -> *const AVCodec;
type FCodecIsDecoder = unsafe extern "C" fn(*const AVCodec) -> c_int;
type FAllocCodec = unsafe extern "C" fn(*const AVCodec) -> *mut AVCodecContext;
type FFreeCodec = unsafe extern "C" fn(*mut *mut AVCodecContext);
type FParamsToContext =
    unsafe extern "C" fn(*mut AVCodecContext, *const AVCodecParameters) -> c_int;
type FCodecOpen =
    unsafe extern "C" fn(*mut AVCodecContext, *const AVCodec, *mut *mut AVDictionary) -> c_int;
type FSendPacket = unsafe extern "C" fn(*mut AVCodecContext, *const AVPacket) -> c_int;
type FReceiveFrame = unsafe extern "C" fn(*mut AVCodecContext, *mut AVFrame) -> c_int;
type FFlush = unsafe extern "C" fn(*mut AVCodecContext);
type FFrameAlloc = unsafe extern "C" fn() -> *mut AVFrame;
type FFrameFree = unsafe extern "C" fn(*mut *mut AVFrame);
type FFrameUnref = unsafe extern "C" fn(*mut AVFrame);
type FDictGet = unsafe extern "C" fn(
    *const AVDictionary,
    *const c_char,
    *const AVDictionaryEntry,
    c_int,
) -> *const AVDictionaryEntry;
type FDictSet =
    unsafe extern "C" fn(*mut *mut AVDictionary, *const c_char, *const c_char, c_int) -> c_int;
type FDictFree = unsafe extern "C" fn(*mut *mut AVDictionary);
type FStrError = unsafe extern "C" fn(c_int, *mut c_char, usize) -> c_int;
type FCodecName = unsafe extern "C" fn(AVCodecID) -> *const c_char;
type FProfileName = unsafe extern "C" fn(AVCodecID, c_int) -> *const c_char;
type FChannelFromIndex = unsafe extern "C" fn(*const AVChannelLayout, u32) -> AVChannel;
type FVersion = unsafe extern "C" fn() -> u32;
struct Functions {
    format_version: FVersion,
    codec_version: FVersion,
    util_version: FVersion,
    open_input: FOpenInput,
    find_info: FFindInfo,
    close_input: FCloseInput,
    read_frame: FReadFrame,
    seek_file: FSeekFile,
    packet_alloc: FPacketAlloc,
    packet_free: FPacketFree,
    packet_unref: FPacketUnref,
    find_decoder: FFindDecoder,
    alloc_codec: FAllocCodec,
    free_codec: FFreeCodec,
    params_to_context: FParamsToContext,
    codec_open: FCodecOpen,
    send_packet: FSendPacket,
    receive_frame: FReceiveFrame,
    flush: FFlush,
    frame_alloc: FFrameAlloc,
    frame_free: FFrameFree,
    frame_unref: FFrameUnref,
    dict_get: FDictGet,
    dict_set: FDictSet,
    dict_free: FDictFree,
    strerror: FStrError,
    codec_name: FCodecName,
    profile_name: FProfileName,
    channel_from_index: FChannelFromIndex,
}
struct Runtime {
    // Field order is intentional: Functions never outlive these libraries.
    _util: Library,
    _codec: Library,
    _format: Library,
    f: Functions,
    audio_codecs: CString,
}
unsafe impl Send for Runtime {}
unsafe impl Sync for Runtime {}
static RUNTIME: OnceLock<Mutex<Option<Arc<Runtime>>>> = OnceLock::new();

unsafe fn symbol<T: Copy>(library: &Library, name: &[u8]) -> Result<T> {
    // Every caller supplies an extern function pointer. Option preserves its
    // nullable ABI and lets us reject a successful lookup with a null address.
    let value = *unsafe { library.get::<Option<T>>(name) }
        .with_context(|| format!("missing FFmpeg symbol {}", String::from_utf8_lossy(name)))?;
    value.with_context(|| {
        format!(
            "missing FFmpeg symbol {} (null address)",
            String::from_utf8_lossy(name)
        )
    })
}
fn load_library(names: &[&str], component: &str) -> Result<Library> {
    let mut errors = Vec::new();
    for name in names {
        // SAFETY: Loading a named shared object is the sole unsafe operation here;
        // symbols are checked below before they are called.
        match unsafe { Library::new(name) } {
            Ok(library) => return Ok(library),
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }
    bail!(
        "FFmpeg {component} library unavailable ({})",
        errors.join(", ")
    )
}
fn load_runtime() -> Result<Arc<Runtime>> {
    let expected = (
        LIBAVUTIL_VERSION_MAJOR,
        LIBAVCODEC_VERSION_MAJOR,
        LIBAVFORMAT_VERSION_MAJOR,
    );
    let util_name = format!("libavutil.so.{}", expected.0);
    let codec_name = format!("libavcodec.so.{}", expected.1);
    let format_name = format!("libavformat.so.{}", expected.2);
    let util = load_library(&[&util_name, "libavutil.so"], "libavutil")?;
    let codec = load_library(&[&codec_name, "libavcodec.so"], "libavcodec")?;
    let format = load_library(&[&format_name, "libavformat.so"], "libavformat")?;
    // Check major ABI families before resolving any operational symbol.
    let uv: FVersion = unsafe { symbol(&util, b"avutil_version\0")? };
    let cv: FVersion = unsafe { symbol(&codec, b"avcodec_version\0")? };
    let fv: FVersion = unsafe { symbol(&format, b"avformat_version\0")? };
    let (um, cm, fm) = (
        unsafe { uv() } >> 16,
        unsafe { cv() } >> 16,
        unsafe { fv() } >> 16,
    );
    if (um, cm, fm) != expected {
        bail!(
            "unsupported FFmpeg ABI (libavutil {um}, libavcodec {cm}, libavformat {fm}); expected {}/{}/{}",
            expected.0,
            expected.1,
            expected.2
        )
    }
    macro_rules! s {
        ($lib:expr, $name:literal, $ty:ty) => {
            unsafe { symbol::<$ty>($lib, concat!($name, "\0").as_bytes())? }
        };
    }
    let f = Functions {
        format_version: fv,
        codec_version: cv,
        util_version: uv,
        open_input: s!(&format, "avformat_open_input", FOpenInput),
        find_info: s!(&format, "avformat_find_stream_info", FFindInfo),
        close_input: s!(&format, "avformat_close_input", FCloseInput),
        read_frame: s!(&format, "av_read_frame", FReadFrame),
        seek_file: s!(&format, "avformat_seek_file", FSeekFile),
        packet_alloc: s!(&codec, "av_packet_alloc", FPacketAlloc),
        packet_free: s!(&codec, "av_packet_free", FPacketFree),
        packet_unref: s!(&codec, "av_packet_unref", FPacketUnref),
        find_decoder: s!(&codec, "avcodec_find_decoder", FFindDecoder),
        alloc_codec: s!(&codec, "avcodec_alloc_context3", FAllocCodec),
        free_codec: s!(&codec, "avcodec_free_context", FFreeCodec),
        params_to_context: s!(&codec, "avcodec_parameters_to_context", FParamsToContext),
        codec_open: s!(&codec, "avcodec_open2", FCodecOpen),
        send_packet: s!(&codec, "avcodec_send_packet", FSendPacket),
        receive_frame: s!(&codec, "avcodec_receive_frame", FReceiveFrame),
        flush: s!(&codec, "avcodec_flush_buffers", FFlush),
        frame_alloc: s!(&util, "av_frame_alloc", FFrameAlloc),
        frame_free: s!(&util, "av_frame_free", FFrameFree),
        frame_unref: s!(&util, "av_frame_unref", FFrameUnref),
        dict_get: s!(&util, "av_dict_get", FDictGet),
        dict_set: s!(&util, "av_dict_set", FDictSet),
        dict_free: s!(&util, "av_dict_free", FDictFree),
        strerror: s!(&util, "av_strerror", FStrError),
        codec_name: s!(&codec, "avcodec_get_name", FCodecName),
        profile_name: s!(&codec, "avcodec_profile_name", FProfileName),
        channel_from_index: s!(
            &util,
            "av_channel_layout_channel_from_index",
            FChannelFromIndex
        ),
    };
    let iterate = s!(&codec, "av_codec_iterate", FCodecIterate);
    let is_decoder = s!(&codec, "av_codec_is_decoder", FCodecIsDecoder);
    let mut opaque = ptr::null_mut();
    let mut names = Vec::new();
    loop {
        let entry = unsafe { iterate(&mut opaque) };
        if entry.is_null() {
            break;
        }
        let entry = unsafe { &*entry };
        if entry.type_ != AVMediaType_AVMEDIA_TYPE_AUDIO || unsafe { is_decoder(entry) } == 0 {
            continue;
        }
        if entry.name.is_null() {
            bail!("FFmpeg audio decoder has no name");
        }
        if !names.is_empty() {
            names.push(b',');
        }
        names.extend_from_slice(unsafe { CStr::from_ptr(entry.name) }.to_bytes());
    }
    if names.is_empty() {
        bail!("FFmpeg runtime has no audio decoders");
    }
    let audio_codecs = CString::new(names)?;
    Ok(Arc::new(Runtime {
        _util: util,
        _codec: codec,
        _format: format,
        f,
        audio_codecs,
    }))
}
fn runtime() -> Result<Arc<Runtime>> {
    let lock = RUNTIME.get_or_init(|| Mutex::new(None));
    let mut guard = lock
        .lock()
        .map_err(|_| anyhow::anyhow!("FFmpeg runtime lock poisoned"))?;
    if let Some(existing) = guard.as_ref() {
        return Ok(existing.clone());
    }
    let loaded = load_runtime()?;
    *guard = Some(loaded.clone());
    Ok(loaded)
}
pub(super) fn availability() -> Result<String> {
    let rt = runtime()?;
    let f = &rt.f;
    Ok(format!(
        "FFmpeg runtime available (libavcodec {}.{}.{} / libavformat {}.{}.{} / libavutil {}.{}.{}; audio)",
        unsafe { (f.codec_version)() >> 16 },
        unsafe { ((f.codec_version)() >> 8) & 255 },
        unsafe { (f.codec_version)() & 255 },
        unsafe { (f.format_version)() >> 16 },
        unsafe { ((f.format_version)() >> 8) & 255 },
        unsafe { (f.format_version)() & 255 },
        unsafe { (f.util_version)() >> 16 },
        unsafe { ((f.util_version)() >> 8) & 255 },
        unsafe { (f.util_version)() & 255 }
    ))
}
fn error_text(rt: &Runtime, code: c_int, operation: &str) -> anyhow::Error {
    let mut buf = [0i8; 256];
    let text = unsafe { (rt.f.strerror)(code, buf.as_mut_ptr(), buf.len()) };
    let detail = if text == 0 {
        CStr::from_bytes_until_nul(unsafe {
            std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len())
        })
        .map(|v| v.to_string_lossy())
        .unwrap_or_default()
        .into_owned()
    } else {
        format!("error {code}")
    };
    anyhow::anyhow!("FFmpeg {operation} failed: {detail} ({code})")
}
fn cstr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}
fn tag(rt: &Runtime, dict: *const AVDictionary, key: &str) -> Option<String> {
    let key = CString::new(key).ok()?;
    let entry = unsafe { (rt.f.dict_get)(dict, key.as_ptr(), ptr::null(), 0) };
    if entry.is_null() {
        None
    } else {
        Some(cstr(unsafe { (*entry).value }))
    }
}
fn rational_seconds(value: i64, q: AVRational) -> Option<f64> {
    if value == NOPTS || q.den == 0 {
        None
    } else {
        Some(value as f64 * q.num as f64 / q.den as f64)
    }
}

pub(super) struct Decoder {
    rt: Arc<Runtime>,
    format: *mut AVFormatContext,
    codec: *mut AVCodecContext,
    packet: *mut AVPacket,
    frame: *mut AVFrame,
    stream: c_int,
    time_base: AVRational,
    start_time: i64,
    draining: bool,
    require_timestamp: bool,
    next_sample: i64,
    frames: Vec<f32>,
    primed: Option<f64>,
    layout: Vec<Channel>,
    info: MediaInfo,
}
unsafe impl Send for Decoder {}
impl Decoder {
    pub(super) fn open(path: &Path) -> Result<Self> {
        let path =
            std::fs::canonicalize(path).with_context(|| format!("Opening {}", path.display()))?;
        let metadata = std::fs::metadata(&path)?;
        if !metadata.is_file() {
            bail!("FFmpeg backend only accepts regular local files");
        }
        let rt = runtime()?;
        let cpath = CString::new(path.as_os_str().as_encoded_bytes())
            .context("FFmpeg path contains NUL")?;
        let mut format = ptr::null_mut();
        let mut options = ptr::null_mut();
        for (key, value) in [
            (c"protocol_whitelist", c"file"),
            (c"codec_whitelist", rt.audio_codecs.as_c_str()),
            (c"probesize", c"33554432"),
            (c"analyzeduration", c"5000000"),
        ] {
            let ret = unsafe { (rt.f.dict_set)(&mut options, key.as_ptr(), value.as_ptr(), 0) };
            if ret < 0 {
                unsafe {
                    (rt.f.dict_free)(&mut options);
                }
                bail!("FFmpeg could not set input option {key:?}");
            }
        }
        let ret =
            unsafe { (rt.f.open_input)(&mut format, cpath.as_ptr(), ptr::null(), &mut options) };
        unsafe {
            (rt.f.dict_free)(&mut options);
        }
        if ret < 0 {
            return Err(error_text(&rt, ret, "open input"));
        }
        let mut close_on_error = true;
        let result = (|| {
            // Disable every non-audio stream before stream-info analysis. This prevents
            // an audio-only player from implicitly opening a video decoder.
            let ctx = unsafe { &mut *format };
            for index in 0..ctx.nb_streams {
                let candidate = unsafe { *ctx.streams.add(index as usize) };
                if !candidate.is_null()
                    && !unsafe { (*candidate).codecpar }.is_null()
                    && unsafe { (*(*candidate).codecpar).codec_type }
                        != AVMediaType_AVMEDIA_TYPE_AUDIO
                {
                    unsafe {
                        (*candidate).discard = AVDiscard_AVDISCARD_ALL;
                    }
                }
            }
            let ret = unsafe { (rt.f.find_info)(format, ptr::null_mut()) };
            if ret < 0 {
                return Err(error_text(&rt, ret, "find stream info"));
            }
            let ctx = unsafe { &*format };
            for index in 0..ctx.nb_streams {
                let candidate = unsafe { *ctx.streams.add(index as usize) };
                if !candidate.is_null()
                    && !unsafe { (*candidate).codecpar }.is_null()
                    && unsafe { (*(*candidate).codecpar).codec_type }
                        != AVMediaType_AVMEDIA_TYPE_AUDIO
                {
                    unsafe {
                        (*candidate).discard = AVDiscard_AVDISCARD_ALL;
                    }
                }
            }
            let mut stream = None;
            for index in 0..ctx.nb_streams {
                let candidate = unsafe { *ctx.streams.add(index as usize) };
                if candidate.is_null()
                    || unsafe { (*candidate).codecpar }.is_null()
                    || unsafe { (*(*candidate).codecpar).codec_type }
                        != AVMediaType_AVMEDIA_TYPE_AUDIO
                {
                    continue;
                }
                stream = Some((index as c_int, candidate));
                break;
            }
            let (stream_index, stream_ptr) = stream.context("FFmpeg input has no audio stream")?;
            let params = unsafe { (*stream_ptr).codecpar };
            if params.is_null() {
                bail!("FFmpeg audio stream has no codec parameters");
            }
            let decoder = unsafe { (rt.f.find_decoder)((*params).codec_id) };
            if decoder.is_null() {
                let name = unsafe { (rt.f.codec_name)((*params).codec_id) };
                bail!("FFmpeg decoder unavailable for {}", cstr(name));
            }
            let codec = unsafe { (rt.f.alloc_codec)(decoder) };
            if codec.is_null() {
                bail!("FFmpeg could not allocate codec context");
            }
            let packet = unsafe { (rt.f.packet_alloc)() };
            let frame = unsafe { (rt.f.frame_alloc)() };
            if packet.is_null() || frame.is_null() {
                unsafe {
                    if !frame.is_null() {
                        (rt.f.frame_free)(&mut (frame as *mut AVFrame));
                    }
                    if !packet.is_null() {
                        (rt.f.packet_free)(&mut (packet as *mut AVPacket));
                    }
                    (rt.f.free_codec)(&mut (codec as *mut AVCodecContext));
                }
                bail!("FFmpeg could not allocate decode buffers");
            }
            let ret = unsafe { (rt.f.params_to_context)(codec, params) };
            if ret < 0 {
                unsafe {
                    (rt.f.frame_free)(&mut (frame as *mut AVFrame));
                    (rt.f.packet_free)(&mut (packet as *mut AVPacket));
                    (rt.f.free_codec)(&mut (codec as *mut AVCodecContext));
                }
                return Err(error_text(&rt, ret, "copy codec parameters"));
            }
            let ret = unsafe { (rt.f.codec_open)(codec, decoder, ptr::null_mut()) };
            if ret < 0 {
                unsafe {
                    (rt.f.frame_free)(&mut (frame as *mut AVFrame));
                    (rt.f.packet_free)(&mut (packet as *mut AVPacket));
                    (rt.f.free_codec)(&mut (codec as *mut AVCodecContext));
                }
                return Err(error_text(&rt, ret, "open audio decoder"));
            }
            let p = unsafe { &*params };
            let codec_name = cstr(unsafe { (rt.f.codec_name)(p.codec_id) });
            let profile = cstr(unsafe { (rt.f.profile_name)(p.codec_id, p.profile) });
            let codec_text = if profile.is_empty() || profile == "unknown" {
                codec_name.clone()
            } else {
                format!("{codec_name} ({profile})")
            };
            let stream_ref = unsafe { &*stream_ptr };
            let duration =
                rational_seconds(stream_ref.duration, stream_ref.time_base).or_else(|| {
                    let d = unsafe { (*format).duration };
                    if d == NOPTS {
                        None
                    } else {
                        Some(d as f64 / AV_TIME_BASE as f64)
                    }
                });
            let container_metadata = unsafe { (*format).metadata };
            let title = tag(&rt, stream_ref.metadata, "title")
                .or_else(|| tag(&rt, container_metadata, "title"))
                .unwrap_or_else(|| {
                    path.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                });
            let artist = tag(&rt, stream_ref.metadata, "artist")
                .or_else(|| tag(&rt, container_metadata, "artist"))
                .unwrap_or_default();
            let album = tag(&rt, stream_ref.metadata, "album")
                .or_else(|| tag(&rt, container_metadata, "album"))
                .unwrap_or_default();
            let read_tag = |key| {
                tag(&rt, stream_ref.metadata, key).or_else(|| tag(&rt, container_metadata, key))
            };
            let number_tag = |key| {
                read_tag(key)
                    .and_then(|value| value.split('/').next()?.trim().parse::<u32>().ok())
                    .filter(|value| *value > 0)
            };
            let track_number = number_tag("track");
            let disc_number = number_tag("disc");
            let release_date = read_tag("date")
                .filter(|value| !value.trim().is_empty())
                .or_else(|| read_tag("year").filter(|value| !value.trim().is_empty()));
            let pcm = codec_name.starts_with("pcm_");
            let lossless = matches!(
                codec_name.as_str(),
                "flac" | "alac" | "ape" | "tta" | "truehd" | "mlp"
            );
            let bits_per_sample = if pcm || lossless {
                u32::try_from(p.bits_per_raw_sample)
                    .ok()
                    .filter(|bits| *bits > 0)
                    .or_else(|| {
                        pcm.then(|| {
                            u32::try_from(p.bits_per_coded_sample)
                                .ok()
                                .filter(|bits| *bits > 0)
                        })
                        .flatten()
                    })
            } else {
                None
            };
            let info = MediaInfo {
                title,
                artist,
                album,
                duration,
                bitrate_bps: u64::try_from(p.bit_rate)
                    .ok()
                    .filter(|bitrate| *bitrate > 0),
                track_number,
                disc_number,
                bits_per_sample,
                release_date,
                codec: codec_text,
                channels: 0,
                sample_rate: 0,
            };
            let mut decoder = Self {
                rt: rt.clone(),
                format,
                codec,
                packet,
                frame,
                stream: stream_index,
                time_base: stream_ref.time_base,
                start_time: stream_ref.start_time,
                draining: false,
                require_timestamp: false,
                next_sample: 0,
                frames: Vec::new(),
                layout: Vec::new(),
                info,
                primed: None,
            };
            format = ptr::null_mut();
            close_on_error = false;
            let timestamp = decoder
                .next_frames()?
                .context("FFmpeg audio stream contains no decodable frames")?
                .1;
            decoder.primed = Some(timestamp);
            Ok(decoder)
        })();
        if result.is_err() && close_on_error {
            unsafe {
                (rt.f.close_input)(&mut format);
            }
        }
        if result.is_ok() {
            close_on_error = false;
        }
        let _ = close_on_error;
        result
    }
    pub(super) fn info(&self) -> &MediaInfo {
        &self.info
    }
    pub(super) fn layout(&self) -> &[Channel] {
        &self.layout
    }
    pub(super) fn next_frames(&mut self) -> Result<Option<(&[f32], f64)>> {
        if let Some(timestamp) = self.primed.take() {
            return Ok(Some((&self.frames, timestamp)));
        }
        loop {
            let ret = unsafe { (self.rt.f.receive_frame)(self.codec, self.frame) };
            if ret >= 0 {
                let frame = unsafe { &*self.frame };
                let rate = frame.sample_rate;
                let channels = frame.ch_layout.nb_channels;
                if rate <= 0 || channels <= 0 || frame.nb_samples <= 0 {
                    bail!("FFmpeg returned invalid or unknown audio frame layout");
                }
                if self.layout.is_empty() {
                    for index in 0..channels as u32 {
                        self.layout.push(
                            frame_channel(&self.rt, &frame.ch_layout, index).context(
                                "FFmpeg returned an unknown or unsupported audio channel layout",
                            )?,
                        );
                    }
                    self.info.channels = channels as u16;
                }
                if channels as usize != self.layout.len() {
                    bail!("FFmpeg audio channel layout changed during decode");
                }
                if self.info.sample_rate == 0 {
                    self.info.sample_rate = rate as u32;
                } else if self.info.sample_rate != rate as u32 {
                    bail!("FFmpeg audio sample rate changed during decode");
                }
                for index in 0..channels as u32 {
                    if frame_channel(&self.rt, &frame.ch_layout, index)
                        != Some(self.layout[index as usize])
                    {
                        bail!("FFmpeg audio channel layout changed during decode");
                    }
                }
                let timestamp = if frame.best_effort_timestamp != NOPTS {
                    rational_seconds(
                        frame.best_effort_timestamp
                            - if self.start_time == NOPTS {
                                0
                            } else {
                                self.start_time
                            },
                        self.time_base,
                    )
                    .unwrap_or(0.0)
                } else if self.require_timestamp {
                    bail!("FFmpeg seek produced an audio frame without a timestamp")
                } else {
                    self.next_sample as f64 / rate as f64
                };
                self.require_timestamp = false;
                self.next_sample = ((timestamp.max(0.0) * rate as f64).round() as i64)
                    .saturating_add(frame.nb_samples as i64);
                self.frames
                    .resize(frame.nb_samples as usize * channels as usize, 0.0);
                copy_frame(frame, &mut self.frames)?;
                return Ok(Some((&self.frames, timestamp.max(0.0))));
            }
            if ret != ERR_EAGAIN {
                if ret == ERR_EOF {
                    return Ok(None);
                }
                bail!("{}", error_text(&self.rt, ret, "receive audio frame"));
            }
            if self.draining {
                bail!("FFmpeg decoder requested input while draining");
            }
            let read = unsafe { (self.rt.f.read_frame)(self.format, self.packet) };
            if read == ERR_EOF {
                self.draining = true;
                let sent = unsafe { (self.rt.f.send_packet)(self.codec, ptr::null()) };
                if sent < 0 && sent != ERR_EOF {
                    bail!("{}", error_text(&self.rt, sent, "drain decoder"));
                }
            } else if read < 0 {
                bail!("{}", error_text(&self.rt, read, "read audio packet"));
            } else {
                let packet = unsafe { &*self.packet };
                if packet.stream_index != self.stream {
                    unsafe { (self.rt.f.packet_unref)(self.packet) };
                    continue;
                }
                let sent = unsafe { (self.rt.f.send_packet)(self.codec, self.packet) };
                if sent == ERR_EAGAIN {
                    bail!("FFmpeg decoder backpressure while sending audio packet");
                }
                unsafe { (self.rt.f.packet_unref)(self.packet) };
                if sent < 0 {
                    bail!("{}", error_text(&self.rt, sent, "send audio packet"));
                }
            }
        }
    }
    pub(super) fn seek(&mut self, seconds: f64) -> Result<()> {
        if !seconds.is_finite() || seconds < 0.0 {
            bail!("Invalid FFmpeg seek position");
        }
        let preroll = seconds.min(2.0);
        let seek_seconds = seconds - preroll;
        let base = if self.start_time == NOPTS {
            0
        } else {
            self.start_time
        };
        let target = base.saturating_add(
            (seek_seconds / self.time_base.num as f64 * self.time_base.den as f64).round() as i64,
        );
        let ret =
            unsafe { (self.rt.f.seek_file)(self.format, self.stream, i64::MIN, target, target, 0) };
        if ret < 0 {
            return Err(error_text(&self.rt, ret, "seek audio stream"));
        }
        unsafe {
            (self.rt.f.flush)(self.codec);
            (self.rt.f.packet_unref)(self.packet);
            (self.rt.f.frame_unref)(self.frame);
        }
        self.draining = false;
        self.require_timestamp = seconds > 0.0;
        self.next_sample = 0;
        self.frames.clear();
        self.primed = None;
        Ok(())
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            (self.rt.f.frame_free)(&mut self.frame);
            (self.rt.f.packet_free)(&mut self.packet);
            (self.rt.f.free_codec)(&mut self.codec);
            (self.rt.f.close_input)(&mut self.format);
        }
    }
}
fn frame_channel(rt: &Runtime, layout: &AVChannelLayout, index: u32) -> Option<Channel> {
    if layout.order == AVChannelOrder_AV_CHANNEL_ORDER_UNSPEC {
        // Mono and stereo have conventional identities. Multichannel does not:
        // never infer 5.1/6.1/7.1 speaker positions from a channel count.
        return match (layout.nb_channels, index) {
            (1, 0) => Some(Channel::FrontCenter),
            (2, 0) => Some(Channel::FrontLeft),
            (2, 1) => Some(Channel::FrontRight),
            _ => None,
        };
    }
    let id = unsafe { (rt.f.channel_from_index)(layout, index) };
    u32::try_from(id)
        .ok()
        .and_then(Channel::from_standard_index)
}
fn copy_frame(frame: &AVFrame, output: &mut [f32]) -> Result<()> {
    use abi::{
        AVSampleFormat_AV_SAMPLE_FMT_DBL as F64, AVSampleFormat_AV_SAMPLE_FMT_DBLP as F64P,
        AVSampleFormat_AV_SAMPLE_FMT_FLT as F32, AVSampleFormat_AV_SAMPLE_FMT_FLTP as F32P,
        AVSampleFormat_AV_SAMPLE_FMT_S16 as S16, AVSampleFormat_AV_SAMPLE_FMT_S16P as S16P,
        AVSampleFormat_AV_SAMPLE_FMT_S32 as S32, AVSampleFormat_AV_SAMPLE_FMT_S32P as S32P,
        AVSampleFormat_AV_SAMPLE_FMT_S64 as S64, AVSampleFormat_AV_SAMPLE_FMT_S64P as S64P,
        AVSampleFormat_AV_SAMPLE_FMT_U8 as U8, AVSampleFormat_AV_SAMPLE_FMT_U8P as U8P,
    };
    let channels = frame.ch_layout.nb_channels as usize;
    let planar = matches!(frame.format, U8P | S16P | S32P | S64P | F32P | F64P);
    if frame.extended_data.is_null()
        || (!planar && frame.data[0].is_null())
        || (planar
            && (0..channels)
                .any(|channel| unsafe { (*frame.extended_data.add(channel)).is_null() }))
    {
        bail!("FFmpeg returned empty audio data");
    }
    // FFmpeg owns aligned storage for nb_samples complete frames. Dispatch once
    // per frame, rather than checking format and looking up a plane per sample.
    unsafe {
        match frame.format {
            U8 | U8P => copy_samples::<u8>(frame, output, planar, |value| {
                (value as f32 - 128.0) / 128.0
            }),
            S16 | S16P => {
                copy_samples::<i16>(frame, output, planar, |value| value as f32 / 32768.0)
            }
            S32 | S32P => {
                copy_samples::<i32>(frame, output, planar, |value| value as f32 / 2147483648.0)
            }
            S64 | S64P => copy_samples::<i64>(frame, output, planar, |value| {
                value as f32 / 9223372036854775808.0
            }),
            F32 | F32P => copy_samples::<f32>(frame, output, planar, |value| value),
            F64 | F64P => copy_samples::<f64>(frame, output, planar, |value| value as f32),
            _ => bail!("Unsupported FFmpeg sample format {}", frame.format),
        }
    }
    Ok(())
}

unsafe fn copy_samples<T: Copy>(
    frame: &AVFrame,
    output: &mut [f32],
    planar: bool,
    convert: impl Fn(T) -> f32,
) {
    if planar {
        let channels = frame.ch_layout.nb_channels as usize;
        for channel in 0..channels {
            let plane = unsafe {
                std::slice::from_raw_parts(
                    (*frame.extended_data.add(channel)).cast::<T>(),
                    frame.nb_samples as usize,
                )
            };
            for (out, &sample) in output.iter_mut().skip(channel).step_by(channels).zip(plane) {
                *out = convert(sample);
            }
        }
    } else {
        let samples =
            unsafe { std::slice::from_raw_parts(frame.data[0].cast::<T>(), output.len()) };
        for (out, &sample) in output.iter_mut().zip(samples) {
            *out = convert(sample);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_signed_64_bit_samples_are_not_planar() {
        let samples = [i64::MIN, 0, i64::MAX, 1i64 << 62];
        let mut frame: AVFrame = unsafe { std::mem::zeroed() };
        frame.format = AVSampleFormat_AV_SAMPLE_FMT_S64;
        frame.ch_layout.nb_channels = 2;
        frame.nb_samples = 2;
        frame.data[0] = samples.as_ptr().cast_mut().cast();
        frame.extended_data = frame.data.as_mut_ptr();
        let mut output = [0.0; 4];
        copy_frame(&frame, &mut output).unwrap();
        assert_eq!(output, [-1.0, 0.0, 1.0, 0.5]);
    }

    #[test]
    fn planar_samples_preserve_channel_and_frame_order() {
        let left = [0.1f32, 0.2, 0.3];
        let right = [-0.4f32, -0.5, -0.6];
        let mut planes = [
            left.as_ptr().cast_mut().cast(),
            right.as_ptr().cast_mut().cast(),
        ];
        let mut frame: AVFrame = unsafe { std::mem::zeroed() };
        frame.format = AVSampleFormat_AV_SAMPLE_FMT_FLTP;
        frame.ch_layout.nb_channels = 2;
        frame.nb_samples = 3;
        frame.extended_data = planes.as_mut_ptr();
        let mut output = [0.0; 6];
        copy_frame(&frame, &mut output).unwrap();
        assert_eq!(output, [0.1, -0.4, 0.2, -0.5, 0.3, -0.6]);
    }
}
