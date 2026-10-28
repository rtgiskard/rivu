use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    sync::{Arc, LazyLock, Weak},
};

use gpui::{ImageCacheError, RenderImage};
use image::{ImageDecoder, ImageFormat, ImageReader, codecs::webp::WebPDecoder};
use parking_lot::Mutex;

use crate::artwork::cache::thumbnail_path;

use super::{Budget, decode, limit_error, preflight_webp, to_bgra};

// A disposable tier is at most 1024px; its file-safety cap is not the source's
// configurable encoded admission limit.
const MAX_THUMBNAIL_BYTES: usize = 8 * 1024 * 1024;

type SourceKey = (blake3::Hash, usize, Budget);
type Sources = HashMap<SourceKey, Weak<Mutex<Source>>>;

#[derive(Default)]
struct Source {
    info: Option<SourceInfo>,
    original: Option<Arc<RenderImage>>,
    rendered: HashMap<u32, Weak<RenderImage>>,
}

#[derive(Clone, Copy)]
struct SourceInfo {
    longest: u32,
    animated: bool,
}

fn read_bounded(path: &Path, max_bytes: usize) -> Result<Vec<u8>, ImageCacheError> {
    // Bound the read even if a file was changed outside the artwork manager.
    let mut bytes = Vec::new();
    File::open(path)?
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(limit_error().into());
    }
    Ok(bytes)
}

fn source(hash: blake3::Hash, max_encoded_bytes: usize, budget: Budget) -> Arc<Mutex<Source>> {
    static SOURCES: LazyLock<Mutex<Sources>> = LazyLock::new(Mutex::default);
    let mut sources = SOURCES.lock();
    // Only active background loads own these entries: neither original pixels
    // nor locks accumulate after the last waiter leaves.
    sources.retain(|_, source| source.strong_count() != 0);
    if let Some(source) = sources
        .get(&(hash, max_encoded_bytes, budget))
        .and_then(Weak::upgrade)
    {
        return source;
    }
    let source = Arc::new(Mutex::new(Source::default()));
    sources.insert((hash, max_encoded_bytes, budget), Arc::downgrade(&source));
    source
}

fn inspect(bytes: &[u8], budget: Budget) -> image::ImageResult<SourceInfo> {
    let format = image::guess_format(bytes)?;
    if format == ImageFormat::WebP {
        // A cached thumbnail must never bypass the original's admission checks.
        preflight_webp(bytes, budget)?;
        let mut decoder = WebPDecoder::new(std::io::Cursor::new(bytes))?;
        budget.configure(&mut decoder)?;
        let (width, height) = decoder.dimensions();
        return Ok(SourceInfo {
            longest: width.max(height),
            animated: decoder.has_animation(),
        });
    }
    let mut reader = ImageReader::with_format(std::io::Cursor::new(bytes), format);
    reader.limits(budget.limits());
    let mut decoder = reader.into_decoder()?;
    budget.configure(&mut decoder)?;
    let (width, height) = decoder.dimensions();
    Ok(SourceInfo {
        longest: width.max(height),
        // GIF keeps its original path, including one-frame GIFs.
        animated: format == ImageFormat::Gif,
    })
}

fn cached(path: &Path, budget: Budget, tier: u32) -> Option<Arc<RenderImage>> {
    // NotFound also covers manual cleanup between path construction and open.
    // Corrupt disposable thumbnails are regenerated from the admitted source.
    let bytes = read_bounded(path, MAX_THUMBNAIL_BYTES).ok()?;
    let info = inspect(&bytes, budget).ok()?;
    if info.animated || info.longest != tier {
        return None;
    }
    let frames = decode(&bytes, budget).ok()?;
    (frames.len() == 1).then(|| Arc::new(RenderImage::new(frames)))
}

fn publish(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("Thumbnail has no directory"))?;
    fs::create_dir_all(parent)?;
    // No partial filename is ever visible to readers or cache maintenance.
    // A concurrent process may publish equivalent bytes; replacement is atomic.
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn dimensions(width: u32, height: u32, side: u32) -> (u32, u32) {
    let longest = width.max(height);
    let scale = |dimension: u32| {
        ((u64::from(dimension) * u64::from(side) + u64::from(longest) / 2) / u64::from(longest))
            .max(1) as u32
    };
    (scale(width), scale(height))
}

