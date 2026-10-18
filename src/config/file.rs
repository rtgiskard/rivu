//! Strict grouped representation used only by config.toml persistence.
use super::*;

// Each leaf declares its file key, runtime field and type once. Loading overlays
// supplied values on one Config::default(); saving borrows the runtime fields.
macro_rules! file_group {
    ($owned:ident, $view:ident { $( $(#[$attr:meta])* $key:ident => $field:ident : $ty:ty, )* } $(; $nested:ident : $nested_owned:ident, $nested_view:ident)?) => {
        #[derive(Default, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct $owned {
            $( $(#[$attr])* $key: Option<$ty>, )*
            $( $nested: $nested_owned, )?
        }

        impl $owned {
            fn apply(self, config: &mut Config) {
                $( if let Some(value) = self.$key { config.$field = value; } )*
                $( self.$nested.apply(config); )?
            }
        }

        #[derive(Serialize)]
        struct $view<'a> {
            $( $(#[$attr])* $key: &'a $ty, )*
            $( $nested: $nested_view<'a>, )?
        }

        impl<'a> From<&'a Config> for $view<'a> {
            fn from(config: &'a Config) -> Self {
                Self { $( $key: &config.$field, )* $( $nested: config.into(), )? }
            }
        }
    };
}

file_group!(Library, LibraryView {
    roots => library_roots: Vec<PathBuf>,
    scan_max_depth => scan_max_depth: u32,
    scan_min_duration_secs => scan_min_duration_secs: u32,
});

fn deserialize_volume_percent<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<f32>, D::Error> {
    let percent = f64::deserialize(deserializer)?;
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return Err(serde::de::Error::custom(
            "playback.volume_percent must be finite and between 0 and 100 (inclusive)",
        ));
    }
    Ok(Some((percent / 100.0) as f32))
}

fn serialize_volume_percent<S: serde::Serializer>(
    volume: &f32,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_f64(f64::from(*volume) * 100.0)
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Playback {
    output_device: Option<Option<String>>,
    #[serde(deserialize_with = "deserialize_volume_percent")]
    volume_percent: Option<f32>,
    shuffle: Option<bool>,
    repeat: Option<RepeatMode>,
    play_count_threshold_percent: Option<f64>,
    media_read_buffer_mb: Option<u32>,
    ffmpeg_enabled: Option<bool>,
    pipewire_auto_mix: Option<bool>,
}

impl Playback {
    fn apply(self, config: &mut Config) {
        if let Some(value) = self.output_device {
            config.output_device = value;
        }
        if let Some(value) = self.volume_percent {
            config.volume = value;
        }
        if let Some(value) = self.shuffle {
            config.shuffle = value;
        }
        if let Some(value) = self.repeat {
            config.repeat = value;
        }
        if let Some(value) = self.play_count_threshold_percent {
            config.play_count_threshold_percent = value;
        }
        if let Some(value) = self.media_read_buffer_mb {
            config.media_read_buffer_mb = value;
        }
        if let Some(value) = self.ffmpeg_enabled {
            config.ffmpeg_enabled = value;
        }
        if let Some(value) = self.pipewire_auto_mix {
            config.pipewire_auto_mix = value;
        }
    }
}

#[derive(Serialize)]
struct PlaybackView<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    output_device: &'a Option<String>,
    #[serde(serialize_with = "serialize_volume_percent")]
    volume_percent: &'a f32,
    shuffle: &'a bool,
    repeat: &'a RepeatMode,
    play_count_threshold_percent: &'a f64,
    media_read_buffer_mb: &'a u32,
    ffmpeg_enabled: &'a bool,
    pipewire_auto_mix: &'a bool,
}

impl<'a> From<&'a Config> for PlaybackView<'a> {
    fn from(config: &'a Config) -> Self {
        Self {
            output_device: &config.output_device,
            volume_percent: &config.volume,
            shuffle: &config.shuffle,
            repeat: &config.repeat,
            play_count_threshold_percent: &config.play_count_threshold_percent,
            media_read_buffer_mb: &config.media_read_buffer_mb,
            ffmpeg_enabled: &config.ffmpeg_enabled,
            pipewire_auto_mix: &config.pipewire_auto_mix,
        }
    }
}

file_group!(Interface, InterfaceView {
    language => ui_language: String,
    theme => ui_theme: Theme,
    tray_enabled => tray_enabled: bool,
    mpris_enabled => mpris_enabled: bool,
    confirm_queue_replace => confirm_queue_replace: bool,
    page_size => page_size: u32,
    artwork_decode_budget_mb => artwork_decode_budget_mb: u32,
    gpu_material_budget_mb => gpu_material_budget_mb: u32,
    library_columns => library_columns: LibraryColumns,
    font => ui_font: String,
    font_size => ui_font_size: u32,
    scale => ui_scale: f32,
    layout_snap_enabled => layout_snap_enabled: bool,
    layout_snap_grid_px => layout_snap_grid_px: u32,
}; album_artwork: AlbumArtwork, AlbumArtworkView);

file_group!(Logging, LoggingView {
    level => log_level: LogLevel,
    to_file => log_to_file: bool,
    retention_weeks => log_retention_weeks: u32,
});

file_group!(Spectrum, SpectrumView {
    style => spectrum_style: SpectrumStyle,
    interpolate => spectrum_interpolate: bool,
    bar_width => spectrum_bar_width: f32,
    bars => spectrum_bars: u32,
    gap => spectrum_gap: f32,
    peaks => spectrum_peaks: bool,
    peak_hold_ms => spectrum_peak_hold_ms: u32,
    peak_gravity => spectrum_peak_gravity: f32,
    bar_hold_ms => spectrum_bar_hold_ms: u32,
    bar_gravity => spectrum_bar_gravity: f32,
    smoothing_ms => spectrum_smoothing_ms: u32,
    grid => spectrum_grid: bool,
    labels => spectrum_labels: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_range => spectrum_db_range: Option<f32>,
});

file_group!(Spectrogram, SpectrogramView {
    labels => spectrogram_labels: bool,
    interpolate => spectrogram_interpolate: bool,
    sampling_points_scale => spectrogram_sampling_points_scale: f32,
    interpolation_points => spectrogram_interpolation_points: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_range => spectrogram_db_range: Option<f32>,
    history_seconds => spectrogram_history_seconds: u32,
});

file_group!(AudioVisualizer, AudioVisualizerView {
    style => audio_visualizer_style: AudioVisualizerStyle,
    grid => audio_visualizer_grid: bool,
    labels => audio_visualizer_labels: bool,
    sensitivity => audio_visualizer_sensitivity: f32,
    rotation_speed => audio_visualizer_rotation_speed: f32,
    bar_width => audio_visualizer_bar_width: f32,
    bar_glow_layers => audio_visualizer_bar_glow_layers: u32,
    ring_opacity => audio_visualizer_ring_opacity: f32,
    bloom_intensity => audio_visualizer_bloom_intensity: f32,
    inner_diameter => audio_visualizer_inner_diameter: f32,
    fade_when_idle => audio_visualizer_fade_when_idle: bool,
    primary_color => audio_visualizer_primary_color: RgbColor,
    secondary_color => audio_visualizer_secondary_color: RgbColor,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_range => audio_visualizer_db_range: Option<f32>,
});

file_group!(Waveform, WaveformView {
    labels => waveform_labels: bool,
    seek_enabled => waveform_seek_enabled: bool,
    cursor_color => waveform_cursor_color: RgbColor,
    glow => waveform_glow: f32,
    rms_gain => waveform_rms_gain: f32,
    peak_gain => waveform_peak_gain: f32,
    peak_gamma => waveform_peak_gamma: f32,
});

file_group!(AlbumArtwork, AlbumArtworkView {
    style => album_artwork_style: AlbumArtworkStyle,
    mode => album_artwork_mode: AlbumArtworkMode,
    period_seconds => album_artwork_period_seconds: f32,
    direction => album_artwork_direction: AlbumArtworkDirection,
    fps => album_artwork_fps: u16,
});

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Visualizations {
    still_mode: Option<bool>,
    analysis_fps: Option<u32>,
    background: Option<RgbColor>,
    palette: Option<VisualizationPalette>,
    analysis_fft_size: Option<u32>,
    analysis_window: Option<AnalysisWindow>,
    db_range: Option<f32>,
    cache: Option<bool>,
    spectrum: Spectrum,
    spectrogram: Spectrogram,
    audio_visualizer: AudioVisualizer,
    waveform: Waveform,
}

#[derive(Serialize)]
struct VisualizationsView<'a> {
    still_mode: &'a bool,
    analysis_fps: &'a u32,
    background: &'a RgbColor,
    palette: &'a VisualizationPalette,
    analysis_fft_size: &'a u32,
    analysis_window: &'a AnalysisWindow,
    db_range: &'a f32,
    cache: &'a bool,
    spectrum: SpectrumView<'a>,
    spectrogram: SpectrogramView<'a>,
    audio_visualizer: AudioVisualizerView<'a>,
    waveform: WaveformView<'a>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct FileConfig {
    library: Library,
    playback: Playback,
    interface: Interface,
    logging: Logging,
    visualizations: Visualizations,
}

impl FileConfig {
    pub(super) fn into_config(self) -> Config {
        let mut config = Config::default();
        self.library.apply(&mut config);
        self.playback.apply(&mut config);
        self.interface.apply(&mut config);
        self.logging.apply(&mut config);
        if let Some(value) = self.visualizations.still_mode {
            config.still_mode = value;
        }
        if let Some(value) = self.visualizations.analysis_fps {
            config.analysis_fps = value;
        }
        if let Some(value) = self.visualizations.background {
            config.visual_background = value;
        }
        if let Some(value) = self.visualizations.palette {
            config.visual_palette = value;
        }
        if let Some(value) = self.visualizations.analysis_fft_size {
            config.analysis_fft_size = value;
        }
        if let Some(value) = self.visualizations.analysis_window {
            config.analysis_window = value;
        }
        if let Some(value) = self.visualizations.db_range {
            config.visualization_db_range = value;
        }
        if let Some(value) = self.visualizations.cache {
            config.visualization_cache = value;
        }
        self.visualizations.spectrum.apply(&mut config);
        self.visualizations.spectrogram.apply(&mut config);
        self.visualizations.audio_visualizer.apply(&mut config);
        self.visualizations.waveform.apply(&mut config);
        config
    }
}

#[derive(Serialize)]
pub(super) struct FileConfigView<'a> {
    library: LibraryView<'a>,
    playback: PlaybackView<'a>,
    interface: InterfaceView<'a>,
    logging: LoggingView<'a>,
    visualizations: VisualizationsView<'a>,
}

impl<'a> From<&'a Config> for FileConfigView<'a> {
    fn from(config: &'a Config) -> Self {
        Self {
            library: config.into(),
            playback: config.into(),
            interface: config.into(),
            logging: config.into(),
            visualizations: VisualizationsView {
                still_mode: &config.still_mode,
                analysis_fps: &config.analysis_fps,
                background: &config.visual_background,
                palette: &config.visual_palette,
                analysis_fft_size: &config.analysis_fft_size,
                analysis_window: &config.analysis_window,
                db_range: &config.visualization_db_range,
                cache: &config.visualization_cache,
                spectrum: config.into(),
                spectrogram: config.into(),
                audio_visualizer: config.into(),
                waveform: config.into(),
            },
        }
    }
}
