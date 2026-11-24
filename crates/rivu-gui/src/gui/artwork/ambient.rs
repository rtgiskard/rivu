//! A bounded, static low-frequency image, sampled from the already decoded cover.
use gpui::RenderImage;
use image::{ImageBuffer, Rgba};
use std::sync::Arc;

pub(super) const SIDE: u32 = 64;

pub(super) fn filtered(source: &RenderImage) -> Arc<RenderImage> {
    let size = source.size(0);
    let source = ImageBuffer::<Rgba<u8>, _>::from_raw(
        size.width.0 as u32,
        size.height.0 as u32,
        source.as_bytes(0).expect("decoded artwork frame"),
    )
    .expect("decoded artwork dimensions");
    // Sample the center of each output cell without filtering whole source rows.
    let center = |index: u32, extent: u32| (2 * index + 1) * extent / (2 * SIDE);
    let columns: [u32; SIDE as usize] = std::array::from_fn(|x| center(x as u32, source.width()));
    let sampled = ImageBuffer::from_fn(SIDE, SIDE, |x, y| {
        let mut pixel = *source.get_pixel(columns[x as usize], center(y, source.height()));
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel.0[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
        pixel
    });
    let mut blurred = image::imageops::blur(&sampled, 10.0);
    for pixel in blurred.pixels_mut() {
        if pixel[3] != 0 {
            for channel in 0..3 {
                pixel[channel] = ((u16::from(pixel[channel]) * 255 + u16::from(pixel[3]) / 2)
                    / u16::from(pixel[3]))
                .min(255) as u8;
            }
        }
    }
    Arc::new(RenderImage::new([image::Frame::new(blurred)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atmosphere_preserves_bgra_and_uses_a_static_first_frame() {
        let first = ImageBuffer::from_pixel(12, 8, Rgba([31, 67, 191, 255]));
        let second = ImageBuffer::from_pixel(12, 8, Rgba([230, 0, 0, 255]));
        let source = RenderImage::new(vec![image::Frame::new(first), image::Frame::new(second)]);
        let result = filtered(&source);
        assert_eq!(result.frame_count(), 1);
        assert_eq!(
            result.size(0),
            gpui::size(gpui::DevicePixels(64), gpui::DevicePixels(64))
        );
        for pixel in result.as_bytes(0).unwrap().as_chunks::<4>().0 {
            assert_eq!(*pixel, [31, 67, 191, 255]);
        }
    }

    #[test]
    fn non_square_atmosphere_samples_pixel_centers_on_both_axes() {
        let pixels = ImageBuffer::from_fn(173, 29, |x, y| {
            Rgba([(x * 43 + y * 73) as u8, (x ^ y) as u8, y as u8, 255])
        });
        let expected =
            image::imageops::resize(&pixels, SIDE, SIDE, image::imageops::FilterType::Nearest);
        let expected = image::imageops::blur(&expected, 10.0);
        let result = filtered(&RenderImage::new([image::Frame::new(pixels)]));
        assert_eq!(result.as_bytes(0).unwrap(), expected.as_raw());
    }

    #[test]
    fn transparent_rgb_does_not_bleed_into_visible_atmosphere() {
        let mut pixels = ImageBuffer::from_pixel(64, 64, Rgba([255, 0, 0, 0]));
        for y in 0..64 {
            for x in 0..32 {
                pixels.put_pixel(x, y, Rgba([0, 0, 200, 255]));
            }
        }
        let result = filtered(&RenderImage::new([image::Frame::new(pixels)]));
        let bytes = result.as_bytes(0).unwrap();
        let pixels = bytes.as_chunks::<4>().0;
        assert!(pixels.iter().any(|pixel| pixel[3] > 32));
        assert!(pixels.iter().any(|pixel| pixel[3] < 32));
        for pixel in pixels {
            assert_eq!(pixel[0], 0);
            assert_eq!(pixel[1], 0);
            if pixel[3] > 32 {
                assert!((190..=210).contains(&pixel[2]));
            }
        }
    }
}