/// Triangle support and pixel-center convention match image's separable filter.
struct TriangleAxis {
    length: usize,
    ratio: f32,
    support: f32,
}

impl TriangleAxis {
    fn new(source: u32, target: u32) -> Self {
        let ratio = source as f32 / target as f32;
        Self {
            length: source as usize,
            ratio,
            support: ratio.max(1.0),
        }
    }

    fn max_taps(&self) -> usize {
        ((self.support * 2.0).ceil() as usize + 1).min(self.length)
    }

    fn weights(&self, position: usize, weights: &mut Vec<f32>) -> usize {
        let center = (position as f32 + 0.5) * self.ratio;
        let start =
            ((center - self.support).floor() as i64).clamp(0, self.length as i64 - 1) as usize;
        let end = ((center + self.support).ceil() as i64)
            .clamp(start as i64 + 1, self.length as i64) as usize;
        let center = center - 0.5;
        weights.clear();
        let mut sum = 0.0;
        for pixel in start..end {
            let distance = ((pixel as f32 - center) / self.support).abs();
            let weight = if distance < 1.0 { 1.0 - distance } else { 0.0 };
            weights.push(weight);
            sum += weight;
        }
        for weight in weights {
            *weight /= sum;
        }
        start
    }
}

fn resize_bgra(
    view: image::ImageBuffer<image::Rgba<u8>, &[u8]>,
    width: u32,
    height: u32,
) -> image::RgbaImage {
    let horizontal = TriangleAxis::new(view.width(), width);
    let vertical = TriangleAxis::new(view.height(), height);
    let source_width = view.width() as usize;
    let source = view.as_raw().as_chunks::<4>().0;
    let alpha = source.iter().any(|pixel| pixel[3] != 255);
    let mut taps = Vec::with_capacity(horizontal.max_taps().max(vertical.max_taps()));
    let mut spans = Vec::with_capacity(width as usize);
    let mut weights = Vec::with_capacity(width as usize * horizontal.max_taps());
    for x in 0..width as usize {
        let start = horizontal.weights(x, &mut taps);
        spans.push((start, weights.len(), taps.len()));
        weights.extend_from_slice(&taps);
    }

    // Retain only one source-width f32 row, not source_width * target_height.
    // Each pixel keeps the existing vertical-then-horizontal accumulation order.
    let mut row = vec![[0.0_f32; 4]; source_width];
    let mut output = image::RgbaImage::new(width, height);
    for (y, destination) in output
        .as_mut()
        .chunks_exact_mut(width as usize * 4)
        .enumerate()
    {
        let start = vertical.weights(y, &mut taps);
        row.fill([0.0; 4]);
        for (source_row, &weight) in source[start * source_width..]
            .chunks_exact(source_width)
            .zip(&taps)
        {
            if alpha {
                // Premultiply on read at 16-bit precision, without a source copy.
                for (accumulator, pixel) in row.iter_mut().zip(source_row) {
                    let alpha = u32::from(pixel[3]);
                    for channel in 0..3 {
                        let premultiplied = (u32::from(pixel[channel]) * alpha * 257 + 127) / 255;
                        accumulator[channel] += premultiplied as f32 * weight;
                    }
                    accumulator[3] += (alpha * 257) as f32 * weight;
                }
            } else {
                for (accumulator, pixel) in row.iter_mut().zip(source_row) {
                    for channel in 0..4 {
                        accumulator[channel] += f32::from(pixel[channel]) * weight;
                    }
                }
            }
        }
        for (destination, &(start, offset, length)) in
            destination.as_chunks_mut::<4>().0.iter_mut().zip(&spans)
        {
            let mut pixel = [0.0_f32; 4];
            for (source, &weight) in row[start..start + length]
                .iter()
                .zip(&weights[offset..offset + length])
            {
                for channel in 0..4 {
                    pixel[channel] += source[channel] * weight;
                }
            }
            if alpha {
                // Preserve the u16 filter rounding before unpremultiplication,
                // but write BGRA8 directly instead of retaining a u16 image.
                let alpha = pixel[3].clamp(0.0, 65535.0).round() as u32;
                let alpha8 = ((alpha + 128) / 257) as u8;
                if alpha8 == 0 {
                    destination.fill(0);
                } else {
                    for channel in 0..3 {
                        let value = pixel[channel].clamp(0.0, 65535.0).round() as u32;
                        destination[channel] = ((value * 255 + alpha / 2) / alpha).min(255) as u8;
                    }
                    destination[3] = alpha8;
                }
            } else {
                for channel in 0..4 {
                    destination[channel] = pixel[channel].clamp(0.0, 255.0).round() as u8;
                }
            }
        }
    }
    output
}

