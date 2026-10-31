//! Concrete rendering parameters, captured by the host when configuration changes.
use rivu_audio::analysis::AnalysisSettings;
use rivu_format::config::{
    AnalysisWindow, Config, RgbColor, SpectrumBarLayout, SpectrumStyle, VisualizationPalette,
    VisualizationSignal,
};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisualParameters {
    pub analysis_fft_size: u32,
    pub analysis_rate_hz: u32,
    pub analysis_window: AnalysisWindow,
    pub radial_bar_width: f32,
    pub reactive_bloom_intensity: f32,
    pub reactive_db_range: Option<f32>,
    pub radial_fade_when_idle: bool,
    pub radial_inner_diameter: f32,
    pub effect_primary_color: RgbColor,
    pub radial_ring_opacity: f32,
    pub radial_rotation_speed: f32,
    pub effect_secondary_color: RgbColor,
    pub reactive_sensitivity: f32,
    pub oscilloscope_time_span_ms: f32,
    pub oscilloscope_gain: f32,
    pub spectrogram_db_range: Option<f32>,
    pub spectrogram_history_seconds: u32,
    pub spectrogram_interpolate: bool,
    pub spectrum_bar_gravity: f32,
    pub spectrum_bar_hold_ms: u32,
    pub spectrum_bar_layout: SpectrumBarLayout,
    pub spectrum_db_range: Option<f32>,
    pub spectrum_gap: f32,
    pub spectrum_interpolate: bool,
    pub spectrum_peak_gravity: f32,
    pub spectrum_peak_hold_ms: u32,
    pub spectrum_peaks: bool,
    pub spectrum_smoothing_ms: u32,
    pub spectrum_style: SpectrumStyle,
    pub visualization_background: RgbColor,
    pub visualization_palette: VisualizationPalette,
    pub visualization_db_range: f32,
    pub visualization_signal: VisualizationSignal,
    pub waveform_cursor_color: RgbColor,
    pub waveform_glow: f32,
    pub waveform_peak_gain: f32,
    pub waveform_peak_gamma: f32,
    pub waveform_rms_gain: f32,
    pub waveform_seek_enabled: bool,
    pub gpu_material_budget_bytes: u64,
}

impl From<&Config> for VisualParameters {
    fn from(config: &Config) -> Self {
        Self {
            analysis_fft_size: config.analysis_fft_size,
            analysis_rate_hz: config.analysis_rate_hz,
            analysis_window: config.analysis_window,
            radial_bar_width: config.radial_bar_width,
            reactive_bloom_intensity: config.reactive_bloom_intensity,
            reactive_db_range: config.reactive_db_range,
            radial_fade_when_idle: config.radial_fade_when_idle,
            radial_inner_diameter: config.radial_inner_diameter,
            effect_primary_color: config.effect_primary_color,
            radial_ring_opacity: config.radial_ring_opacity,
            radial_rotation_speed: config.radial_rotation_speed,
            effect_secondary_color: config.effect_secondary_color,
            reactive_sensitivity: config.reactive_sensitivity,
            oscilloscope_time_span_ms: config.oscilloscope_time_span_ms,
            oscilloscope_gain: config.oscilloscope_gain,
            spectrogram_db_range: config.spectrogram_db_range,
            spectrogram_history_seconds: config.spectrogram_history_seconds,
            spectrogram_interpolate: config.spectrogram_interpolate,
            spectrum_bar_gravity: config.spectrum_bar_gravity,
            spectrum_bar_hold_ms: config.spectrum_bar_hold_ms,
            spectrum_bar_layout: config.spectrum_bar_layout,
            spectrum_db_range: config.spectrum_db_range,
            spectrum_gap: config.spectrum_gap,
            spectrum_interpolate: config.spectrum_interpolate,
            spectrum_peak_gravity: config.spectrum_peak_gravity,
            spectrum_peak_hold_ms: config.spectrum_peak_hold_ms,
            spectrum_peaks: config.spectrum_peaks,
            spectrum_smoothing_ms: config.spectrum_smoothing_ms,
            spectrum_style: config.spectrum_style,
            visualization_background: config.visualization_background,
            visualization_palette: config.visualization_palette,
            visualization_db_range: config.visualization_db_range,
            visualization_signal: config.visualization_signal,
            waveform_cursor_color: config.waveform_cursor_color,
            waveform_glow: config.waveform_glow,
            waveform_peak_gain: config.waveform_peak_gain,
            waveform_peak_gamma: config.waveform_peak_gamma,
            waveform_rms_gain: config.waveform_rms_gain,
            waveform_seek_enabled: config.waveform_seek_enabled,
            gpu_material_budget_bytes: config.gpu_material_budget_bytes(),
        }
    }
}

impl Default for VisualParameters {
    fn default() -> Self {
        Self::from(&Config::default())
    }
}

impl VisualParameters {
    pub fn analysis_settings(&self) -> AnalysisSettings {
        AnalysisSettings {
            fps: self.analysis_rate_hz,
            fft_size: self.analysis_fft_size,
            window: self.analysis_window,
            signal: self.visualization_signal,
        }
    }
    pub fn spectrum_db_range(&self) -> f32 {
        self.spectrum_db_range
            .unwrap_or(self.visualization_db_range)
    }
    pub fn spectrogram_db_range(&self) -> f32 {
        self.spectrogram_db_range
            .unwrap_or(self.visualization_db_range)
    }
    pub fn reactive_db_range(&self) -> f32 {
        self.reactive_db_range
            .unwrap_or(self.visualization_db_range)
    }
    pub fn gpu_material_budget_bytes(&self) -> u64 {
        self.gpu_material_budget_bytes
    }
}

/// Explicit scan setup; unlike rendering, preparation may clone equalizer state.
pub struct WaveformScanParameters {
    pub signal: VisualizationSignal,
    pub equalizer: Arc<rivu_format::config::EqualizerConfig>,
    pub buffer_mib: u32,
    pub ffmpeg_enabled: bool,
}

impl From<&Config> for WaveformScanParameters {
    fn from(config: &Config) -> Self {
        Self {
            signal: config.visualization_signal,
            equalizer: Arc::new(config.equalizer.clone()),
            buffer_mib: config.media_read_buffer_mib,
            ffmpeg_enabled: config.ffmpeg_enabled,
        }
    }
}
