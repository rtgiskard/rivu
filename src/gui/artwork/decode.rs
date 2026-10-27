//! Admission for untrusted real covers only; trusted decorations keep GPUI's policy.
//!
//! The retained budget counts BGRA pixels plus conservative frame/vector overhead.
//! Workspace is a separate pre-decode estimate, not a codec allocator or RSS cap:
//! codecs, encoded input, temporary conversions and GPU textures have other costs.
mod thumbnail;

use std::{io::Cursor, path::Path, sync::Arc};

use gpui::{ImageCacheError, RenderImage};
use image::{
    AnimationDecoder, DynamicImage, Frame, ImageDecoder, ImageFormat, ImageReader, Limits,
    codecs::{gif::GifDecoder, webp::WebPDecoder},
};

const MIB: u64 = 1024 * 1024;
const FRAME_OVERHEAD: u64 = 2 * std::mem::size_of::<Frame>() as u64 + 64;

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub(super) struct Budget {
    retained: u64,
    workspace: u64,
    max_side: u32,
}

fn limit_error() -> image::ImageError {
    image::ImageError::Limits(image::error::LimitError::from_kind(
        image::error::LimitErrorKind::InsufficientMemory,
    ))
}

impl Default for Budget {
    fn default() -> Self {
        Self::new(64)
    }
}

impl Budget {
    pub(super) const fn new(retained_mb: u32) -> Self {
        Self {
            retained: retained_mb as u64 * MIB,
            workspace: 64 * MIB,
            max_side: 4096,
        }
    }

    fn canvas(self, width: u32, height: u32) -> image::ImageResult<u64> {
        if width == 0 || height == 0 || width > self.max_side || height > self.max_side {
            return Err(limit_error());
        }
        let rgba = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(limit_error)?;
        // Allow for compositing, the next decoded frame and conversion scratch.
        if rgba.checked_mul(4).ok_or_else(limit_error)? > self.workspace {
            return Err(limit_error());
        }
        self.charge(0, rgba)?;
        Ok(rgba)
    }

    fn charge(self, retained: u64, rgba: u64) -> image::ImageResult<u64> {
        let next = retained
            .checked_add(rgba)
            .and_then(|bytes| bytes.checked_add(FRAME_OVERHEAD))
            .ok_or_else(limit_error)?;
        if next > self.retained {
            return Err(limit_error());
        }
        Ok(next)
    }

    fn limits(self) -> Limits {
        let mut limits = Limits::default();
        limits.max_image_width = Some(self.max_side);
        limits.max_image_height = Some(self.max_side);
        limits.max_alloc = Some(self.workspace);
        limits
    }

    fn configure(self, decoder: &mut impl ImageDecoder) -> image::ImageResult<()> {
        let (width, height) = decoder.dimensions();
        let rgba = self.canvas(width, height)?;
        // A static source can be 16-bit/float, and orientation/conversion can
        // temporarily coexist with its source buffer. Use checked native bytes.
        let workspace = decoder
            .total_bytes()
            .checked_add(rgba)
            .and_then(|bytes| bytes.checked_mul(2))
            .ok_or_else(limit_error)?;
        if workspace > self.workspace {
            return Err(limit_error());
        }
        decoder.set_limits(self.limits())
    }
}

/// Resolve a physical-pixel demand to a shared discrete decode tier.
pub(super) fn requested_tier(side: u32) -> u32 {
    match side {
        0..=256 => 256,
        257..=512 => 512,
        513..=1024 => 1024,
        _ => u32::MAX,
    }
}

pub(super) struct DecodedArtwork {
    pub(super) image: Arc<RenderImage>,
    /// Format-level animation policy, including a one-frame GIF or WebP.
    pub(super) animated: bool,
    pub(super) source_side: u32,
}

/// Blocking background work; never call this from the UI thread.
pub(super) fn load(
    path: &Path,
    max_encoded_bytes: usize,
    budget: Budget,
    requested_side: u32,
) -> Result<DecodedArtwork, ImageCacheError> {
    thumbnail::load(
        path,
        max_encoded_bytes,
        budget,
        requested_tier(requested_side),
    )
}

fn decode(bytes: &[u8], budget: Budget) -> image::ImageResult<Vec<Frame>> {
    match image::guess_format(bytes)? {
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(Cursor::new(bytes))?;
            budget.configure(&mut decoder)?;
            collect_frames(decoder.into_frames(), budget)
        }
        ImageFormat::WebP => {
            // image's WebP wrapper does not forward allocation limits. Inspect
            // both canvas and embedded bitstream dimensions before pixel decode.
            preflight_webp(bytes, budget)?;
            let mut decoder = WebPDecoder::new(Cursor::new(bytes))?;
            budget.configure(&mut decoder)?;
            if decoder.has_animation() {
                decoder.set_background_color(image::Rgba([0, 0, 0, 0]))?;
                collect_frames(decoder.into_frames(), budget)
            } else {
                static_frame(decoder, budget)
            }
        }
        format => {
            let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
            reader.limits(budget.limits());
            let mut decoder = reader.into_decoder()?;
            budget.configure(&mut decoder)?;
            static_frame(decoder, budget)
        }
    }
}

