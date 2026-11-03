//! Shared color lookup and lazy GPU palettes for Spectrum, Spectrogram and Waveform.
mod matplotlib;

use gpui::GpuPalette;
use rivu_format::config::VisualizationPalette;
use std::{
    cell::{OnceCell, RefCell},
    rc::Rc,
    sync::{Arc, LazyLock},
};

// Retain the authored black guard point: it determines the cubic tangent at blue.
pub(super) const DEADBEEF_STOPS: &[(f32, [u8; 3])] = &[
    (0.0, [0, 0, 0]),
    (0.06, [0, 32, 100]),
    (0.06 + 0.94 * 0.2, [0, 148, 160]),
    (0.06 + 0.94 * 0.4, [128, 255, 120]),
    (0.06 + 0.94 * 0.6, [255, 255, 0]),
    (0.06 + 0.94 * 0.8, [255, 128, 0]),
    (1.0, [255, 0, 0]),
];

pub(super) const PALETTE_LUT_SIZE: usize = 1024;
pub(super) const MAX_PALETTE_COLORS: usize = PALETTE_LUT_SIZE;

pub(super) struct PaletteLut {
    pub(super) palette: VisualizationPalette,
    colors: &'static [[f32; 3]],
    pub(super) gpu_palette: OnceCell<Result<Arc<GpuPalette>, String>>,
    waveform_gpu_palette: OnceCell<Result<Arc<GpuPalette>, String>>,
}

impl PaletteLut {
    pub(super) fn new(palette: VisualizationPalette) -> Self {
        Self {
            palette,
            colors: match palette {
                VisualizationPalette::Inferno => &matplotlib::INFERNO,
                VisualizationPalette::Magma => &matplotlib::MAGMA,
                VisualizationPalette::Viridis => &matplotlib::VIRIDIS,
                VisualizationPalette::Cividis => &matplotlib::CIVIDIS,
                VisualizationPalette::Grayscale => &[[0.0; 3], [1.0; 3]],
                VisualizationPalette::Deadbeef => {
                    static VISIBLE: LazyLock<[[f32; 3]; PALETTE_LUT_SIZE]> = LazyLock::new(|| {
                        std::array::from_fn(|index| {
                            let fraction = index as f32 / (PALETTE_LUT_SIZE - 1) as f32;
                            // Sample after the authored black guard point; all
                            // consumers receive the same visible 0..1 domain.
                            gradient_channels(0.06 + fraction * 0.94, DEADBEEF_STOPS)
                                .map(|channel| channel / 255.0)
                        })
                    });
                    &*VISIBLE
                }
            },
            gpu_palette: OnceCell::new(),
            waveform_gpu_palette: OnceCell::new(),
        }
    }

    pub(super) fn gpu_palette(&self) -> Result<Arc<GpuPalette>, String> {
        self.gpu_palette
            .get_or_init(|| {
                let mut colors = [[0.0; 4]; MAX_PALETTE_COLORS];
                for (color, &[r, g, b]) in colors.iter_mut().zip(self.colors) {
                    *color = [r, g, b, 1.0];
                }
                GpuPalette::new(&colors[..self.colors.len()], MAX_PALETTE_COLORS)
                    .map_err(|error| format!("Cannot create visualization GPU palette: {error}"))
            })
            .clone()
    }

    // Adjacent pairs keep each RMS ramp a two-color RGB mix, not a palette sweep.
    pub(super) fn waveform_gpu_palette(&self) -> Result<Arc<GpuPalette>, String> {
        self.waveform_gpu_palette
            .get_or_init(|| {
                let floor = self.colors[0];
                let neutral = floor.iter().sum::<f32>() / 3.0;
                let low = floor.map(|value| (value + 3.0 * neutral) / 6.0);
                let colors: Vec<_> = self
                    .colors
                    .iter()
                    .flat_map(|channels| [low, *channels].map(|[r, g, b]| [r, g, b, 1.0]))
                    .collect();
                GpuPalette::new(&colors, PALETTE_LUT_SIZE * 2).map_err(|error| error.to_string())
            })
            .clone()
    }

    pub(super) fn lookup_rgb(&self, fraction: f32) -> [u8; 3] {
        let position = fraction.clamp(0.0, 1.0) * (self.colors.len() - 1) as f32;
        let left = position.floor() as usize;
        let right = (left + 1).min(self.colors.len() - 1);
        let mix = position - left as f32;
        std::array::from_fn(|channel| {
            ((self.colors[left][channel] * (1.0 - mix) + self.colors[right][channel] * mix) * 255.0)
                .round() as u8
        })
    }
}

thread_local! {
    static ACTIVE_PALETTE_LUT: RefCell<Option<Rc<PaletteLut>>> = const { RefCell::new(None) };
}

pub(super) fn active_palette_lut(palette: VisualizationPalette) -> Rc<PaletteLut> {
    ACTIVE_PALETTE_LUT.with_borrow_mut(|active| {
        if active.as_ref().is_none_or(|lut| lut.palette != palette) {
            *active = Some(Rc::new(PaletteLut::new(palette)));
        }
        Rc::clone(active.as_ref().expect("active palette LUT is initialized"))
    })
}

