//! A bounded, static low-frequency image, sampled from the already decoded cover.
use gpui::RenderImage;
use image::{ImageBuffer, Rgba};
use std::sync::Arc;

const SIDE: u32 = 64;

pub(super) fn filtered(source: &RenderImage) -> Arc<RenderImage> {
    let size = source.size(0);
    let source = ImageBuffer::<Rgba<u8>, _>::from_raw(
        size.width.0 as u32,
        size.height.0 as u32,
        source.as_bytes(0).expect("decoded artwork frame"),
    )
    .expect("decoded artwork dimensions");
    // The atmosphere needs only low frequencies. Nearest sampling bounds the
    // work to the small output instead of allocating a cover-sized filter pass.
    let mut sampled =
        image::imageops::resize(&source, SIDE, SIDE, image::imageops::FilterType::Nearest);
    for pixel in sampled.pixels_mut() {
        for channel in 0..3 {
            pixel[channel] = ((u16::from(pixel[channel]) * u16::from(pixel[3]) + 127) / 255) as u8;
        }
    }
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
