use super::WireSpectrumBarLayout;
use crate::model::{ConfigMutation, ConfigPatch};
use rivu_format::config::LibraryColumn;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
pub enum WireConfigMutation {
    ToggleVisualization,
    ToggleWaveformSeek,
    ToggleLibraryColumn(WireLibraryColumn),
    AddLibraryRoot(PathBuf),
    RemoveLibraryRoot(PathBuf),
    Settings(Box<WireConfigPatch>),
    Material(rivu_format::config::MaterialChange),
}

#[derive(Serialize, Deserialize)]
pub enum WireLibraryColumn {
    Artist,
    Album,
    Duration,
    Codec,
    Bitrate,
}

impl From<ConfigMutation> for WireConfigMutation {
    fn from(value: ConfigMutation) -> Self {
        match value {
            ConfigMutation::ToggleVisualization => Self::ToggleVisualization,
            ConfigMutation::ToggleWaveformSeek => Self::ToggleWaveformSeek,
            ConfigMutation::ToggleLibraryColumn(column) => {
                Self::ToggleLibraryColumn(match column {
                    LibraryColumn::Artist => WireLibraryColumn::Artist,
                    LibraryColumn::Album => WireLibraryColumn::Album,
                    LibraryColumn::Duration => WireLibraryColumn::Duration,
                    LibraryColumn::Codec => WireLibraryColumn::Codec,
                    LibraryColumn::Bitrate => WireLibraryColumn::Bitrate,
                })
            }
            ConfigMutation::AddLibraryRoot(path) => Self::AddLibraryRoot(path),
            ConfigMutation::RemoveLibraryRoot(path) => Self::RemoveLibraryRoot(path),
            ConfigMutation::Settings(patch) => Self::Settings(Box::new((*patch).into())),
            ConfigMutation::Material(change) => Self::Material(change),
        }
    }
}

impl From<WireConfigMutation> for ConfigMutation {
    fn from(value: WireConfigMutation) -> Self {
        match value {
            WireConfigMutation::ToggleVisualization => Self::ToggleVisualization,
            WireConfigMutation::ToggleWaveformSeek => Self::ToggleWaveformSeek,
            WireConfigMutation::ToggleLibraryColumn(column) => {
                Self::ToggleLibraryColumn(match column {
                    WireLibraryColumn::Artist => LibraryColumn::Artist,
                    WireLibraryColumn::Album => LibraryColumn::Album,
                    WireLibraryColumn::Duration => LibraryColumn::Duration,
                    WireLibraryColumn::Codec => LibraryColumn::Codec,
                    WireLibraryColumn::Bitrate => LibraryColumn::Bitrate,
                })
            }
            WireConfigMutation::AddLibraryRoot(path) => Self::AddLibraryRoot(path),
            WireConfigMutation::RemoveLibraryRoot(path) => Self::RemoveLibraryRoot(path),
            WireConfigMutation::Settings(patch) => Self::Settings(Box::new((*patch).into())),
            WireConfigMutation::Material(change) => Self::Material(change),
        }
    }
}

