//! Vertical triangle-filter accumulation for straight-alpha BGRA thumbnail sources.

pub(super) fn accumulator(alpha: bool) -> impl Fn(&mut [[f32; 4]], &[[u8; 4]], f32) {
    #[cfg(target_arch = "x86_64")]
    let avx2 = std::is_x86_feature_detected!("avx2");
    move |row, source, weight| {
        #[cfg(target_arch = "x86_64")]
        if avx2 {
            // SAFETY: The detected feature applies for the lifetime of this resize.
            unsafe {
                if alpha {
                    accumulate_avx2::<true>(row, source, weight);
                } else {
                    accumulate_avx2::<false>(row, source, weight);
                }
            }
            return;
        }
        accumulate_scalar(row, source, weight, alpha);
    }
}

fn accumulate_scalar(row: &mut [[f32; 4]], source: &[[u8; 4]], weight: f32, alpha: bool) {
    if alpha {
        for (accumulator, pixel) in row.iter_mut().zip(source) {
            let alpha = u32::from(pixel[3]);
            for channel in 0..3 {
                let premultiplied = (u32::from(pixel[channel]) * alpha * 257 + 127) / 255;
                accumulator[channel] += premultiplied as f32 * weight;
            }
            accumulator[3] += (alpha * 257) as f32 * weight;
        }
    } else {
        for (accumulator, pixel) in row.iter_mut().zip(source) {
            for channel in 0..4 {
                accumulator[channel] += f32::from(pixel[channel]) * weight;
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn accumulate_avx2<const ALPHA: bool>(
    row: &mut [[f32; 4]],
    source: &[[u8; 4]],
    weight: f32,
) {
    use std::arch::x86_64::*;

    let lanes = _mm256_set1_ps(weight);
    let (rows, row_tail) = row.as_chunks_mut::<2>();
    let (pixels, source_tail) = source.as_chunks::<2>();
    for (accumulator, pair) in rows.iter_mut().zip(pixels) {
        // SAFETY: Each pair contains eight contiguous bytes and each accumulator
        // contains eight contiguous f32s. These loads and stores permit unaligned addresses.
        unsafe {
            let bytes = _mm_loadl_epi64(pair.as_ptr().cast());
            let values = _mm256_cvtepu8_epi32(bytes);
            let values = if ALPHA {
                let alpha = _mm256_shuffle_epi32::<0xff>(values);
                let channels = _mm256_blend_epi32::<0x88>(values, _mm256_set1_epi32(255));
                let product = _mm256_mullo_epi16(channels, alpha);
                // For p = channel * alpha, (257*p + 127)/255 = p + (2*p + 127)/255.
                // For n = 2*p + 127, floor(n/255) = ((n+1) * 0x01010101) >> 32.
                // Factor 0x01010101 as 257 * 65537 to stay in 32-bit lanes.
                let t = _mm256_mullo_epi32(
                    _mm256_add_epi32(product, _mm256_set1_epi32(64)),
                    _mm256_set1_epi32(514),
                );
                let correction =
                    _mm256_srli_epi32::<16>(_mm256_add_epi32(t, _mm256_srli_epi32::<16>(t)));
                _mm256_add_epi32(product, correction)
            } else {
                values
            };
            let dst = accumulator.as_mut_ptr().cast::<f32>();
            let weighted = _mm256_mul_ps(_mm256_cvtepi32_ps(values), lanes);
            _mm256_storeu_ps(dst, _mm256_add_ps(_mm256_loadu_ps(dst), weighted));
        }
    }
    accumulate_scalar(row_tail, source_tail, weight, ALPHA);
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;

    #[test]
    fn avx2_preserves_scalar_premultiplication_for_all_channel_and_alpha_values() {
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }
        let accumulate = accumulator(true);
        for alpha in 0..=255u8 {
            // An odd-width row puts the last pixel through the scalar tail.
            let source: [[u8; 4]; 257] = std::array::from_fn(|x| {
                let value = x as u8;
                [value, 255 - value, value.rotate_left(3), alpha]
            });
            let mut actual = [[1.5; 4]; 257];
            let mut expected = actual;
            for weight in [0.37, 0.125] {
                accumulate(&mut actual, &source, weight);
                accumulate_scalar(&mut expected, &source, weight, true);
            }
            assert_eq!(actual, expected, "alpha={alpha}");
        }
    }
}
