//! Runtime-loaded FFmpeg audio backend.
//!
//! ABI declarations are generated from development headers into Cargo OUT_DIR.
//! No FFmpeg symbol is linked by the executable.

use super::{
    Channel, MediaInfo,
    bitrate::PacketStats,
    source::{DecodeStep, FrameTiming, SourceIdentity, check_cancelled, finite_sample},
};
use anyhow::{Context, Result, bail};
use libloading::Library;
use std::{
    ffi::{CStr, CString, c_char, c_int},
    fs::File,
    io::ErrorKind,
    os::fd::AsRawFd,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    ptr,
    sync::{LazyLock, atomic::AtomicBool},
};

#[cfg(not(all(
    target_os = "linux",
    target_endian = "little",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
compile_error!("the FFmpeg extension supports little-endian x86_64/aarch64 Linux");
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
pub(super) const SEEK_PREROLL_SECONDS: f64 = 2.0;

const MAX_PACKET_SIDE_DATA: i32 = 64;

// FFmpeg's default decoder applies AV_PKT_DATA_SKIP_SAMPLES. Restore these
// samples to the encoded-duration denominator, never to the audible PCM.
fn packet_trims(packet: &AVPacket) -> Option<(u64, u64)> {
    if !(0..=MAX_PACKET_SIDE_DATA).contains(&packet.side_data_elems)
        || (packet.side_data_elems > 0 && packet.side_data.is_null())
    {
        return None;
    }
    let mut trim = None;
    for index in 0..packet.side_data_elems as usize {
        let side = unsafe { &*packet.side_data.add(index) };
        if side.type_ != AVPacketSideDataType_AV_PKT_DATA_SKIP_SAMPLES {
            continue;
        }
        if trim.is_some() || side.size < 10 || side.data.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(side.data, 10) };
        let start = u32::from_le_bytes(bytes[..4].try_into().ok()?);
        let end = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
        trim = Some((u64::from(start), u64::from(end)));
    }
    Some(trim.unwrap_or((0, 0)))
}

fn packet_trim_samples(packet: &AVPacket) -> Option<u64> {
    packet_trims(packet).map(|(start, end)| start + end)
}

fn automatic_frame_trim(frame: &AVFrame) -> bool {
    if !(0..=MAX_PACKET_SIDE_DATA).contains(&frame.nb_side_data)
        || (frame.nb_side_data > 0 && frame.side_data.is_null())
    {
        return false;
    }
    for index in 0..frame.nb_side_data as usize {
        let side = unsafe { *frame.side_data.add(index) };
        if side.is_null() {
            return false;
        }
        // Manual skip mode exports this side data instead of applying it. Do
        // not double-count: that mode cannot publish our automatic-trim stats.
        if unsafe { (*side).type_ } == AVFrameSideDataType_AV_FRAME_DATA_SKIP_SAMPLES {
            return false;
        }
    }
    true
}

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
type FOptSetQ =
    unsafe extern "C" fn(*mut std::ffi::c_void, *const c_char, AVRational, c_int) -> c_int;
type FStrError = unsafe extern "C" fn(c_int, *mut c_char, usize) -> c_int;
type FCodecName = unsafe extern "C" fn(AVCodecID) -> *const c_char;
type FProfileName = unsafe extern "C" fn(AVCodecID, c_int) -> *const c_char;
type FChannelFromIndex = unsafe extern "C" fn(*const AVChannelLayout, u32) -> AVChannel;
type FVersion = unsafe extern "C" fn() -> u32;
type FLogSetLevel = unsafe extern "C" fn(c_int);
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
    opt_set_q: FOptSetQ,
    strerror: FStrError,
    codec_name: FCodecName,
    profile_name: FProfileName,
    channel_from_index: FChannelFromIndex,
    log_set_level: FLogSetLevel,
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
// Successful libraries stay loaded for the process lifetime; clients only borrow.
// Non-forcing snapshots remain available while an explicit load is in progress.
static RUNTIME: LazyLock<std::result::Result<Runtime, String>> =
    LazyLock::new(|| load_runtime().map_err(|error| format!("{error:#}")));
