//! Causal edge alignment and wide accumulation for graph mixing.

/// A zero-initialized delay measured in interleaved samples.
pub struct DelayLine {
    samples: Vec<f32>,
    position: usize,
}

impl DelayLine {
    pub fn new(samples: usize) -> Self {
        Self {
            samples: vec![0.0; samples],
            position: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Add this edge's delayed, scaled contribution to the shared accumulator.
    #[inline]
    pub fn add(&mut self, input: &[f32], output: &mut [f64], gain: f64) {
        if self.samples.is_empty() {
            for (output, &input) in output.iter_mut().zip(input) {
                *output += f64::from(input) * gain;
            }
        } else {
            let mut position = self.position;
            for (output, &input) in output.iter_mut().zip(input) {
                let delayed = std::mem::replace(&mut self.samples[position], input);
                *output += f64::from(delayed) * gain;
                position += 1;
                if position == self.samples.len() {
                    position = 0;
                }
            }
            self.position = position;
        }
    }

    pub fn reset(&mut self) {
        self.samples.fill(0.0);
        self.position = 0;
    }
}

/// Round each sum once to PCM. Returns false for unequal lengths or nonfinite
/// PCM. On failure the output may be partially written and must not be published.
#[inline]
pub fn write_finite_pcm(sum: &[f64], output: &mut [f32]) -> bool {
    if sum.len() != output.len() {
        return false;
    }
    for (sample, &sum) in output.iter_mut().zip(sum) {
        *sample = sum as f32;
        if !sample.is_finite() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn incomplete_pcm_conversion_is_rejected() {
        assert!(!super::write_finite_pcm(&[1.0], &mut [0.0, f32::NAN]));
        assert!(!super::write_finite_pcm(&[1.0, 2.0], &mut [0.0]));
    }
}