fn static_frame(mut decoder: impl ImageDecoder, budget: Budget) -> image::ImageResult<Vec<Frame>> {
    let (width, height) = decoder.dimensions();
    budget.canvas(width, height)?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    let mut frame = Frame::new(image.into_rgba8());
    to_bgra(&mut frame);
    Ok(vec![frame])
}

fn collect_frames(frames: image::Frames<'_>, budget: Budget) -> image::ImageResult<Vec<Frame>> {
    let mut retained = 0;
    // Start at one descriptor: subsequent Vec growth stays within the charged
    // two-descriptor allowance per admitted frame, including tiny animations.
    let mut decoded = Vec::with_capacity(1);
    for frame in frames {
        // Never skip a corrupt/over-budget frame and publish a partial animation.
        // The candidate frame is workspace until admitted to the retained set.
        let mut frame = frame?;
        let (width, height) = frame.buffer().dimensions();
        let rgba = budget.canvas(width, height)?;
        retained = budget.charge(retained, rgba)?;
        to_bgra(&mut frame);
        decoded.push(frame);
    }
    if decoded.is_empty() {
        return Err(image::ImageError::Decoding(
            image::error::DecodingError::new(
                image::error::ImageFormatHint::Unknown,
                "cover has no frames",
            ),
        ));
    }
    Ok(decoded)
}

fn to_bgra(frame: &mut Frame) {
    for pixel in frame.buffer_mut().as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
}

fn malformed_webp() -> image::ImageError {
    image::ImageError::Decoding(image::error::DecodingError::new(
        ImageFormat::WebP.into(),
        "invalid WebP cover chunk",
    ))
}

fn u24(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

fn preflight_webp(bytes: &[u8], budget: Budget) -> image::ImageResult<()> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(malformed_webp());
    }
    let size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let end = size
        .checked_add(8)
        .filter(|&end| end <= bytes.len())
        .ok_or_else(malformed_webp)?;
    if end < 12 {
        return Err(malformed_webp());
    }
    webp_chunks(&bytes[12..end], budget, false)
}