static LOAD_COMPLETED: LazyLock<(async_channel::Sender<()>, async_channel::Receiver<()>)> =
    LazyLock::new(|| async_channel::bounded(1));

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
    for &name in names {
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
fn load_runtime() -> Result<Runtime> {
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
        opt_set_q: s!(&util, "av_opt_set_q", FOptSetQ),
        strerror: s!(&util, "av_strerror", FStrError),
        codec_name: s!(&codec, "avcodec_get_name", FCodecName),
        profile_name: s!(&codec, "avcodec_profile_name", FProfileName),
        channel_from_index: s!(
            &util,
            "av_channel_layout_channel_from_index",
            FChannelFromIndex
        ),
        log_set_level: s!(&util, "av_log_set_level", FLogSetLevel),
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
    let runtime = Runtime {
        _util: util,
        _codec: codec,
        _format: format,
        f,
        audio_codecs,
    };
    // FFmpeg's default logger writes codec probe failures directly to stderr.
    // Rivu returns the structured error to the scan result instead.
    unsafe { (runtime.f.log_set_level)(-8) } // AV_LOG_QUIET
    Ok(runtime)
}
fn runtime() -> Result<&'static Runtime> {
    let result = LazyLock::force(&RUNTIME);
    // Close only after LazyLock has published success or failure. Every current
    // and future waiter then observes completion without consuming a token.
    LOAD_COMPLETED.0.close();
    result.as_ref().map_err(|error| anyhow::anyhow!("{error}"))
}
pub(super) fn ensure_loaded() -> Result<()> {
    runtime().map(|_| ())
}
pub(super) async fn load_completed() {
    let _ = LOAD_COMPLETED.1.recv().await;
}
#[cfg(test)]
pub(super) fn load_attempted() -> bool {
    // get() stays nonblocking and returns None until initialization is published.
    LazyLock::get(&RUNTIME).is_some()
}
pub(super) fn availability() -> Result<String> {
    let rt = match LazyLock::get(&RUNTIME) {
        None => return Ok("not loaded".to_owned()),
        Some(Ok(rt)) => rt,
        Some(Err(error)) => bail!("{error}"),
    };
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

/// An analyzed input owns its format context until decoding takes it over.
pub(super) struct Input {
    rt: &'static Runtime,
    path: PathBuf,
    identity: SourceIdentity,
    // Keep the descriptor path valid until the format context is closed.
    file: Option<File>,
    format: *mut AVFormatContext,
    stream: c_int,
    stream_ptr: *mut AVStream,
    params: *mut AVCodecParameters,
}
unsafe impl Send for Input {}
impl Input {
    fn from_file(path: PathBuf, file: File, rt: &'static Runtime) -> Result<Self> {
        let identity = SourceIdentity::from_file(&file)?;
        // FFmpeg opens its own independently seekable descriptor to this exact
        // checked object, never the original path that could now name a FIFO.
        let cpath = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        let mut input = Self {
            rt,
            path,
            identity,
            file: Some(file),
            format: ptr::null_mut(),
            stream: 0,
            stream_ptr: ptr::null_mut(),
            params: ptr::null_mut(),
        };
        let rt = input.rt;
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
        let ret = unsafe {
            (rt.f.open_input)(&mut input.format, cpath.as_ptr(), ptr::null(), &mut options)
        };
        unsafe {
            (rt.f.dict_free)(&mut options);
        }
        if ret < 0 {
            return Err(error_text(rt, ret, "open input"));
        }
        // Disable every non-audio stream before stream-info analysis. This prevents
        // an audio-only player from implicitly opening a video decoder.
        let ctx = unsafe { &mut *input.format };
        for index in 0..ctx.nb_streams {
            let candidate = unsafe { *ctx.streams.add(index as usize) };
            if !candidate.is_null()
                && !unsafe { (*candidate).codecpar }.is_null()
                && unsafe { (*(*candidate).codecpar).codec_type } != AVMediaType_AVMEDIA_TYPE_AUDIO
            {
                unsafe {
                    (*candidate).discard = AVDiscard_AVDISCARD_ALL;
                }
            }
        }
        let ret = unsafe { (rt.f.find_info)(input.format, ptr::null_mut()) };
        if ret < 0 {
            return Err(error_text(rt, ret, "find stream info"));
        }
        let ctx = unsafe { &*input.format };
        for index in 0..ctx.nb_streams {
            let candidate = unsafe { *ctx.streams.add(index as usize) };
            if !candidate.is_null()
                && !unsafe { (*candidate).codecpar }.is_null()
                && unsafe { (*(*candidate).codecpar).codec_type } != AVMediaType_AVMEDIA_TYPE_AUDIO
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
                || unsafe { (*(*candidate).codecpar).codec_type } != AVMediaType_AVMEDIA_TYPE_AUDIO
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
        input.stream = stream_index;
        input.stream_ptr = stream_ptr;
        input.params = params;
        Ok(input)
    }
    pub(super) fn is_pcm(&self) -> bool {
        let name = unsafe { (self.rt.f.codec_name)((*self.params).codec_id) };
        !name.is_null()
            && unsafe { CStr::from_ptr(name) }
                .to_bytes()
                .starts_with(b"pcm_")
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        if !self.format.is_null() {
            unsafe {
                (self.rt.f.close_input)(&mut self.format);
            }
        }
    }
}

/// Inspect WAVE only with a loaded runtime, preserving the file for native decoding.
pub(super) fn inspect_wave(path: &Path, file: &File) -> Result<Option<Input>> {
    let rt = match LazyLock::get(&RUNTIME) {
        Some(Ok(rt)) => rt,
        _ => return Ok(None),
    };
    let path = rivu_format::config::logical_path(path)?;
    let mut magic = [0; 12];
    // Positional reading leaves native decoding at byte zero without a seek.
    match file.read_exact_at(&mut magic, 0) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("read WAVE container magic"),
    }
    if !matches!(&magic[..4], b"RIFF" | b"RIFX" | b"RF64" | b"BW64") || &magic[8..] != b"WAVE" {
        return Ok(None);
    }
    Ok(Some(Input::from_file(path, file.try_clone()?, rt)?))
}

pub(super) struct Decoder {
    rt: &'static Runtime,
    identity: SourceIdentity,
    _file: File,
    path: PathBuf,
    format: *mut AVFormatContext,
    codec: *mut AVCodecContext,
    packet: *mut AVPacket,
    frame: *mut AVFrame,
    stream: c_int,
    time_base: AVRational,
    start_time: i64,
    vorbis_rate: Option<u32>,
    vorbis_trims: (u64, u64),
    draining: bool,
    require_timestamp: bool,
    next_sample: i64,
    pending_frame: Option<(usize, FrameTiming)>,
    layout: Vec<Channel>,
    info: MediaInfo,
    packet_stats: PacketStats,
    expected_samples: Option<u64>,
}
unsafe impl Send for Decoder {}
impl Decoder {
    pub(super) fn from_file(
        path: &Path,
        file: File,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Self> {
        check_cancelled(cancelled)?;
        let path = rivu_format::config::logical_path(path)?;
        let input = Input::from_file(path, file, runtime()?)?;
        Self::from_input(input, cancelled)
    }
    pub(super) fn from_input(mut input: Input, cancelled: Option<&AtomicBool>) -> Result<Self> {
        check_cancelled(cancelled)?;
        let rt = input.rt;
        let path = &input.path;
        let format = input.format;
        let stream_index = input.stream;
        let stream_ptr = input.stream_ptr;
        let params = input.params;
        let decoder = unsafe { (rt.f.find_decoder)((*params).codec_id) };
        if decoder.is_null() {
            let name = unsafe { (rt.f.codec_name)((*params).codec_id) };
            bail!("FFmpeg decoder unavailable for {}", cstr(name));
        }
        let mut codec = unsafe { (rt.f.alloc_codec)(decoder) };
        if codec.is_null() {
            bail!("FFmpeg could not allocate codec context");
        }
        let mut packet = unsafe { (rt.f.packet_alloc)() };
        let mut frame = unsafe { (rt.f.frame_alloc)() };
        if packet.is_null() || frame.is_null() {
            unsafe {
                if !frame.is_null() {
                    (rt.f.frame_free)(&mut frame);
                }
                if !packet.is_null() {
                    (rt.f.packet_free)(&mut packet);
                }
                (rt.f.free_codec)(&mut codec);
            }
            bail!("FFmpeg could not allocate decode buffers");
        }
        let ret = unsafe { (rt.f.params_to_context)(codec, params) };
        if ret < 0 {
            unsafe {
                (rt.f.frame_free)(&mut frame);
                (rt.f.packet_free)(&mut packet);
                (rt.f.free_codec)(&mut codec);
            }
            return Err(error_text(rt, ret, "copy codec parameters"));
        }
        // Automatic skip-sample trimming must adjust frame PTS in the stream
        // clock; codec parameters alone do not populate this field.
        let ret = unsafe {
            (rt.f.opt_set_q)(
                codec.cast(),
                c"pkt_timebase".as_ptr(),
                (*stream_ptr).time_base,
                0,
            )
        };
        if ret < 0 {
            unsafe {
                (rt.f.frame_free)(&mut frame);
                (rt.f.packet_free)(&mut packet);
                (rt.f.free_codec)(&mut codec);
            }
            return Err(error_text(rt, ret, "set packet timebase"));
        }
        let ret = unsafe { (rt.f.codec_open)(codec, decoder, ptr::null_mut()) };
        if ret < 0 {
            unsafe {
                (rt.f.frame_free)(&mut frame);
                (rt.f.packet_free)(&mut packet);
                (rt.f.free_codec)(&mut codec);
            }
            return Err(error_text(rt, ret, "open audio decoder"));
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
        let duration = rational_seconds(stream_ref.duration, stream_ref.time_base).or_else(|| {
            let d = unsafe { (*format).duration };
            if d == NOPTS {
                None
            } else {
                Some(d as f64 / AV_TIME_BASE as f64)
            }
        });
        let container_metadata = unsafe { (*format).metadata };
        let title = tag(rt, stream_ref.metadata, "title")
            .or_else(|| tag(rt, container_metadata, "title"))
            .unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        let artist = tag(rt, stream_ref.metadata, "artist")
            .or_else(|| tag(rt, container_metadata, "artist"))
            .unwrap_or_default();
        let album = tag(rt, stream_ref.metadata, "album")
            .or_else(|| tag(rt, container_metadata, "album"))
            .unwrap_or_default();
        let read_tag =
            |key| tag(rt, stream_ref.metadata, key).or_else(|| tag(rt, container_metadata, key));
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
        let pcm_bitrate = if pcm {
            u64::try_from(p.bits_per_coded_sample)
                .ok()
                .and_then(|bits| bits.checked_mul(u64::try_from(p.sample_rate).ok()?))
                .and_then(|bits| bits.checked_mul(u64::try_from(p.ch_layout.nb_channels).ok()?))
                .filter(|bits| *bits > 0)
        } else {
            None
        };
        let info = MediaInfo {
            title,
            artist,
            album,
            duration,
            bitrate_bps: pcm_bitrate.or_else(|| {
                u64::try_from(p.bit_rate)
                    .ok()
                    .filter(|bitrate| *bitrate > 0)
            }),
            // Only encoded PCM width × channels × sample rate is reliable here.
            bitrate_exact: pcm_bitrate.is_some(),
            track_number,
            disc_number,
            bits_per_sample,
            release_date,
            codec: codec_text,
            channels: 0,
            sample_rate: 0,
        };
        let decoder = Self {
            rt,
            identity: input.identity,
            _file: input.file.take().expect("FFmpeg input owns its local file"),
            path: path.clone(),
            format,
            codec,
            packet,
            frame,
            stream: stream_index,
            time_base: stream_ref.time_base,
            start_time: stream_ref.start_time,
            vorbis_rate: (codec_name == "vorbis")
                .then(|| u32::try_from(p.sample_rate).ok().filter(|rate| *rate > 0))
                .flatten(),
            vorbis_trims: (0, 0),
            draining: false,
            require_timestamp: false,
            next_sample: 0,
            pending_frame: None,
            layout: Vec::new(),
            info,
            packet_stats: PacketStats::new(),
            expected_samples: if pcm {
                u64::try_from(stream_ref.duration).ok().and_then(|ticks| {
                    let rate = u128::try_from(p.sample_rate).ok()?;
                    let num = u128::try_from(stream_ref.time_base.num).ok()?;
                    let den = u128::try_from(stream_ref.time_base.den)
                        .ok()
                        .filter(|den| *den > 0)?;
                    let samples = u128::from(ticks).checked_mul(num)?.checked_mul(rate)?;
                    (samples % den == 0)
                        .then(|| u64::try_from(samples / den).ok())
                        .flatten()
                })
            } else if codec_name == "flac" && p.extradata_size == 34 && !p.extradata.is_null() {
                let info = unsafe { std::slice::from_raw_parts(p.extradata, 34) };
                let samples =
                    u64::from_be_bytes(info[10..18].try_into().unwrap()) & ((1_u64 << 36) - 1);
                (samples > 0).then_some(samples)
            } else {
                None
            },
        };
        input.format = ptr::null_mut();
        Ok(decoder)
    }
    pub(super) fn identity(&self) -> SourceIdentity {
        self.identity
    }
    pub(super) fn info(&self) -> &MediaInfo {
        &self.info
    }
    pub(super) fn time_base(&self) -> Result<symphonia::core::units::TimeBase> {
        let numer =
            u32::try_from(self.time_base.num).context("Invalid FFmpeg time base numerator")?;
        let denom =
            u32::try_from(self.time_base.den).context("Invalid FFmpeg time base denominator")?;
        symphonia::core::units::TimeBase::try_new(numer, denom)
            .context("FFmpeg stream has a zero time base")
    }
    /// Stream origin is a timestamp anchor, not a decoded zero-length frame.
    pub(super) fn vorbis_timing(&self) -> Option<(u32, Option<f64>)> {
        self.vorbis_rate
            .map(|rate| (rate, (self.start_time != NOPTS).then_some(0.0)))
    }
    pub(super) fn layout(&self) -> &[Channel] {
        &self.layout
    }
    pub(super) fn completed_bitrate(&self) -> Option<u64> {
        self.packet_stats.bitrate(1, self.info.sample_rate)
    }
    /// Replaces caller-owned PCM only on `Frames`; `Yield` is not EOF.
    pub(super) fn next_frames(
        &mut self,
        frames: &mut Vec<f32>,
        cancelled: Option<&AtomicBool>,
        budget: &mut usize,
    ) -> Result<DecodeStep<FrameTiming>> {
        let read = self.rt.f.read_frame;
        let result =
            self.next_frames_with_reader(frames, cancelled, budget, |format, packet| unsafe {
                read(format, packet)
            });
        if result.is_err() {
            self.packet_stats.invalidate();
        }
        result
    }

    fn next_frames_with_reader(
        &mut self,
        frames: &mut Vec<f32>,
        cancelled: Option<&AtomicBool>,
        budget: &mut usize,
        mut read: impl FnMut(*mut AVFormatContext, *mut AVPacket) -> c_int,
    ) -> Result<DecodeStep<FrameTiming>> {
        check_cancelled(cancelled)?;
        if *budget == 0 {
            return Ok(DecodeStep::Yield);
        }
        loop {
            // Include skipped packets and decoder buffering in the caller's
            // shared work budget, even when playback has no cancellation flag.
            check_cancelled(cancelled)?;
            if *budget == 0 {
                return Ok(DecodeStep::Yield);
            }
            *budget -= 1;
            if self.pending_frame.is_some() {
                return self.copy_pending_frame(frames).map(DecodeStep::Frames);
            }
            let ret = unsafe { (self.rt.f.receive_frame)(self.codec, self.frame) };
            check_cancelled(cancelled)?;
            if ret >= 0 {
                let frame = unsafe { &*self.frame };
                // AV_FRAME_FLAG_CORRUPT (1); the runtime ABI is generated without constants.
                if frame.decode_error_flags != 0
                    || frame.flags & 1 != 0
                    || !automatic_frame_trim(frame)
                {
                    self.packet_stats.invalidate();
                }
                let rate = frame.sample_rate;
                let channels = frame.ch_layout.nb_channels;
                if rate <= 0 || channels <= 0 || frame.nb_samples <= 0 {
                    bail!("FFmpeg returned invalid or unknown audio frame layout");
                }
                if self.layout.is_empty() {
                    for index in 0..channels as u32 {
                        self.layout
                            .push(frame_channel(self.rt, &frame.ch_layout, index).context(
                                "FFmpeg returned an unknown or unsupported audio channel layout",
                            )?);
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
                    if frame_channel(self.rt, &frame.ch_layout, index)
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
                let (leading, trailing) = if self.vorbis_rate.is_some() {
                    self.vorbis_trims
                } else {
                    (0, 0)
                };
                self.pending_frame = Some((
                    0,
                    FrameTiming {
                        timestamp: timestamp - leading as f64 / rate as f64,
                        represented: frame.nb_samples as u64 + leading + trailing,
                        trim: leading as f64 / rate as f64,
                        offset: 0,
                    },
                ));
                self.packet_stats.add_duration(frame.nb_samples as u64);
                return self.copy_pending_frame(frames).map(DecodeStep::Frames);
            }
            if ret != ERR_EAGAIN {
                if ret == ERR_EOF {
                    if self.draining {
                        self.packet_stats.finish(self.expected_samples);
                    }
                    return Ok(DecodeStep::Eof);
                }
                bail!("{}", error_text(self.rt, ret, "receive audio frame"));
            }
            if self.draining {
                bail!("FFmpeg decoder requested input while draining");
            }
            let read = read(self.format, self.packet);
            if let Err(error) = check_cancelled(cancelled) {
                unsafe { (self.rt.f.packet_unref)(self.packet) };
                return Err(error);
            }
            if read == ERR_EOF {
                self.draining = true;
                let sent = unsafe { (self.rt.f.send_packet)(self.codec, ptr::null()) };
                if sent < 0 && sent != ERR_EOF {
                    bail!("{}", error_text(self.rt, sent, "drain decoder"));
                }
            } else if read < 0 {
                bail!("{}", error_text(self.rt, read, "read audio packet"));
            } else {
                let packet = unsafe { &*self.packet };
                if packet.stream_index != self.stream {
                    unsafe { (self.rt.f.packet_unref)(self.packet) };
                    continue;
                }
                // AV_PKT_FLAG_CORRUPT (2).
                if packet.flags & 2 != 0 {
                    self.packet_stats.invalidate();
                }
                match u64::try_from(packet.size)
                    .ok()
                    .zip(packet_trim_samples(packet))
                {
                    Some((bytes, trim)) if packet.duration > 0 => {
                        self.packet_stats.add(bytes, trim)
                    }
                    _ => self.packet_stats.invalidate(),
                }
                if self.vorbis_rate.is_some() {
                    self.vorbis_trims = packet_trims(packet).unwrap_or((0, 0));
                }
                let sent = unsafe { (self.rt.f.send_packet)(self.codec, self.packet) };
                if sent == ERR_EAGAIN {
                    bail!("FFmpeg decoder backpressure while sending audio packet");
                }
                unsafe { (self.rt.f.packet_unref)(self.packet) };
                if sent < 0 {
                    bail!("{}", error_text(self.rt, sent, "send audio packet"));
                }
            }
        }
    }
    // AVFrame remains decoder-owned until every fragment is consumed. This
    // limits our converted PCM allocation/work without rejecting codec blocks
    // larger than a playback round or claiming to bound FFmpeg internals.
    fn copy_pending_frame(&mut self, output: &mut Vec<f32>) -> Result<FrameTiming> {
        let (offset, timing) = self.pending_frame.expect("pending FFmpeg frame");
        let frame = unsafe { &*self.frame };
        let channels = self.layout.len();
        let count =
            (frame.nb_samples as usize - offset).min(super::source::pcm_chunk_frames(channels)?);
        let samples = super::source::reserve_pcm(output, count, channels)?;
        output.resize(samples, 0.0);
        copy_frame(frame, offset, output)?;
        self.pending_frame =
            (offset + count < frame.nb_samples as usize).then_some((offset + count, timing));
        Ok(FrameTiming { offset, ..timing })
    }
    pub(super) fn validate_seek(&self, seconds: f64) -> Result<()> {
        self.seek_timestamp(seconds).map(|_| ())
    }

    fn seek_timestamp(&self, seconds: f64) -> Result<i64> {
        super::validate_seek_position(seconds)?;
        let ticks = ((seconds - SEEK_PREROLL_SECONDS).max(0.0) / self.time_base.num as f64
            * self.time_base.den as f64)
            .round();
        if !ticks.is_finite() || ticks < 0.0 || ticks >= i64::MAX as f64 {
            return Err(super::SeekRejected.into());
        }
        let base = if self.start_time == NOPTS {
            0
        } else {
            self.start_time
        };
        base.checked_add(ticks as i64)
            .ok_or_else(|| super::SeekRejected.into())
    }

    pub(super) fn seek(&mut self, seconds: f64, cancelled: Option<&AtomicBool>) -> Result<()> {
        check_cancelled(cancelled)?;
        let target = self.seek_timestamp(seconds)?;
        self.packet_stats.invalidate();
        // Origin-reaching preroll starts a fresh decoder traversal. Demuxer
        // seeks plus codec flush can lose overlap primers or initial skip data.
        // Reopen the held file object, never its potentially replaced path.
        if seconds <= SEEK_PREROLL_SECONDS {
            let file = self
                ._file
                .try_clone()
                .context("Retaining FFmpeg seek source")?;
            let input = Input::from_file(self.path.clone(), file, self.rt)?;
            let mut replacement = Self::from_input(input, cancelled)?;
            replacement.packet_stats.invalidate();
            *self = replacement;
            return Ok(());
        }
        let ret =
            unsafe { (self.rt.f.seek_file)(self.format, self.stream, i64::MIN, target, target, 0) };
        check_cancelled(cancelled)?;
        if ret == -libc::EPERM || ret == -libc::ENOSYS {
            // No usable demuxer seek: reopen the same object at origin once.
            // Source retains the requested target and discards toward it.
            return self.seek(0.0, cancelled);
        }
        if ret < 0 {
            return Err(error_text(self.rt, ret, "seek audio stream"));
        }
        unsafe {
            (self.rt.f.flush)(self.codec);
            (self.rt.f.packet_unref)(self.packet);
            (self.rt.f.frame_unref)(self.frame);
        }
        self.draining = false;
        self.require_timestamp = seconds > 0.0;
        self.next_sample = 0;
        self.pending_frame = None;
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
fn copy_frame(frame: &AVFrame, offset: usize, output: &mut [f32]) -> Result<()> {
    use abi::{
        AVSampleFormat_AV_SAMPLE_FMT_DBL as F64, AVSampleFormat_AV_SAMPLE_FMT_DBLP as F64P,
        AVSampleFormat_AV_SAMPLE_FMT_FLT as F32, AVSampleFormat_AV_SAMPLE_FMT_FLTP as F32P,
        AVSampleFormat_AV_SAMPLE_FMT_S16 as S16, AVSampleFormat_AV_SAMPLE_FMT_S16P as S16P,
        AVSampleFormat_AV_SAMPLE_FMT_S32 as S32, AVSampleFormat_AV_SAMPLE_FMT_S32P as S32P,
        AVSampleFormat_AV_SAMPLE_FMT_S64 as S64, AVSampleFormat_AV_SAMPLE_FMT_S64P as S64P,
        AVSampleFormat_AV_SAMPLE_FMT_U8 as U8, AVSampleFormat_AV_SAMPLE_FMT_U8P as U8P,
    };
    let channels = frame.ch_layout.nb_channels as usize;
    if channels == 0
        || !output.len().is_multiple_of(channels)
        || offset
            .checked_add(output.len() / channels)
            .is_none_or(|end| end > frame.nb_samples.max(0) as usize)
        || (frame.nb_samples.max(0) as usize)
            .checked_mul(channels)
            .and_then(|samples| samples.checked_mul(std::mem::size_of::<f64>()))
            .is_none_or(|bytes| bytes > isize::MAX as usize)
    {
        bail!("FFmpeg returned an invalid audio frame extent");
    }
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
            U8 | U8P => copy_samples::<u8>(frame, offset, output, planar, |value| {
                (value as f32 - 128.0) / 128.0
            }),
            S16 | S16P => copy_samples::<i16>(frame, offset, output, planar, |value| {
                value as f32 / 32768.0
            }),
            S32 | S32P => copy_samples::<i32>(frame, offset, output, planar, |value| {
                value as f32 / 2147483648.0
            }),
            S64 | S64P => copy_samples::<i64>(frame, offset, output, planar, |value| {
                value as f32 / 9223372036854775808.0
            }),
            F32 | F32P => copy_samples::<f32>(frame, offset, output, planar, finite_sample),
            F64 | F64P => copy_samples::<f64>(frame, offset, output, planar, |value| {
                finite_sample(value as f32)
            }),
            _ => bail!("Unsupported FFmpeg sample format {}", frame.format),
        }
    }
    Ok(())
}

unsafe fn copy_samples<T: Copy>(
    frame: &AVFrame,
    offset: usize,
    output: &mut [f32],
    planar: bool,
    convert: impl Fn(T) -> f32,
) {
    if planar {
        let channels = frame.ch_layout.nb_channels as usize;
        for channel in 0..channels {
            let plane = unsafe {
                std::slice::from_raw_parts(
                    (*frame.extended_data.add(channel)).cast::<T>().add(offset),
                    output.len() / channels,
                )
            };
            for (out, &sample) in output.iter_mut().skip(channel).step_by(channels).zip(plane) {
                *out = convert(sample);
            }
        }
    } else {
        let samples = unsafe {
            std::slice::from_raw_parts(
                frame.data[0]
                    .cast::<T>()
                    .add(offset * frame.ch_layout.nb_channels as usize),
                output.len(),
            )
        };
        for (out, &sample) in output.iter_mut().zip(samples) {
            *out = convert(sample);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivu_format::input::open_regular_file;

    #[test]
    fn encoded_duration_restores_skip_discard_and_rejects_manual_or_malformed_trim() {
        let mut bytes = [0_u8; 10];
        bytes[..4].copy_from_slice(&312_u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&648_u32.to_le_bytes());
        let mut side: AVPacketSideData = unsafe { std::mem::zeroed() };
        side.type_ = AVPacketSideDataType_AV_PKT_DATA_SKIP_SAMPLES;
        side.data = bytes.as_mut_ptr();
        side.size = bytes.len();
        let mut packet: AVPacket = unsafe { std::mem::zeroed() };
        packet.side_data = &mut side;
        packet.side_data_elems = 1;
        assert_eq!(packet_trim_samples(&packet), Some(960));
        let mut stats = PacketStats::new();
        stats.add(1000, packet_trim_samples(&packet).unwrap());
        stats.add_duration(8640);
        stats.finish(Some(9600));
        assert_eq!(stats.bitrate(1, 48_000), Some(40_000));
        side.size = 7;
        packet.side_data = &mut side;
        assert_eq!(packet_trim_samples(&packet), None);
        packet.side_data_elems = MAX_PACKET_SIDE_DATA + 1;
        assert_eq!(packet_trim_samples(&packet), None);

        let mut frame: AVFrame = unsafe { std::mem::zeroed() };
        assert!(automatic_frame_trim(&frame));
        let mut manual: AVFrameSideData = unsafe { std::mem::zeroed() };
        manual.type_ = AVFrameSideDataType_AV_FRAME_DATA_SKIP_SAMPLES;
        let mut manual_ptr = &mut manual as *mut AVFrameSideData;
        frame.side_data = &mut manual_ptr;
        frame.nb_side_data = 1;
        assert!(!automatic_frame_trim(&frame));
    }

    fn pcm_file() -> tempfile::NamedTempFile {
        use std::io::Write;

        let mut file = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        let samples = 16_000_u32;
        let bytes = samples * 2;
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + bytes).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt \x10\0\0\0\x01\0\x01\0").unwrap();
        file.write_all(&8_000_u32.to_le_bytes()).unwrap();
        file.write_all(&16_000_u32.to_le_bytes()).unwrap();
        file.write_all(b"\x02\0\x10\0data").unwrap();
        file.write_all(&bytes.to_le_bytes()).unwrap();
        file.write_all(&vec![0; bytes as usize]).unwrap();
        file
    }

    #[test]
    fn ffmpeg_opens_the_checked_file_when_its_path_is_replaced_by_fifo() {
        let original = pcm_file();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("audio.wav");
        std::fs::copy(original.path(), &path).unwrap();
        let file = open_regular_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let name = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is a live NUL-terminated path; mkfifo retains no pointer.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (completed, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let input = Input::from_file(path, file, runtime().unwrap()).unwrap();
            let mut decoder = Decoder::from_input(input, None).unwrap();
            let mut frames = Vec::new();
            let mut samples = 0;
            loop {
                match decoder.next_frames(&mut frames, None, &mut 64).unwrap() {
                    DecodeStep::Frames(_) => samples += frames.len(),
                    DecodeStep::Yield => (),
                    DecodeStep::Eof => break,
                }
            }
            assert_eq!(samples, 16_000);
            // Decoder seek may return preroll; Source owns sample-accurate clipping.
            // Rewinding must still replay the checked object, not the FIFO path.
            decoder.seek(0.0, None).unwrap();
            samples = 0;
            loop {
                match decoder.next_frames(&mut frames, None, &mut 64).unwrap() {
                    DecodeStep::Frames(_) => samples += frames.len(),
                    DecodeStep::Yield => (),
                    DecodeStep::Eof => break,
                }
            }
            assert_eq!(samples, 16_000);
            completed.send(()).unwrap();
        });
        // Do not join on timeout: reopening the substituted path would hang.
        receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn complete_ffmpeg_decode_counts_selected_packets_not_pcm_or_other_streams() {
        let file = pcm_file();
        let mut decoder =
            Decoder::from_file(file.path(), open_regular_file(file.path()).unwrap(), None).unwrap();
        assert_eq!(decoder.completed_bitrate(), None);
        let mut frames = Vec::new();
        let mut injected = false;
        let mut samples = 0;
        let read = decoder.rt.f.read_frame;
        loop {
            let step = decoder
                .next_frames_with_reader(&mut frames, None, &mut 64, |format, packet| {
                    if !injected {
                        injected = true;
                        unsafe {
                            (*packet).stream_index = -1;
                            (*packet).size = 1_000_000;
                            (*packet).duration = 1;
                        }
                        return 0;
                    }
                    unsafe { read(format, packet) }
                })
                .unwrap();
            match step {
                DecodeStep::Frames(_) => samples += frames.len(),
                DecodeStep::Yield => (),
                DecodeStep::Eof => break,
            }
        }
        assert_eq!(samples, 16_000);
        assert_eq!(decoder.completed_bitrate(), Some(128_000));
        decoder.seek(0.0, None).unwrap();
        loop {
            if matches!(
                decoder.next_frames(&mut frames, None, &mut 64).unwrap(),
                DecodeStep::Eof
            ) {
                break;
            }
        }
        assert_eq!(
            decoder.completed_bitrate(),
            None,
            "a seek permanently excludes complete statistics"
        );
    }

    #[test]
    fn ffmpeg_missing_packet_duration_and_corrupt_packet_never_become_exact() {
        let file = pcm_file();
        for corrupt in [false, true] {
            let mut decoder =
                Decoder::from_file(file.path(), open_regular_file(file.path()).unwrap(), None)
                    .unwrap();
            let read = decoder.rt.f.read_frame;
            let mut frames = Vec::new();
            loop {
                let step = decoder
                    .next_frames_with_reader(&mut frames, None, &mut 64, |format, packet| {
                        let status = unsafe { read(format, packet) };
                        if status >= 0 {
                            unsafe {
                                if corrupt {
                                    (*packet).flags |= 2;
                                } else {
                                    (*packet).duration = 0;
                                }
                            }
                        }
                        status
                    })
                    .unwrap();
                if matches!(step, DecodeStep::Eof) {
                    break;
                }
            }
            assert_eq!(decoder.completed_bitrate(), None);
        }
    }

    #[test]
    fn skipped_ffmpeg_packets_yield_without_cancellation_or_eof() {
        let file = pcm_file();
        let mut decoder =
            Decoder::from_file(file.path(), open_regular_file(file.path()).unwrap(), None).unwrap();
        decoder.seek(0.0, None).unwrap();
        let mut frames = Vec::new();
        let mut budget = 3;
        let mut packets = 0;
        let read = decoder.rt.f.read_frame;
        let step = decoder
            .next_frames_with_reader(&mut frames, None, &mut budget, |format, packet| {
                let status = unsafe { read(format, packet) };
                assert!(status >= 0, "fixture has more packets");
                packets += 1;
                unsafe { (*packet).stream_index = -1 };
                status
            })
            .unwrap();
        assert_eq!(step, DecodeStep::Yield);
        assert_eq!(packets, 3);
        assert_eq!(budget, 0);
        assert!(!decoder.draining);
        decoder.seek(0.0, None).unwrap();
        assert!(matches!(
            decoder.next_frames(&mut frames, None, &mut 64).unwrap(),
            DecodeStep::Frames(_)
        ));
    }

    #[test]
    fn cancellation_stops_ffmpeg_skipped_packets_and_seek_input_at_finite_counts() {
        use std::sync::atomic::Ordering;
        let file = pcm_file();
        for skip in [false, true] {
            let mut decoder =
                Decoder::from_file(file.path(), open_regular_file(file.path()).unwrap(), None)
                    .unwrap();
            decoder.seek(1.25, None).unwrap();
            let cancelled = AtomicBool::new(false);
            let mut packets = 0;
            let limit = if skip { 3 } else { 1 };
            let read = decoder.rt.f.read_frame;
            let mut frames = Vec::new();
            let error = decoder
                .next_frames_with_reader(
                    &mut frames,
                    Some(&cancelled),
                    &mut 64,
                    |format, packet| {
                        let status = unsafe { read(format, packet) };
                        assert!(status >= 0, "fixture must contain the packet prefix");
                        packets += 1;
                        if skip {
                            unsafe { (*packet).stream_index = -1 };
                        }
                        if packets == limit {
                            cancelled.store(true, Ordering::Relaxed);
                        }
                        status
                    },
                )
                .unwrap_err();
            assert_eq!(packets, limit);
            assert_eq!(error.to_string(), "Waveform loading cancelled");
            assert!(!decoder.draining, "cancellation is not EOF");
            cancelled.store(false, Ordering::Relaxed);
            decoder.seek(0.0, Some(&cancelled)).unwrap();
            assert!(matches!(
                decoder
                    .next_frames(&mut frames, Some(&cancelled), &mut 64)
                    .unwrap(),
                DecodeStep::Frames(_)
            ));
        }
    }

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
        copy_frame(&frame, 0, &mut output).unwrap();
        assert_eq!(output, [-1.0, 0.0, 1.0, 0.5]);
    }

    #[test]
    fn planar_float_samples_clear_invalid_values_without_reordering_channels() {
        let left = [f32::NAN, 2.0, f32::MAX];
        let right = [f32::NEG_INFINITY, -3.0, 0.5];
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
        copy_frame(&frame, 0, &mut output).unwrap();
        assert_eq!(output, [0.0, 0.0, 2.0, -3.0, f32::MAX, 0.5]);
    }
}