macro_rules! declare_config_patch {
    ($( $(#[$attr:meta])* $field:ident: $ty:ty, )*) => {
        /// Positional optional fields, without human-readable Config serde attributes.
        /// Nested options distinguish unchanged fields from explicitly clearing them.
        #[derive(Serialize, Deserialize)]
        pub struct WireConfigPatch {
            $( $(#[$attr])* $field: Option<$ty>, )*
        }
    };
}

config_wire_fields!(declare_config_patch);

impl From<ConfigPatch> for WireConfigPatch {
    fn from(value: ConfigPatch) -> Self {
        Self {
            library_roots: value.library_roots,
            scan_max_depth: value.scan_max_depth,
            scan_min_duration_seconds: value.scan_min_duration_seconds,
            output_device: value.output_device,
            volume: value.volume,
            shuffle: value.shuffle,
            repeat: value.repeat,
            play_count_threshold_percent: value.play_count_threshold_percent,
            tray_enabled: value.tray_enabled,
            confirm_queue_replace: value.confirm_queue_replace,
            mpris_enabled: value.mpris_enabled,
            log_level: value.log_level,
            log_to_file: value.log_to_file,
            log_max_files: value.log_max_files,
            query_batch_size: value.query_batch_size,
            library_columns: value
                .library_columns
                .map(|v| [v.artist, v.album, v.duration, v.codec, v.bitrate]),
            ui_font: value.ui_font,
            ui_font_size_pt: value.ui_font_size_pt,
            ui_language: value.ui_language,
            ui_theme: value.ui_theme,
            material: value.material,
            ui_scale: value.ui_scale,
            layout_snap_enabled: value.layout_snap_enabled,
            layout_snap_grid_px: value.layout_snap_grid_px,
            visualization_enabled: value.visualization_enabled,
            visualization_signal: value.visualization_signal,
            analysis_rate_hz: value.analysis_rate_hz,
            visualization_fps: value.visualization_fps,
            visualization_background: value.visualization_background,
            visualization_palette: value.visualization_palette,
            spectrum_style: value.spectrum_style,
            analysis_fft_size: value.analysis_fft_size,
            analysis_window: value.analysis_window,
            spectrum_interpolate: value.spectrum_interpolate,
            spectrum_bar_layout: value.spectrum_bar_layout.map(Into::into),
            spectrum_gap: value.spectrum_gap,
            spectrum_peaks: value.spectrum_peaks,
            spectrum_peak_hold_ms: value.spectrum_peak_hold_ms,
            spectrum_peak_gravity: value.spectrum_peak_gravity,
            spectrum_bar_hold_ms: value.spectrum_bar_hold_ms,
            spectrum_bar_gravity: value.spectrum_bar_gravity,
            spectrum_smoothing_ms: value.spectrum_smoothing_ms,
            default_visualizer_style: value.default_visualizer_style,
            default_visualizer_reference_lines: value.default_visualizer_reference_lines,
            default_visualizer_labels: value.default_visualizer_labels,
            reactive_sensitivity: value.reactive_sensitivity,
            radial_rotation_speed: value.radial_rotation_speed,
            radial_bar_width: value.radial_bar_width,
            radial_ring_opacity: value.radial_ring_opacity,
            reactive_bloom_intensity: value.reactive_bloom_intensity,
            radial_inner_diameter: value.radial_inner_diameter,
            radial_fade_when_idle: value.radial_fade_when_idle,
            effect_primary_color: value.effect_primary_color,
            effect_secondary_color: value.effect_secondary_color,
            artwork_style: value.artwork_style,
            artwork_mode: value.artwork_mode,
            artwork_period_seconds: value.artwork_period_seconds,
            artwork_direction: value.artwork_direction,
            artwork_fps: value.artwork_fps,
            artwork_file_limit_mib: value.artwork_file_limit_mib,
            artwork_decode_budget_mib: value.artwork_decode_budget_mib,
            gpu_material_budget_mib: value.gpu_material_budget_mib,
            gpu_filter_budget_mib: value.gpu_filter_budget_mib,
            gpu_filter_max_depth: value.gpu_filter_max_depth,
            spectrogram_interpolate: value.spectrogram_interpolate,
            waveform_seek_enabled: value.waveform_seek_enabled,
            visualization_db_range: value.visualization_db_range,
            spectrum_db_range: value.spectrum_db_range,
            spectrogram_db_range: value.spectrogram_db_range,
            reactive_db_range: value.reactive_db_range,
            spectrogram_history_seconds: value.spectrogram_history_seconds,
            waveform_cursor_color: value.waveform_cursor_color,
            waveform_glow: value.waveform_glow,
            waveform_rms_gain: value.waveform_rms_gain,
            waveform_peak_gain: value.waveform_peak_gain,
            waveform_peak_gamma: value.waveform_peak_gamma,
            oscilloscope_time_span_ms: value.oscilloscope_time_span_ms,
            oscilloscope_gain: value.oscilloscope_gain,
            oscilloscope_overlay_channels: value.oscilloscope_overlay_channels,
            media_read_buffer_mib: value.media_read_buffer_mib,
            ffmpeg_enabled: value.ffmpeg_enabled,
            pipewire_auto_mix: value.pipewire_auto_mix,
            equalizer: value.equalizer,
            denoise: value.denoise,
            neural: value.neural,
            equalizer_presets: value.equalizer_presets,
            equalizer_ab: value.equalizer_ab,
            processing_graph: value.processing_graph,
        }
    }
}

impl From<WireConfigPatch> for ConfigPatch {
    fn from(value: WireConfigPatch) -> Self {
        Self {
            library_roots: value.library_roots,
            scan_max_depth: value.scan_max_depth,
            scan_min_duration_seconds: value.scan_min_duration_seconds,
            output_device: value.output_device,
            volume: value.volume,
            shuffle: value.shuffle,
            repeat: value.repeat,
            play_count_threshold_percent: value.play_count_threshold_percent,
            tray_enabled: value.tray_enabled,
            confirm_queue_replace: value.confirm_queue_replace,
            mpris_enabled: value.mpris_enabled,
            log_level: value.log_level,
            log_to_file: value.log_to_file,
            log_max_files: value.log_max_files,
            query_batch_size: value.query_batch_size,
            library_columns: value
                .library_columns
                .map(|v| rivu_format::config::LibraryColumns {
                    artist: v[0],
                    album: v[1],
                    duration: v[2],
                    codec: v[3],
                    bitrate: v[4],
                }),
            ui_font: value.ui_font,
            ui_font_size_pt: value.ui_font_size_pt,
            ui_language: value.ui_language,
            ui_theme: value.ui_theme,
            material: value.material,
            ui_scale: value.ui_scale,
            layout_snap_enabled: value.layout_snap_enabled,
            layout_snap_grid_px: value.layout_snap_grid_px,
            visualization_enabled: value.visualization_enabled,
            visualization_signal: value.visualization_signal,
            analysis_rate_hz: value.analysis_rate_hz,
            visualization_fps: value.visualization_fps,
            visualization_background: value.visualization_background,
            visualization_palette: value.visualization_palette,
            spectrum_style: value.spectrum_style,
            analysis_fft_size: value.analysis_fft_size,
            analysis_window: value.analysis_window,
            spectrum_interpolate: value.spectrum_interpolate,
            spectrum_bar_layout: value.spectrum_bar_layout.map(Into::into),
            spectrum_gap: value.spectrum_gap,
            spectrum_peaks: value.spectrum_peaks,
            spectrum_peak_hold_ms: value.spectrum_peak_hold_ms,
            spectrum_peak_gravity: value.spectrum_peak_gravity,
            spectrum_bar_hold_ms: value.spectrum_bar_hold_ms,
            spectrum_bar_gravity: value.spectrum_bar_gravity,
            spectrum_smoothing_ms: value.spectrum_smoothing_ms,
            default_visualizer_style: value.default_visualizer_style,
            default_visualizer_reference_lines: value.default_visualizer_reference_lines,
            default_visualizer_labels: value.default_visualizer_labels,
            reactive_sensitivity: value.reactive_sensitivity,
            radial_rotation_speed: value.radial_rotation_speed,
            radial_bar_width: value.radial_bar_width,
            radial_ring_opacity: value.radial_ring_opacity,
            reactive_bloom_intensity: value.reactive_bloom_intensity,
            radial_inner_diameter: value.radial_inner_diameter,
            radial_fade_when_idle: value.radial_fade_when_idle,
            effect_primary_color: value.effect_primary_color,
            effect_secondary_color: value.effect_secondary_color,
            artwork_style: value.artwork_style,
            artwork_mode: value.artwork_mode,
            artwork_period_seconds: value.artwork_period_seconds,
            artwork_direction: value.artwork_direction,
            artwork_fps: value.artwork_fps,
            artwork_file_limit_mib: value.artwork_file_limit_mib,
            artwork_decode_budget_mib: value.artwork_decode_budget_mib,
            gpu_material_budget_mib: value.gpu_material_budget_mib,
            gpu_filter_budget_mib: value.gpu_filter_budget_mib,
            gpu_filter_max_depth: value.gpu_filter_max_depth,
            spectrogram_interpolate: value.spectrogram_interpolate,
            waveform_seek_enabled: value.waveform_seek_enabled,
            visualization_db_range: value.visualization_db_range,
            spectrum_db_range: value.spectrum_db_range,
            spectrogram_db_range: value.spectrogram_db_range,
            reactive_db_range: value.reactive_db_range,
            spectrogram_history_seconds: value.spectrogram_history_seconds,
            waveform_cursor_color: value.waveform_cursor_color,
            waveform_glow: value.waveform_glow,
            waveform_rms_gain: value.waveform_rms_gain,
            waveform_peak_gain: value.waveform_peak_gain,
            waveform_peak_gamma: value.waveform_peak_gamma,
            oscilloscope_time_span_ms: value.oscilloscope_time_span_ms,
            oscilloscope_gain: value.oscilloscope_gain,
            oscilloscope_overlay_channels: value.oscilloscope_overlay_channels,
            media_read_buffer_mib: value.media_read_buffer_mib,
            ffmpeg_enabled: value.ffmpeg_enabled,
            pipewire_auto_mix: value.pipewire_auto_mix,
            equalizer: value.equalizer,
            denoise: value.denoise,
            neural: value.neural,
            equalizer_presets: value.equalizer_presets,
            equalizer_ab: value.equalizer_ab,
            processing_graph: value.processing_graph,
        }
    }
}
