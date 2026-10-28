// Fixed 2:1 Lanczos3 reduction for the supersampled default artwork.
// Preserve image::imageops::resize's vertical-then-horizontal f32 arithmetic,
// but retain only one intermediate row instead of a full floating-point image.
use image::{ImageBuffer, Rgba, RgbaImage};

struct Tap {
    start: usize,
    weights: [f32; 12],
    len: usize,
}

fn taps(side: usize) -> Vec<Tap> {
    fn sinc(x: f32) -> f32 {
        if x == 0.0 {
            1.0
        } else {
            let a = x * std::f32::consts::PI;
            a.sin() / a
        }
    }
    (0..side)
        .map(|position| {
            let center = (position as f32 + 0.5) * 2.0;
            let start = ((center - 6.0).floor() as i64).clamp(0, (side * 2 - 1) as i64) as usize;
            let end = ((center + 6.0).ceil() as usize).clamp(start + 1, side * 2);
            let mut tap = Tap {
                start,
                weights: [0.0; 12],
                len: end - start,
            };
            let mut sum = 0.0;
            for (offset, weight) in tap.weights[..tap.len].iter_mut().enumerate() {
                let x = ((start + offset) as f32 - (center - 0.5)) / 2.0;
                *weight = if x.abs() < 3.0 {
                    sinc(x) * sinc(x / 3.0)
                } else {
                    0.0
                };
                sum += *weight;
            }
            for weight in &mut tap.weights[..tap.len] {
                *weight /= sum;
            }
            tap
        })
        .collect()
}

pub(super) fn reduce_default(sampled: &[u8], side: u32) -> RgbaImage {
    let side = side as usize;
    assert!(side > 0);
    let width = side * 2;
    assert_eq!(sampled.len(), width * width * 4);
    let taps = taps(side);
    let mut row = vec![[0.0_f32; 4]; width];
    let mut output = ImageBuffer::<Rgba<u8>, _>::new(side as u32, side as u32);
    for (y, vertical) in taps.iter().enumerate() {
        row.fill([0.0; 4]);
        for (offset, weight) in vertical.weights[..vertical.len].iter().enumerate() {
            let start = (vertical.start + offset) * width * 4;
            let source = &sampled[start..start + width * 4];
            for (pixel, source) in row.iter_mut().zip(source.as_chunks::<4>().0) {
                for channel in 0..4 {
                    pixel[channel] += f32::from(source[channel]) * weight;
                }
            }
        }
        for (x, horizontal) in taps.iter().enumerate() {
            let mut pixel = [0.0_f32; 4];
            for (offset, weight) in horizontal.weights[..horizontal.len].iter().enumerate() {
                let source = row[horizontal.start + offset];
                for channel in 0..4 {
                    pixel[channel] += source[channel] * weight;
                }
            }
            output.put_pixel(
                x as u32,
                y as u32,
                Rgba(pixel.map(|v| v.clamp(0.0, 255.0).round() as u8)),
            );
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduction_preserves_lanczos_pixels_at_edges_and_interior() {
        for side in [1, 2, 3, 7, 17, 139, 256] {
            let mut source = RgbaImage::new(side * 2, side * 2);
            let mut state = 12345_u32;
            for byte in source.as_mut() {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                *byte = (state >> 24) as u8;
            }
            assert_eq!(
                reduce_default(source.as_raw(), side),
                image::imageops::resize(&source, side, side, image::imageops::FilterType::Lanczos3),
                "side={side}"
            );
        }
    }
}