pub(super) fn load(
    path: &Path,
    max_encoded_bytes: usize,
    budget: Budget,
    tier: u32,
) -> Result<super::DecodedArtwork, ImageCacheError> {
    let bytes = read_bounded(path, max_encoded_bytes)?;
    let hash = blake3::hash(&bytes);
    let source = source(hash, max_encoded_bytes, budget);
    // This mutex is taken only by existing background decode jobs. Same-source
    // requests share one original decode and one result per requested tier.
    let mut source = source.lock();
    let info = match source.info {
        Some(info) => info,
        None => {
            let info = inspect(&bytes, budget)?;
            source.info = Some(info);
            info
        }
    };
    let decoded = |image| super::DecodedArtwork {
        image,
        animated: info.animated,
        source_side: info.longest,
    };
    // Do not upscale or create several equivalent files for a small source.
    let tier = if info.animated || tier >= info.longest {
        u32::MAX
    } else {
        tier
    };
    if let Some(image) = source.rendered.get(&tier).and_then(Weak::upgrade) {
        return Ok(decoded(image));
    }
    let path = (tier != u32::MAX)
        .then(|| thumbnail_path(path, hash, tier))
        .flatten();
    if let Some(image) = path.as_deref().and_then(|path| cached(path, budget, tier)) {
        source.rendered.insert(tier, Arc::downgrade(&image));
        return Ok(decoded(image));
    }
    let original = match &source.original {
        Some(image) => Arc::clone(image),
        None => {
            let image = Arc::new(RenderImage::new(decode(&bytes, budget)?));
            source.original = Some(Arc::clone(&image));
            image
        }
    };
    if tier == u32::MAX {
        source.rendered.insert(tier, Arc::downgrade(&original));
        return Ok(decoded(original));
    }
    let size = original.size(0);
    let (width, height) = (u32::from(size.width), u32::from(size.height));
    let (width, height) = dimensions(width, height, tier);
    // Borrow source BGRA; alpha-aware filtering never mutates shared pixels.
    let view = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(
        u32::from(size.width),
        u32::from(size.height),
        original.as_bytes(0).expect("admitted cover has a frame"),
    )
    .expect("admitted cover has complete pixels");
    let mut frame = image::Frame::new(resize_bgra(view, width, height));
    to_bgra(&mut frame); // BGRA -> RGBA for the encoder, preserving alpha.
    let mut encoded = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut encoded).encode(
        frame.buffer().as_raw(),
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )?;
    // Lossless encoding preserves these admitted pixels; keep the existing
    // thumbnail instead of decoding a second copy just for the first display.
    to_bgra(&mut frame);
    let image = Arc::new(RenderImage::new(vec![frame]));
    if let Some(path) = path {
        // Disk caching is best effort; a read-only/full cache must not hide art.
        if let Err(error) = publish(&path, &encoded) {
            tracing::warn!(?error, "artwork_thumbnail_cache_write_failed");
        }
    }
    source.rendered.insert(tier, Arc::downgrade(&image));
    Ok(decoded(image))
}

#[cfg(test)]
mod tests {
    use image::ImageEncoder;

    use super::*;

