//! Shared color lookup and lazy GPU palettes for frequency plots and track previews.
use gpui::GpuPalette;
use rivu_format::config::VisualizationPalette;
use std::{
    cell::{OnceCell, RefCell},
    rc::Rc,
    sync::Arc,
};

pub(super) const TOKYO_NIGHT_STOPS: &[(f32, [u8; 3])] = &[
    (0.0, [65, 72, 110]),
    (0.2, [187, 154, 247]),
    (0.42, [125, 207, 255]),
    (0.62, [158, 206, 106]),
    (0.82, [224, 175, 104]),
    (1.0, [247, 118, 142]),
];
// Preserve DeaDBeeF's six color positions after a small black low-end floor.
pub(super) const DEADBEEF_STOPS: &[(f32, [u8; 3])] = &[
    (0.0, [0, 0, 0]),
    (0.06, [0, 32, 100]),
    (0.06 + 0.94 * 0.2, [0, 148, 160]),
    (0.06 + 0.94 * 0.4, [128, 255, 120]),
    (0.06 + 0.94 * 0.6, [255, 255, 0]),
    (0.06 + 0.94 * 0.8, [255, 128, 0]),
    (1.0, [255, 0, 0]),
];

pub(super) fn palette_stops(palette: VisualizationPalette) -> &'static [(f32, [u8; 3])] {
    match palette {
        VisualizationPalette::TokyoNight => TOKYO_NIGHT_STOPS,
        VisualizationPalette::Deadbeef => DEADBEEF_STOPS,
    }
}

pub(super) fn visible_palette_fraction(palette: VisualizationPalette, fraction: f32) -> f32 {
    let fraction = fraction.clamp(0.0, 1.0);
    match palette {
        VisualizationPalette::Deadbeef => 0.06 + fraction * 0.94,
        VisualizationPalette::TokyoNight => fraction,
    }
}

pub(super) const PALETTE_LUT_SIZE: usize = 1024;
pub(super) const SPECTRUM_GPU_PALETTE_SIZE: usize = PALETTE_LUT_SIZE;

pub(super) struct PaletteLut {
    pub(super) palette: VisualizationPalette,
    colors: [[u8; 3]; PALETTE_LUT_SIZE],
    pub(super) spectrum_gpu_palette: OnceCell<Result<Arc<GpuPalette>, String>>,
    spectrogram_gpu_palette: OnceCell<Result<Arc<GpuPalette>, String>>,
    waveform_gpu_palette: OnceCell<Result<Arc<GpuPalette>, String>>,
}

impl PaletteLut {
    pub(super) fn new(palette: VisualizationPalette) -> Self {
        Self {
            palette,
            colors: std::array::from_fn(|index| {
                let fraction = index as f32 / (PALETTE_LUT_SIZE - 1) as f32;
                gradient(fraction, palette_stops(palette))
            }),
            spectrum_gpu_palette: OnceCell::new(),
            spectrogram_gpu_palette: OnceCell::new(),
            waveform_gpu_palette: OnceCell::new(),
        }
    }

    pub(super) fn spectrum_gpu_palette(&self) -> Result<Arc<GpuPalette>, String> {
        self.spectrum_gpu_palette
            .get_or_init(|| {
                let colors: [[f32; 4]; SPECTRUM_GPU_PALETTE_SIZE] = std::array::from_fn(|index| {
                    let fraction = index as f32 / (SPECTRUM_GPU_PALETTE_SIZE - 1) as f32;
                    let [red, green, blue] = gradient(
                        visible_palette_fraction(self.palette, fraction),
                        palette_stops(self.palette),
                    );
                    [red, green, blue, 255].map(|channel| channel as f32 / 255.0)
                });
                GpuPalette::new(&colors, SPECTRUM_GPU_PALETTE_SIZE)
                    .map_err(|error| format!("Cannot create spectrum GPU palette: {error}"))
            })
            .clone()
    }

    pub(super) fn spectrogram_gpu_palette(&self) -> Result<Arc<GpuPalette>, String> {
        self.spectrogram_gpu_palette
            .get_or_init(|| {
                let colors: [[f32; 4]; PALETTE_LUT_SIZE] = std::array::from_fn(|index| {
                    let [r, g, b] = self.colors[index];
                    [r, g, b, 255].map(|v| v as f32 / 255.0)
                });
                GpuPalette::new(&colors, PALETTE_LUT_SIZE).map_err(|error| error.to_string())
            })
            .clone()
    }

    // Adjacent pairs keep each RMS ramp a two-color RGB mix, not a palette sweep.
    pub(super) fn waveform_gpu_palette(&self) -> Result<Arc<GpuPalette>, String> {
        self.waveform_gpu_palette
            .get_or_init(|| {
                let stops = palette_stops(self.palette);
                let floor = gradient_channels(visible_palette_fraction(self.palette, 0.0), stops);
                let neutral = floor.iter().sum::<f32>() / 3.0;
                let low = floor.map(|value| (value + 3.0 * neutral) / (6.0 * 255.0));
                let colors: Vec<_> = (0..PALETTE_LUT_SIZE)
                    .flat_map(|index| {
                        let fraction = index as f32 / (PALETTE_LUT_SIZE - 1) as f32;
                        let high = gradient_channels(fraction, stops).map(|value| value / 255.0);
                        [low, high].map(|[r, g, b]| [r, g, b, 1.0])
                    })
                    .collect();
                GpuPalette::new(&colors, PALETTE_LUT_SIZE * 2).map_err(|error| error.to_string())
            })
            .clone()
    }

    pub(super) fn waveform_gpu_params(&self) -> gpui::GpuWaveformParams {
        gpui::GpuWaveformParams {
            palette_start: visible_palette_fraction(self.palette, 0.0),
        }
    }

    pub(super) fn lookup_rgb(&self, fraction: f32) -> [u8; 3] {
        let index = (fraction.clamp(0.0, 1.0) * (PALETTE_LUT_SIZE - 1) as f32).round() as usize;
        self.colors[index]
    }

    // Waveform and Spectrum exclude DeaDBeeF's black floor; Spectrogram does not.
    pub(super) fn lookup_mapped_rgb(&self, fraction: f32) -> [u8; 3] {
        self.lookup_rgb(visible_palette_fraction(self.palette, fraction))
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

/// Cubic Hermite interpolation preserves the palette position and tangent
/// continuously inside each authored color-stop interval. The lookup table and
/// Spectrum's shared GPU palette samples this same authored mapping.
pub(super) fn gradient(value: f32, stops: &[(f32, [u8; 3])]) -> [u8; 3] {
    gradient_channels(value, stops).map(|channel| channel.round() as u8)
}

pub(super) fn gradient_channels(value: f32, stops: &[(f32, [u8; 3])]) -> [f32; 3] {
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