/// Preserve the authored cubic tangents before sampling the display domain.
fn gradient_channels(value: f32, stops: &[(f32, [u8; 3])]) -> [f32; 3] {
    debug_assert!(stops.len() >= 2);
    let value = value.clamp(0.0, 1.0);
    let index = stops
        .windows(2)
        .position(|pair| value <= pair[1].0)
        .unwrap_or(stops.len() - 2);
    let (x0, c0) = stops[index];
    let (x1, c1) = stops[index + 1];
    let span = (x1 - x0).max(f32::EPSILON);
    let t = ((value - x0) / span).clamp(0.0, 1.0);
    let slope = |point: usize, channel: usize| {
        if point == 0 {
            (stops[1].1[channel] as f32 - stops[0].1[channel] as f32)
                / (stops[1].0 - stops[0].0).max(f32::EPSILON)
        } else if point + 1 == stops.len() {
            let left = point - 1;
            (stops[point].1[channel] as f32 - stops[left].1[channel] as f32)
                / (stops[point].0 - stops[left].0).max(f32::EPSILON)
        } else {
            let left = point - 1;
            let right = point + 1;
            (stops[right].1[channel] as f32 - stops[left].1[channel] as f32)
                / (stops[right].0 - stops[left].0).max(f32::EPSILON)
        }
    };
    let t2 = t * t;
    let t3 = t2 * t;
    let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
    let h10 = t3 - 2.0 * t2 + t;
    let h01 = -2.0 * t3 + 3.0 * t2;
    let h11 = t3 - t2;
    std::array::from_fn(|channel| {
        let value = h00 * c0[channel] as f32
            + h10 * span * slope(index, channel)
            + h01 * c1[channel] as f32
            + h11 * span * slope(index + 1, channel);
        value.clamp(0.0, 255.0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_lut_interpolation_preserves_authored_colors_within_one_rgb_step() {
        let lut = PaletteLut::new(VisualizationPalette::Deadbeef);
        let waveform = lut.waveform_gpu_palette().unwrap();
        assert_eq!(lut.lookup_rgb(0.0), [0, 32, 100]);
        assert_eq!(lut.lookup_rgb(1.0), [255, 0, 0]);
        // Uniform samples preserve the authored cubic without a shader crop.
        for step in 0..=2048 {
            let fraction = step as f32 / 2048.0;
            let position = fraction * (PALETTE_LUT_SIZE - 1) as f32;
            let left = position.floor() as usize;
            let right = (left + 1).min(PALETTE_LUT_SIZE - 1);
            let mix = position - left as f32;
            let expected = gradient_channels(0.06 + fraction * 0.94, DEADBEEF_STOPS);
            for (channel, expected) in expected.iter().enumerate() {
                let low = waveform.colors()[left * 2 + 1][channel];
                let high = waveform.colors()[right * 2 + 1][channel];
                let actual = low + (high - low) * mix;
                assert!((actual - expected / 255.0).abs() < 1.0 / 255.0);
            }
        }
    }

    #[test]
    fn standard_palettes_preserve_every_official_sample() {
        for (palette, samples) in [
            (VisualizationPalette::Inferno, &matplotlib::INFERNO),
            (VisualizationPalette::Magma, &matplotlib::MAGMA),
            (VisualizationPalette::Viridis, &matplotlib::VIRIDIS),
            (VisualizationPalette::Cividis, &matplotlib::CIVIDIS),
        ] {
            let lut = PaletteLut::new(palette);
            let gpu = lut.gpu_palette().unwrap();
            assert_eq!(gpu.colors().len(), samples.len());
            for (actual, &[r, g, b]) in gpu.colors().iter().zip(samples) {
                assert_eq!(*actual, [r, g, b, 1.0]);
            }
        }
    }

    #[test]
    fn shared_palettes_keep_waveform_rms_mixes_and_peak_colors() {
        for palette in [
            VisualizationPalette::Inferno,
            VisualizationPalette::Magma,
            VisualizationPalette::Viridis,
            VisualizationPalette::Cividis,
            VisualizationPalette::Grayscale,
            VisualizationPalette::Deadbeef,
        ] {
            let lut = active_palette_lut(palette);
            assert!(Rc::ptr_eq(&lut, &active_palette_lut(palette)));
            let gpu = lut.gpu_palette().unwrap();
            assert!(Arc::ptr_eq(&gpu, &lut.gpu_palette().unwrap()));
            let waveform = lut.waveform_gpu_palette().unwrap();
            let floor = lut.colors[0];
            let neutral = floor.iter().sum::<f32>() / 3.0;
            let low = floor.map(|value| (value + 3.0 * neutral) / 6.0);
            assert_eq!(waveform.colors().len(), gpu.colors().len() * 2);
            for (pair, color) in waveform
                .colors()
                .as_chunks::<2>()
                .0
                .iter()
                .zip(gpu.colors())
            {
                assert_eq!(pair[0], [low[0], low[1], low[2], 1.0]);
                assert_eq!(pair[1], *color);
            }
        }
    }
}
