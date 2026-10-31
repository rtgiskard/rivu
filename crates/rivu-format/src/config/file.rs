//! Strict grouped representation used only by config.toml persistence.
use super::*;
use toml_edit::TableLike;

// Serialize only changed runtime fields, using the same key and serde rules as
// full exports. Optional fields serialize to an absent entry when cleared.
macro_rules! patch_field {
    ($table:expr, $baseline:expr, $config:expr, $(#[$attr:meta])* $key:ident => $field:ident : $ty:ty $(=> $mode:ident)?) => {
        if $baseline.$field != $config.$field {
            #[derive(Serialize)]
            struct Field<'a> {
                $(#[$attr])*
                $key: &'a $ty,
            }
            let before = toml_edit::ser::to_document(&Field { $key: &$baseline.$field })?;
            let mut after = toml_edit::ser::to_document(&Field { $key: &$config.$field })?;
            // Required structured payloads need every member on first write.
            // Existing fields still preserve unchanged values and decoration.
            let before = if patch_field!(@complete $($mode)?) && !$table.contains_key(stringify!($key)) {
                None
            } else {
                before.get(stringify!($key))
            };
            super::edit::patch($table, stringify!($key), before, after.remove(stringify!($key)))?;
        }
    };
    (@complete) => { false };
    (@complete complete) => { true };
}

// Each leaf declares its file key, runtime field and type once. Loading overlays
// supplied values on one Config::default(); saving borrows the runtime fields.
macro_rules! file_group {
    ($owned:ident, $view:ident { $( $(#[$attr:meta])* $key:ident => $field:ident : $ty:ty $(=> $mode:ident)?, )* } $(; $nested:ident : $nested_owned:ident, $nested_view:ident)*) => {
        #[derive(Default, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct $owned {
            $( $(#[$attr])* $key: Option<$ty>, )*
            $( $nested: $nested_owned, )*
        }

        impl $owned {
            fn apply(self, config: &mut Config) {
                $( if let Some(value) = self.$key { config.$field = value; } )*
                $( self.$nested.apply(config); )*
            }

            fn changed(baseline: &Config, config: &Config) -> bool {
                $( baseline.$field != config.$field || )*
                $( $nested_owned::changed(baseline, config) || )*
                false
            }

            fn patch(parent: &mut dyn TableLike, key: &str, baseline: &Config, config: &Config) -> Result<()> {
                if !Self::changed(baseline, config) {
                    return Ok(());
                }
                let table = super::edit::group(parent, key)?;
                $( patch_field!(table, baseline, config, $(#[$attr])* $key => $field: $ty $(=> $mode)?); )*
                $( $nested_owned::patch(table, stringify!($nested), baseline, config)?; )*
                Ok(())
            }
        }

        #[derive(Serialize)]
        struct $view<'a> {
            $( $(#[$attr])* $key: &'a $ty, )*
            $( $nested: $nested_view<'a>, )*
        }

        impl<'a> From<&'a Config> for $view<'a> {
            fn from(config: &'a Config) -> Self {
                Self { $( $key: &config.$field, )* $( $nested: config.into(), )* }
            }
        }
    };
}

file_group!(Library, LibraryView {
    roots => library_roots: Vec<PathBuf>,
    scan_max_depth => scan_max_depth: u32,
    scan_min_duration_seconds => scan_min_duration_seconds: u32,
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

file_group!(Playback, PlaybackView {
    #[serde(skip_serializing_if = "Option::is_none")]
    output_device => output_device: Option<String>,
    #[serde(deserialize_with = "deserialize_volume_percent", serialize_with = "serialize_volume_percent")]
    volume_percent => volume: f32,
    shuffle => shuffle: bool,
    repeat => repeat: RepeatMode,
    play_count_threshold_percent => play_count_threshold_percent: f64,
    media_read_buffer_mib => media_read_buffer_mib: u32,
    ffmpeg_enabled => ffmpeg_enabled: bool,
    pipewire_auto_mix => pipewire_auto_mix: bool,
});

file_group!(Interface, InterfaceView {
    language => ui_language: String,
    theme => ui_theme: Theme,
    tray_enabled => tray_enabled: bool,
    mpris_enabled => mpris_enabled: bool,
    confirm_queue_replace => confirm_queue_replace: bool,
    query_batch_size => query_batch_size: u32,
    artwork_file_limit_mib => artwork_file_limit_mib: u32,
    artwork_decode_budget_mib => artwork_decode_budget_mib: u32,
    gpu_material_budget_mib => gpu_material_budget_mib: u32,
    library_columns => library_columns: LibraryColumns,
    font => ui_font: String,
    font_size_pt => ui_font_size_pt: u32,
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
    bar_layout => spectrum_bar_layout: SpectrumBarLayout => complete,
    gap => spectrum_gap: f32,
    peaks => spectrum_peaks: bool,
    peak_hold_ms => spectrum_peak_hold_ms: u32,
    peak_gravity => spectrum_peak_gravity: f32,
    bar_hold_ms => spectrum_bar_hold_ms: u32,
    bar_gravity => spectrum_bar_gravity: f32,
    smoothing_ms => spectrum_smoothing_ms: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_range => spectrum_db_range: Option<f32>,
});

file_group!(Spectrogram, SpectrogramView {
    interpolate => spectrogram_interpolate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_range => spectrogram_db_range: Option<f32>,
    history_seconds => spectrogram_history_seconds: u32,
    startup_expansion_speed => spectrogram_startup_expansion_speed: f32,
    outflow_threshold_percent => spectrogram_outflow_threshold_percent: u32,
});

file_group!(Radial, RadialView {
    rotation_speed => radial_rotation_speed: f32,
    bar_width => radial_bar_width: f32,
    ring_opacity => radial_ring_opacity: f32,
    inner_diameter => radial_inner_diameter: f32,
    fade_when_idle => radial_fade_when_idle: bool,
});

file_group!(Reactive, ReactiveView {
    #[serde(skip_serializing_if = "Option::is_none")]
    db_range => reactive_db_range: Option<f32>,
    sensitivity => reactive_sensitivity: f32,
    bloom_intensity => reactive_bloom_intensity: f32,
});

file_group!(Waveform, WaveformView {
    seek_enabled => waveform_seek_enabled: bool,
    cursor_color => waveform_cursor_color: RgbColor,
    glow => waveform_glow: f32,
    rms_gain => waveform_rms_gain: f32,
    peak_gain => waveform_peak_gain: f32,
    peak_gamma => waveform_peak_gamma: f32,
});

file_group!(Oscilloscope, OscilloscopeView {
    time_span_ms => oscilloscope_time_span_ms: f32,
    gain => oscilloscope_gain: f32,
    overlay_channels => oscilloscope_overlay_channels: bool,
});

file_group!(AlbumArtwork, AlbumArtworkView {
    style => album_artwork_style: AlbumArtworkStyle,
    mode => album_artwork_mode: AlbumArtworkMode,
    period_seconds => album_artwork_period_seconds: f32,
    direction => album_artwork_direction: AlbumArtworkDirection,
    fps => album_artwork_fps: u16,
});

file_group!(General, GeneralView {
    enabled => visualization_enabled: bool,
    signal => visualization_signal: VisualizationSignal,
    analysis_rate_hz => analysis_rate_hz: u32,
    animation_fps => visualization_fps: u16,
    background => visualization_background: RgbColor,
    palette => visualization_palette: VisualizationPalette,
    analysis_fft_size => analysis_fft_size: u32,
    analysis_window => analysis_window: AnalysisWindow,
    db_range => visualization_db_range: f32,
});

file_group!(Defaults, DefaultsView {
    style => default_visualizer_style: AudioVisualizerStyle,
    reference_lines => default_visualizer_reference_lines: bool,
    labels => default_visualizer_labels: bool,
});

file_group!(Effects, EffectsView {
    primary_color => effect_primary_color: RgbColor,
    secondary_color => effect_secondary_color: RgbColor,
});

file_group!(Visualizations, VisualizationsView {
}; general: General, GeneralView
; defaults: Defaults, DefaultsView
; effects: Effects, EffectsView
; radial: Radial, RadialView
; reactive: Reactive, ReactiveView
; spectrum: Spectrum, SpectrumView
; spectrogram: Spectrogram, SpectrogramView
; waveform: Waveform, WaveformView
; oscilloscope: Oscilloscope, OscilloscopeView);

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct FileConfig {
    schema_version: Option<u32>,
    library: Library,
    playback: Playback,
    interface: Interface,
    logging: Logging,
    visualizations: Visualizations,
    equalizer: Option<EqualizerConfig>,
    denoise: Option<DenoiseConfig>,
    neural: Option<NeuralConfig>,
    processing_graph: Option<ProcessingGraph>,
    equalizer_presets: Option<Vec<EqualizerPreset>>,
    equalizer_ab: Option<EqualizerConfig>,
}

impl FileConfig {
    pub(super) fn into_config(self) -> Config {
        debug_assert_eq!(
            self.schema_version.unwrap_or(CONFIG_SCHEMA_VERSION),
            CONFIG_SCHEMA_VERSION
        );
        let mut config = Config::default();
        self.library.apply(&mut config);
        self.playback.apply(&mut config);
        self.interface.apply(&mut config);
        self.logging.apply(&mut config);
        if let Some(value) = self.equalizer {
            config.equalizer = value;
        }
        if let Some(value) = self.denoise {
            config.denoise = value;
        }
        if let Some(value) = self.neural {
            config.neural = value;
        }
        if let Some(value) = self.processing_graph {
            config.processing_graph = value;
        }
        if let Some(value) = self.equalizer_presets {
            config.equalizer_presets = value;
        }
        config.equalizer_ab = self.equalizer_ab;
        self.visualizations.apply(&mut config);
        config
    }

    pub(super) fn patch(
        table: &mut dyn TableLike,
        baseline: &Config,
        config: &Config,
    ) -> Result<()> {
        Library::patch(table, "library", baseline, config)?;
        Playback::patch(table, "playback", baseline, config)?;
        Interface::patch(table, "interface", baseline, config)?;
        Logging::patch(table, "logging", baseline, config)?;
        Visualizations::patch(table, "visualizations", baseline, config)?;
        patch_field!(table, baseline, config, equalizer => equalizer: EqualizerConfig);
        patch_field!(table, baseline, config, denoise => denoise: DenoiseConfig);
        patch_field!(table, baseline, config, neural => neural: NeuralConfig);
        patch_field!(table, baseline, config, processing_graph => processing_graph: ProcessingGraph);
        patch_field!(table, baseline, config, equalizer_presets => equalizer_presets: Vec<EqualizerPreset>);
        patch_field!(table, baseline, config, #[serde(skip_serializing_if = "Option::is_none")] equalizer_ab => equalizer_ab: Option<EqualizerConfig>);
        Ok(())
    }
}

#[derive(Serialize)]
pub(super) struct FileConfigView<'a> {
    schema_version: u32,
    library: LibraryView<'a>,
    playback: PlaybackView<'a>,
    interface: InterfaceView<'a>,
    logging: LoggingView<'a>,
    visualizations: VisualizationsView<'a>,
    equalizer: &'a EqualizerConfig,
    denoise: &'a DenoiseConfig,
    neural: &'a NeuralConfig,
    processing_graph: &'a ProcessingGraph,
    equalizer_presets: &'a Vec<EqualizerPreset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    equalizer_ab: &'a Option<EqualizerConfig>,
}

impl<'a> From<&'a Config> for FileConfigView<'a> {
    fn from(config: &'a Config) -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            library: config.into(),
            playback: config.into(),
            interface: config.into(),
            logging: config.into(),
            equalizer: &config.equalizer,
            denoise: &config.denoise,
            neural: &config.neural,
            processing_graph: &config.processing_graph,
            equalizer_presets: &config.equalizer_presets,
            equalizer_ab: &config.equalizer_ab,
            visualizations: config.into(),
        }
    }
}