// Only bounded header inspection, not a second decoder: actual codecs remain
// responsible for format validation/compositing/delays. Inspect raw VP8 sizes too:
// a malicious ANMF can advertise a small rectangle around a huge VP8 bitstream.
fn webp_chunks(mut bytes: &[u8], budget: Budget, subframe: bool) -> image::ImageResult<()> {
    while !bytes.is_empty() {
        if bytes.len() < 8 {
            return Err(malformed_webp());
        }
        let kind = &bytes[..4];
        let len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let end = 8usize.checked_add(len).ok_or_else(malformed_webp)?;
        let padded = end.checked_add(len & 1).ok_or_else(malformed_webp)?;
        let data = bytes.get(8..end).ok_or_else(malformed_webp)?;
        match kind {
            b"VP8X" if !subframe => {
                if data.len() != 10 {
                    return Err(malformed_webp());
                }
                budget.canvas(u24(&data[4..7]) + 1, u24(&data[7..10]) + 1)?;
            }
            b"ANMF" if !subframe => {
                if data.len() < 16 {
                    return Err(malformed_webp());
                }
                budget.canvas(u24(&data[6..9]) + 1, u24(&data[9..12]) + 1)?;
                webp_chunks(&data[16..], budget, true)?;
            }
            b"VP8 " => {
                if data.len() < 10 || &data[3..6] != b"\x9d\x01\x2a" {
                    return Err(malformed_webp());
                }
                let width = u16::from_le_bytes(data[6..8].try_into().unwrap()) & 0x3fff;
                let height = u16::from_le_bytes(data[8..10].try_into().unwrap()) & 0x3fff;
                budget.canvas(u32::from(width), u32::from(height))?;
            }
            b"VP8L" => {
                if data.len() < 5 || data[0] != 0x2f {
                    return Err(malformed_webp());
                }
                let header = u32::from_le_bytes(data[1..5].try_into().unwrap());
                budget.canvas((header & 0x3fff) + 1, ((header >> 14) & 0x3fff) + 1)?;
            }
            b"ANMF" | b"VP8X" => return Err(malformed_webp()),
            _ => {}
        }
        bytes = bytes.get(padded..).ok_or_else(malformed_webp)?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Valid 800x400 BMP with a legal gap before its pixels, not trailing junk.
    /// Large encoded size must not be confused with decoded pixel/workspace size.
    pub(in crate::gui::artwork) fn large_encoded_bmp() -> Vec<u8> {
        let pixels = image::RgbImage::from_pixel(800, 400, image::Rgb([210, 30, 40]));
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(pixels)
            .write_to(&mut encoded, ImageFormat::Bmp)
            .unwrap();
        let mut bytes = encoded.into_inner();
        let offset = u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize;
        let padding = 9 * MIB as usize - bytes.len();
        bytes.splice(offset..offset, std::iter::repeat_n(0, padding));
        let length = bytes.len() as u32;
        bytes[2..6].copy_from_slice(&length.to_le_bytes());
        bytes[10..14].copy_from_slice(&((offset + padding) as u32).to_le_bytes());
        bytes
    }

    #[test]
    fn encoded_session_limit_is_independent_of_retained_pixels_and_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large-encoded.bmp");
        let bytes = large_encoded_bmp();
        assert_eq!(bytes.len(), 9 * MIB as usize);
        std::fs::write(&path, bytes).unwrap();
        // The decoded canvas is only 1.22 MiB and fits a 2 MiB retained budget.
        let budget = Budget::new(2);
        assert!(load(&path, 8 * MIB as usize, budget, 2048).is_err());
        let decoded = load(&path, crate::artwork::DEFAULT_MAX_IMAGE_BYTES, budget, 2048).unwrap();
        assert_eq!(u32::from(decoded.image.size(0).width), 800);
        assert_eq!(u32::from(decoded.image.size(0).height), 400);
        assert_eq!(
            &decoded.image.as_bytes(0).unwrap()[..4],
            &[40, 30, 210, 255]
        );
        assert!(
            load(
                &path,
                crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
                Budget::new(1),
                2048
            )
            .is_err()
        );
        assert!(load(&path, 8 * MIB as usize, budget, 2048).is_err());
    }

    pub(in crate::gui::artwork) fn budget(frames: u64) -> Budget {
        Budget {
            retained: frames * (16 + FRAME_OVERHEAD),
            workspace: 1024,
            max_side: 8,
        }
    }

    #[test]
    fn configured_retained_budget_keeps_workspace_and_dimension_caps_fixed() {
        for retained_mb in [1, 64, 1024] {
            let budget = Budget::new(retained_mb);
            assert_eq!(budget.retained, u64::from(retained_mb) * MIB);
            assert_eq!(budget.workspace, 64 * MIB);
            assert_eq!(budget.max_side, 4096);
        }
    }

    fn pixels(red: bool) -> image::RgbaImage {
        image::RgbaImage::from_pixel(
            2,
            2,
            image::Rgba(if red {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            }),
        )
    }

    pub(in crate::gui::artwork) fn gif() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            for (red, delay) in [(true, 70), (false, 130)] {
                encoder
                    .encode_frame(Frame::from_parts(
                        pixels(red),
                        0,
                        0,
                        image::Delay::from_numer_denom_ms(delay, 1),
                    ))
                    .unwrap();
            }
        }
        bytes
    }

    fn chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        output.extend_from_slice(kind);
        output.extend_from_slice(&(data.len() as u32).to_le_bytes());
        output.extend_from_slice(data);
        if !data.len().is_multiple_of(2) {
            output.push(0);
        }
    }

    pub(in crate::gui::artwork) fn webp() -> Vec<u8> {
        // Lossless still frames wrapped in the standard animation container;
        // no external encoder or large binary fixture is needed.
        let mut body = b"WEBP".to_vec();
        chunk(&mut body, b"VP8X", &[0x12, 0, 0, 0, 1, 0, 0, 1, 0, 0]);
        chunk(&mut body, b"ANIM", &[0; 6]);
        for (red, delay) in [(true, 70u8), (false, 130)] {
            let mut still = Vec::new();
            image::codecs::webp::WebPEncoder::new_lossless(&mut still)
                .encode(pixels(red).as_raw(), 2, 2, image::ExtendedColorType::Rgba8)
                .unwrap();
            let mut frame = vec![0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, delay, 0, 0, 2];
            frame.extend_from_slice(&still[12..]);
            chunk(&mut body, b"ANMF", &frame);
        }
        let mut output = b"RIFF".to_vec();
        output.extend_from_slice(&(body.len() as u32).to_le_bytes());
        output.extend_from_slice(&body);
        output
    }

    #[test]
    fn animated_cover_gif_and_webp_preserve_all_frames_colors_and_delays() {
        for bytes in [gif(), webp()] {
            let frames = decode(&bytes, budget(2)).unwrap();
            assert_eq!(frames.len(), 2);
            assert_eq!(frames[0].buffer().get_pixel(0, 0).0, [0, 0, 255, 255]);
            assert_eq!(frames[1].buffer().get_pixel(0, 0).0, [255, 0, 0, 255]);
            assert_eq!(frames[0].delay().numer_denom_ms(), (70, 1));
            assert_eq!(frames[1].delay().numer_denom_ms(), (130, 1));
        }
    }

    #[test]
    fn animated_cover_gif_and_webp_reject_whole_image_above_retained_boundary() {
        for bytes in [gif(), webp()] {
            let exact = budget(2);
            assert_eq!(decode(&bytes, exact).unwrap().len(), 2);
            assert!(matches!(
                decode(
                    &bytes,
                    Budget {
                        retained: exact.retained - 1,
                        ..exact
                    }
                ),
                Err(image::ImageError::Limits(_))
            ));
            assert!(matches!(
                decode(&bytes, budget(1)),
                Err(image::ImageError::Limits(_))
            ));
        }
    }

    #[test]
    fn animated_cover_gif_and_webp_check_dimensions_and_workspace_before_pixels() {
        for bytes in [gif(), webp()] {
            assert!(matches!(
                decode(
                    &bytes,
                    Budget {
                        max_side: 1,
                        ..budget(2)
                    }
                ),
                Err(image::ImageError::Limits(_))
            ));
            assert!(matches!(
                decode(
                    &bytes,
                    Budget {
                        workspace: 63,
                        ..budget(2)
                    }
                ),
                Err(image::ImageError::Limits(_))
            ));
            assert_eq!(
                decode(
                    &bytes,
                    Budget {
                        workspace: 64,
                        ..budget(2)
                    }
                )
                .unwrap()
                .len(),
                2
            );
        }
    }

    #[test]
    fn animated_cover_webp_rejects_oversized_rectangles_and_embedded_bitstreams() {
        let source = webp();
        let anmf = source
            .windows(4)
            .position(|bytes| bytes == b"ANMF")
            .unwrap();
        let vp8l = source
            .windows(4)
            .position(|bytes| bytes == b"VP8L")
            .unwrap();
        let mut rectangle = source.clone();
        rectangle[anmf + 8 + 6] = 8; // frame width 9, canvas remains 2
        let mut bitstream = source;
        let offset = vp8l + 9;
        let mut header = u32::from_le_bytes(bitstream[offset..offset + 4].try_into().unwrap());
        header = (header & !0x3fff) | 8; // actual VP8L width 9, rectangle remains 2
        bitstream[offset..offset + 4].copy_from_slice(&header.to_le_bytes());
        for bytes in [rectangle, bitstream] {
            assert!(matches!(
                decode(&bytes, budget(2)),
                Err(image::ImageError::Limits(_))
            ));
        }
    }

    #[test]
    fn animated_cover_webp_rejects_malformed_chunks_without_partial_success() {
        let source = webp();
        for bytes in [&source[..source.len() - 1], &source[..20]] {
            assert!(decode(bytes, budget(2)).is_err());
        }
        let mut nested = source;
        let vp8l = nested
            .windows(4)
            .position(|bytes| bytes == b"VP8L")
            .unwrap();
        nested[vp8l + 4..vp8l + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&nested, budget(2)).is_err());
    }

    #[test]
    fn animated_cover_gif_rejects_decode_errors_after_a_valid_frame() {
        let mut bytes = gif();
        // Preserve frame one, corrupt the second frame's LZW code size.
        let frames = bytes
            .iter()
            .enumerate()
            .filter_map(|(index, &byte)| (byte == 0x2c).then_some(index))
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 2);
        let second = frames[1];
        let packed = bytes[second + 9];
        let palette = if packed & 0x80 != 0 {
            3 * (1usize << ((packed & 7) + 1))
        } else {
            0
        };
        bytes[second + 10 + palette] = 0xff;
        assert!(decode(&bytes, budget(2)).is_err());
    }

    #[test]
    fn static_cover_png_and_webp_keep_bgra_and_obey_the_same_limits() {
        use image::ImageEncoder;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(pixels(true).as_raw(), 2, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .encode(pixels(true).as_raw(), 2, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        for bytes in [png, webp] {
            let frames = decode(&bytes, budget(1)).unwrap();
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].buffer().get_pixel(0, 0).0, [0, 0, 255, 255]);
            assert!(matches!(
                decode(
                    &bytes,
                    Budget {
                        retained: budget(1).retained - 1,
                        ..budget(1)
                    }
                ),
                Err(image::ImageError::Limits(_))
            ));
        }
    }
}