    #[test]
    fn encoded_session_limit_admits_large_small_canvas_before_thumbnail_reuse() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = super::super::tests::large_encoded_bmp();
        let hash = blake3::hash(&bytes);
        let path = crate::artwork::cache::original_path(directory.path(), hash, "bmp");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        let admitted = crate::artwork::DEFAULT_MAX_IMAGE_BYTES;
        let lower = 8 * 1024 * 1024;
        let budget = Budget::new(2);
        // Keep the shared source alive so later requests exercise resident reuse.
        let source = source(hash, admitted, budget);
        let lower_source = super::source(hash, lower, budget);
        assert!(!Arc::ptr_eq(&source, &lower_source));
        assert!(load(&path, lower, budget, 256).is_err());
        let first = load(&path, admitted, budget, 256).unwrap().image;
        assert_eq!(size(&first), (256, 128));
        let thumbnail = thumbnail_path(&path, hash, 256).unwrap();
        assert!(thumbnail.is_file());
        assert_eq!(&first.as_bytes(0).unwrap()[..4], &[40, 30, 210, 255]);
        assert!(load(&path, lower, budget, 256).is_err());
        let shared = load(&path, admitted, budget, 256).unwrap().image;
        assert!(Arc::ptr_eq(&first, &shared));
        drop(source);
        drop(lower_source);
        drop(first);
        drop(shared);
        // A new source lease now uses the disk tier, still after source admission.
        let cached = load(&path, admitted, budget, 256).unwrap().image;
        assert_eq!(size(&cached), (256, 128));
        assert_eq!(&cached.as_bytes(0).unwrap()[..4], &[40, 30, 210, 255]);
        assert!(load(&path, lower, budget, 256).is_err());
    }

    fn png(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
        let pixels = image::RgbaImage::from_pixel(width, height, image::Rgba(pixel));
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(
                pixels.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        bytes
    }

    fn size(image: &RenderImage) -> (u32, u32) {
        let size = image.size(0);
        (size.width.into(), size.height.into())
    }

    #[test]
    fn physical_demand_rounds_up_to_discrete_tiers() {
        for (side, tier) in [
            (0, 256),
            (255, 256),
            (256, 256),
            (257, 512),
            (512, 512),
            (513, 1024),
            (1024, 1024),
            (1025, u32::MAX),
            (u32::MAX, u32::MAX),
        ] {
            assert_eq!(super::super::requested_tier(side), tier);
        }
    }

    #[test]
    fn static_thumbnails_are_lossless_webp_with_correct_channels_alpha_and_aspect() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = png(800, 400, [240, 12, 20, 96]);
        let hash = blake3::hash(&bytes);
        let path = crate::artwork::cache::original_path(directory.path(), hash, "png");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &bytes).unwrap();
        let loaded = load(
            &path,
            crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
            Budget::default(),
            256,
        )
        .unwrap();
        assert!(!loaded.animated);
        assert_eq!(loaded.source_side, 800);
        let image = loaded.image;
        assert_eq!(size(&image), (256, 128));
        let pixel = &image.as_bytes(0).unwrap()[..4];
        assert_eq!(
            pixel,
            &[20, 12, 240, 96],
            "must preserve straight-alpha BGRA"
        );
        let thumbnail = thumbnail_path(&path, hash, 256).unwrap();
        let hex = hash.to_hex();
        assert_eq!(
            thumbnail,
            directory
                .path()
                .join(&hex[..2])
                .join(format!("{}-256.webp", &hex[2..32]))
        );
        let encoded = fs::read(&thumbnail).unwrap();
        assert_eq!(&encoded[8..12], b"WEBP");
        assert!(encoded.windows(4).any(|bytes| bytes == b"VP8L"));
        assert!(!encoded.windows(4).any(|bytes| bytes == b"VP8 "));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let cached = load(
            &path,
            crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
            Budget::default(),
            256,
        )
        .unwrap()
        .image;
        assert_eq!(image.as_bytes(0), cached.as_bytes(0));
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 2);
    }

    #[test]
    fn noninteger_triangle_sampling_keeps_neighboring_contributions_on_both_axes() {
        for vertical in [false, true] {
            let (width, height) = if vertical { (1, 257) } else { (257, 1) };
            let mut pixels =
                image::RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 0, 255]));
            let (x, y) = if vertical { (0, 9) } else { (9, 0) };
            pixels.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            let view =
                image::ImageBuffer::from_raw(width, height, pixels.as_raw().as_slice()).unwrap();
            let (width, height) = if vertical { (1, 243) } else { (243, 1) };
            let resized = resize_bgra(view, width, height);
            let (x, y) = if vertical { (0, 8) } else { (8, 0) };
            // Pixel 9 contributes ~49% of the normalized Triangle support.
            // Rounding the support down to two taps would discard it entirely.
            assert_eq!(resized.get_pixel(x, y).0, [125, 125, 125, 255]);
        }
    }

    #[test]
    fn transparent_hidden_blue_never_tints_visible_red_during_filtering() {
        let pixels = [0, 0, 255, 255, 255, 0, 0, 0]; // BGRA red / invisible blue.
        let view = image::ImageBuffer::from_raw(2, 1, pixels.as_slice()).unwrap();
        let resized = resize_bgra(view, 1, 1);
        assert_eq!(resized.get_pixel(0, 0).0, [0, 0, 255, 128]);

        // Low-alpha colors keep precision rather than rounding premultiplied
        // channels to zero before the filter has integrated them.
        let pixels = [0, 0, 128, 1, 255, 0, 0, 0];
        let view = image::ImageBuffer::from_raw(2, 1, pixels.as_slice()).unwrap();
        let resized = resize_bgra(view, 1, 1);
        assert_eq!(resized.get_pixel(0, 0).0[0], 0);
        assert!(resized.get_pixel(0, 0).0[2].abs_diff(128) <= 1);
    }

    #[test]
    fn transparent_color_boundaries_survive_webp_encoding_and_disk_hits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("alpha-edge.png");
        let pixels = image::RgbaImage::from_fn(512, 64, |x, _| {
            image::Rgba(if x < 256 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 0]
            })
        });
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(pixels.as_raw(), 512, 64, image::ExtendedColorType::Rgba8)
            .unwrap();
        fs::write(&path, &bytes).unwrap();
        let image = load(
            &path,
            crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
            Budget::default(),
            256,
        )
        .unwrap()
        .image;
        let mut edge_pixels = 0;
        for pixel in image.as_bytes(0).unwrap().as_chunks::<4>().0 {
            if pixel[3] != 0 && pixel[3] != 255 {
                edge_pixels += 1;
                assert!(
                    pixel[2] > 200 && pixel[0] < 40,
                    "colored transparent edge: {pixel:?}"
                );
            }
        }
        assert!(edge_pixels > 0);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let cached = load(
            &path,
            crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
            Budget::default(),
            256,
        )
        .unwrap()
        .image;
        assert_eq!(image.as_bytes(0), cached.as_bytes(0));
    }

    #[test]
    fn small_originals_are_never_upscaled_or_written_as_redundant_tiers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.png");
        fs::write(&path, png(120, 80, [200, 10, 20, 128])).unwrap();
        for tier in [256, 512, 1024, u32::MAX] {
            let loaded = load(
                &path,
                crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
                Budget::default(),
                tier,
            )
            .unwrap();
            assert!(!loaded.animated);
            assert_eq!(loaded.source_side, 120);
            let image = loaded.image;
            assert_eq!(size(&image), (120, 80));
            assert_eq!(&image.as_bytes(0).unwrap()[..4], &[20, 10, 200, 128]);
        }
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn each_source_has_only_useful_discrete_files_and_content_changes_get_new_keys() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.png");
        let bytes = png(800, 400, [240, 12, 20, 255]);
        fs::write(&path, &bytes).unwrap();
        for (tier, expected) in [(256, (256, 128)), (512, (512, 256)), (1024, (800, 400))] {
            assert_eq!(
                size(
                    &load(
                        &path,
                        crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
                        Budget::default(),
                        tier
                    )
                    .unwrap()
                    .image
                ),
                expected
            );
        }
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 3);
        let hex = blake3::hash(&bytes).to_hex();
        for tier in [256, 512] {
            assert!(
                directory
                    .path()
                    .join(format!("{}-{tier}.webp", &hex[2..32]))
                    .is_file()
            );
        }
        let replacement = png(800, 400, [12, 20, 240, 255]);
        fs::write(&path, &replacement).unwrap();
        let image = load(
            &path,
            crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
            Budget::default(),
            256,
        )
        .unwrap()
        .image;
        assert!(image.as_bytes(0).unwrap()[0] > 220);
        assert_ne!(
            thumbnail_path(&path, blake3::hash(&bytes), 256),
            thumbnail_path(&path, blake3::hash(&replacement), 256)
        );
    }

    #[test]
    fn animations_keep_frames_delays_and_never_create_thumbnail_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.gif");
        fs::write(&path, super::super::tests::gif()).unwrap();
        let loaded = load(
            &path,
            crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
            Budget::default(),
            256,
        )
        .unwrap();
        assert!(loaded.animated);
        let image = loaded.image;
        assert_eq!(image.frame_count(), 2);
        assert_eq!(image.delay(0).numer_denom_ms(), (70, 1));
        assert_eq!(image.delay(1).numer_denom_ms(), (130, 1));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn one_frame_animation_formats_report_original_policy_without_frame_count_inference() {
        let directory = tempfile::tempdir().unwrap();
        let mut gif = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut gif)
            .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                2,
                2,
                image::Rgba([240, 10, 20, 255]),
            )))
            .unwrap();
        let mut webp = super::super::tests::webp();
        let second_frame = webp
            .windows(4)
            .enumerate()
            .filter_map(|(offset, bytes)| (bytes == b"ANMF").then_some(offset))
            .nth(1)
            .unwrap();
        webp.truncate(second_frame);
        let riff_size = (webp.len() - 8) as u32;
        webp[4..8].copy_from_slice(&riff_size.to_le_bytes());
        for (name, bytes) in [("single.gif", gif), ("single.webp", webp)] {
            let path = directory.path().join(name);
            fs::write(&path, bytes).unwrap();
            let loaded = load(
                &path,
                crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
                Budget::default(),
                256,
            )
            .unwrap();
            assert!(loaded.animated);
            assert_eq!(loaded.source_side, 2);
            assert_eq!(loaded.image.frame_count(), 1);
        }
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn damaged_or_removed_thumbnails_regenerate_but_never_bypass_source_limits() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = png(800, 400, [220, 22, 35, 255]);
        let hash = blake3::hash(&bytes);
        let path = crate::artwork::cache::original_path(directory.path(), hash, "png");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &bytes).unwrap();
        let thumbnail = thumbnail_path(&path, hash, 256).unwrap();
        for damaged in [true, false] {
            load(
                &path,
                crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
                Budget::default(),
                256,
            )
            .unwrap();
            if damaged {
                fs::write(&thumbnail, b"partial webp").unwrap();
            } else {
                fs::remove_file(&thumbnail).unwrap();
            }
            assert_eq!(
                size(
                    &load(
                        &path,
                        crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
                        Budget::default(),
                        256
                    )
                    .unwrap()
                    .image
                ),
                (256, 128)
            );
        }
        // The cached 256px file would fit, but the 800px source must still pass
        // the unchanged input admission policy before any thumbnail is read.
        assert!(
            load(
                &path,
                crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
                Budget {
                    max_side: 512,
                    ..Budget::default()
                },
                256
            )
            .is_err()
        );
    }

    #[test]
    fn concurrent_source_leases_share_pixels_and_release_after_last_waiter() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.png");
        let bytes = png(800, 400, [210, 30, 40, 255]);
        fs::write(&path, &bytes).unwrap();
        let first = source(
            blake3::hash(&bytes),
            crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
            Budget::default(),
        );
        let second = source(
            blake3::hash(&bytes),
            crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
            Budget::default(),
        );
        assert!(Arc::ptr_eq(&first, &second));
        let image = load(
            &path,
            crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
            Budget::default(),
            256,
        )
        .unwrap()
        .image;
        let repeated = load(
            &path,
            crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
            Budget::default(),
            256,
        )
        .unwrap()
        .image;
        assert!(Arc::ptr_eq(&image, &repeated));
        let original = Arc::clone(first.lock().original.as_ref().unwrap());
        load(
            &path,
            crate::artwork::DEFAULT_MAX_IMAGE_BYTES,
            Budget::default(),
            512,
        )
        .unwrap();
        assert!(Arc::ptr_eq(
            first.lock().original.as_ref().unwrap(),
            &original
        ));
        let weak = Arc::downgrade(&first);
        drop(first);
        drop(second);
        assert!(weak.upgrade().is_none());
    }
}
