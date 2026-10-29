//! Concrete rendering parameters, captured by the host when configuration changes.
use rivu_audio::analysis::AnalysisSettings;
use rivu_format::config::{
    AnalysisWindow, Config, RgbColor, SpectrumStyle, VisualizationPalette, VisualizationSignal,
};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisualParameters {
    pub analysis_fft_size: u32,
    pub analysis_fps: u32,
    pub analysis_window: AnalysisWindow,
    pub audio_visualizer_bar_glow_layers: u32,
    pub audio_visualizer_bar_width: f32,
    pub audio_visualizer_bloom_intensity: f32,
    pub audio_visualizer_db_range: Option<f32>,
    pub audio_visualizer_fade_when_idle: bool,
    pub audio_visualizer_inner_diameter: f32,
    pub audio_visualizer_primary_color: RgbColor,
    pub audio_visualizer_ring_opacity: f32,
    pub audio_visualizer_rotation_speed: f32,
    pub audio_visualizer_secondary_color: RgbColor,
    pub audio_visualizer_sensitivity: f32,
    pub oscilloscope_time_span_ms: f32,
    pub oscilloscope_gain: f32,
    pub spectrogram_accumulation: u32,
    pub spectrogram_db_range: Option<f32>,
    pub spectrogram_flow_speed: f32,
    pub spectrogram_history_seconds: u32,
    pub spectrogram_interpolate: bool,
    pub spectrogram_interpolation_points: u32,
    pub spectrogram_sampling_points_scale: f32,
    pub spectrum_bar_gravity: f32,
    pub spectrum_bar_hold_ms: u32,
    pub spectrum_bar_width: f32,
    pub spectrum_bars: u32,
    pub spectrum_db_range: Option<f32>,
    pub spectrum_gap: f32,
    pub spectrum_interpolate: bool,
    pub spectrum_peak_gravity: f32,
    pub spectrum_peak_hold_ms: u32,
    pub spectrum_peaks: bool,
    pub spectrum_smoothing_ms: u32,
    pub spectrum_style: SpectrumStyle,
    pub visual_background: RgbColor,
    pub visual_palette: VisualizationPalette,
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
            analysis_fps: config.analysis_fps,
            analysis_window: config.analysis_window,
            audio_visualizer_bar_glow_layers: config.audio_visualizer_bar_glow_layers,
            audio_visualizer_bar_width: config.audio_visualizer_bar_width,
            audio_visualizer_bloom_intensity: config.audio_visualizer_bloom_intensity,
            audio_visualizer_db_range: config.audio_visualizer_db_range,
            audio_visualizer_fade_when_idle: config.audio_visualizer_fade_when_idle,
            audio_visualizer_inner_diameter: config.audio_visualizer_inner_diameter,
            audio_visualizer_primary_color: config.audio_visualizer_primary_color,
            audio_visualizer_ring_opacity: config.audio_visualizer_ring_opacity,
            audio_visualizer_rotation_speed: config.audio_visualizer_rotation_speed,
            audio_visualizer_secondary_color: config.audio_visualizer_secondary_color,
            audio_visualizer_sensitivity: config.audio_visualizer_sensitivity,
            oscilloscope_time_span_ms: config.oscilloscope_time_span_ms,
            oscilloscope_gain: config.oscilloscope_gain,
            spectrogram_accumulation: config.spectrogram_accumulation,
            spectrogram_db_range: config.spectrogram_db_range,
            spectrogram_flow_speed: config.spectrogram_flow_speed,
            spectrogram_history_seconds: config.spectrogram_history_seconds,
            spectrogram_interpolate: config.spectrogram_interpolate,
            spectrogram_interpolation_points: config.spectrogram_interpolation_points,
            spectrogram_sampling_points_scale: config.spectrogram_sampling_points_scale,
            spectrum_bar_gravity: config.spectrum_bar_gravity,
            spectrum_bar_hold_ms: config.spectrum_bar_hold_ms,
            spectrum_bar_width: config.spectrum_bar_width,
            spectrum_bars: config.spectrum_bars,
            spectrum_db_range: config.spectrum_db_range,
            spectrum_gap: config.spectrum_gap,
            spectrum_interpolate: config.spectrum_interpolate,
            spectrum_peak_gravity: config.spectrum_peak_gravity,
            spectrum_peak_hold_ms: config.spectrum_peak_hold_ms,
            spectrum_peaks: config.spectrum_peaks,
            spectrum_smoothing_ms: config.spectrum_smoothing_ms,
            spectrum_style: config.spectrum_style,
            visual_background: config.visual_background,
            visual_palette: config.visual_palette,
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
            fps: self.analysis_fps,
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
    pub fn audio_visualizer_db_range(&self) -> f32 {
        self.audio_visualizer_db_range
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
    pub buffer_mb: u32,
    pub ffmpeg_enabled: bool,
}

impl From<&Config> for WaveformScanParameters {
    fn from(config: &Config) -> Self {
        Self {
            signal: config.visualization_signal,
            equalizer: Arc::new(config.equalizer.clone()),
            buffer_mb: config.media_read_buffer_mb,
            ffmpeg_enabled: config.ffmpeg_enabled,
        }
    }
}
