use super::*;

#[test]
fn master_bypass_defaults_false_and_roundtrips_independently_of_eq() {
    let original = "# preserve\n[playback]\nvolume_percent = 45\n";
    let baseline = from_toml(original).unwrap();
    assert!(!baseline.processing_bypassed);
    let mut config = baseline.clone();
    config.processing_bypassed = true;
    config.equalizer.enabled = true;
    config.equalizer.preamp_db = -5.5;
    let saved = update_toml(original, Some(&baseline), &config).unwrap();
    assert!(saved.contains("# preserve"));
    assert!(saved.contains("processing_bypassed = true"));
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert_eq!(from_toml(&to_toml(&config).unwrap()).unwrap(), config);
    assert!(from_toml("[playback]\nprocessing_bypassed = 1").is_err());
    assert!(from_toml("[playback]\nprocessing_bypass = true").is_err());
}

#[test]
fn material_preferences_round_trip_defaults_and_independent_presets() {
    let original = "# Keep theme spelling\n[interface]\ntheme = 'rivu_dark'\n";
    let baseline = from_toml(original).unwrap();
    assert_eq!(baseline.material, MaterialPreferences::default());
    let mut config = baseline.clone();
    config.material.enabled = false;
    config.material.selected = MaterialPreset::StillWater;
    for preset in [MaterialPreset::Dissolve, MaterialPreset::StillWater] {
        let mut parameters = MaterialParameters::preset(preset);
        parameters.panel_alpha = 0.25;
        config.material.presets.insert(preset, parameters);
    }
    let saved = update_toml(original, Some(&baseline), &config).unwrap();
    assert!(saved.contains("# Keep theme spelling"));
    assert!(saved.contains("still_water"));
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert_eq!(from_toml(&to_toml(&config).unwrap()).unwrap(), config);
    let baseline = config.clone();
    MaterialChange::Reset(MaterialPreset::Dissolve).apply_to(&mut config.material);
    let saved = update_toml(&saved, Some(&baseline), &config).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert!(
        config
            .material
            .presets
            .contains_key(&MaterialPreset::StillWater)
    );
}

#[test]
fn legacy_original_material_stays_disabled_with_satin_ready_to_enable() {
    assert!(!MaterialPreferences::default().enabled);
    assert_eq!(
        MaterialPreferences::default().selected,
        MaterialPreset::Satin
    );
    for enabled in [false, true] {
        let mut config = Config::default();
        config.material.enabled = enabled;
        config.material.selected = MaterialPreset::Original;
        let mut satin = MaterialParameters::preset(MaterialPreset::Satin);
        satin.panel_alpha = 0.37;
        config.material.presets.insert(MaterialPreset::Satin, satin);
        let legacy = to_toml(&config).unwrap();
        let mut loaded = from_toml(&legacy).unwrap();
        loaded.material.normalize();
        assert!(!loaded.material.enabled);
        assert_eq!(loaded.material.selected, MaterialPreset::Satin);
        assert_eq!(loaded.material.presets, config.material.presets);
        MaterialChange::SetEnabled(true).apply_to(&mut loaded.material);
        assert!(loaded.material.enabled);
        assert_eq!(loaded.material.selected, MaterialPreset::Satin);
        let saved = update_toml(&legacy, None, &loaded).unwrap();
        assert_eq!(from_toml(&saved).unwrap(), loaded);
    }
}

#[test]
fn material_preferences_reject_unknown_ids_and_invalid_parameters() {
    assert!(from_toml("[interface.material]\nselected = 'unknown'\n").is_err());
    assert!(from_toml("[interface.material.presets.unknown]\npanel_alpha = 0.5\n").is_err());
    let mut config = Config::default();
    for (parameter, value) in [
        (material::MaterialParameter::Alpha, f32::NAN),
        (material::MaterialParameter::Wave, f32::INFINITY),
        (material::MaterialParameter::BackdropBlur, 33.),
        (material::MaterialParameter::Speed, -0.1),
        (material::MaterialParameter::Steps, 1.),
        (material::MaterialParameter::Steps, 2.5),
    ] {
        config.material = MaterialPreferences::default();
        MaterialChange::SetParameter {
            preset: MaterialPreset::StillWater,
            parameter,
            value,
        }
        .apply_to(&mut config.material);
        assert!(config.validate().is_err());
        if value.is_finite() {
            let invalid = toml::to_string_pretty(&file::FileConfigView::from(&config)).unwrap();
            assert!(from_toml(&invalid).is_err());
        }
    }
}

#[test]
fn common_palettes_roundtrip_and_patch() {
    let original = "[visualizations.general]\npalette = 'deadbeef'\n";
    let baseline = from_toml(original).unwrap();
    for (name, palette) in [
        ("inferno", VisualizationPalette::Inferno),
        ("magma", VisualizationPalette::Magma),
        ("viridis", VisualizationPalette::Viridis),
        ("cividis", VisualizationPalette::Cividis),
        ("grayscale", VisualizationPalette::Grayscale),
        ("deadbeef", VisualizationPalette::Deadbeef),
    ] {
        let config = from_toml(&format!("[visualizations.general]\npalette = '{name}'\n")).unwrap();
        assert_eq!(config.visualization_palette, palette);
        assert_eq!(from_toml(&to_toml(&config).unwrap()).unwrap(), config);
        let saved = update_toml(original, Some(&baseline), &config).unwrap();
        assert_eq!(from_toml(&saved).unwrap(), config);
    }
    for value in ["tokyo_night", "Inferno", "unknown", ""] {
        assert!(from_toml(&format!("[visualizations.general]\npalette = '{value}'\n")).is_err());
    }
    for value in ["inferno", "deadbeef", "tokyo_night"] {
        assert!(
            from_toml(&format!(
                "[visualizations.spectrogram]\npalette = '{value}'\n"
            ))
            .is_err()
        );
    }
}

#[test]
fn visualization_groups_round_trip_and_preserve_independent_ranges() {
    let original = "[interface]\nanimation_fps = 120\n[visualizations.general]\nenabled = false\nsignal = 'before'\nanalysis_rate_hz = 30\nbackground = '#123456'\npalette = 'viridis'\nanalysis_fft_size = 4096\nanalysis_window = 'hann'\ndb_range = 85.0\n[visualizations.radial]\nrotation_speed = 2.0\nbar_width = 1.0\nring_opacity = 0.5\ninner_diameter = 1.0\nfade_when_idle = true\n[visualizations.reactive]\nsensitivity = 2.0\nbloom_intensity = 1.0\ndb_range = 85.0\n[visualizations.spectrum]\ndb_range = 65.0\n[visualizations.spectrogram]\ndb_range = 105.0\n[visualizations.defaults]\nstyle = 'oscilloscope'\nreference_lines = true\nlabels = true\n[visualizations.effects]\nprimary_color = '#112233'\nsecondary_color = '#445566'\n";
    let mut config = from_toml(original).unwrap();
    assert!(!config.visualization_enabled);
    assert!(config.default_visualizer_reference_lines);
    assert!(config.default_visualizer_labels);
    assert_eq!(config.radial_rotation_speed, 2.0);
    assert_eq!(config.reactive_db_range, Some(85.0));
    assert_eq!(from_toml(&to_toml(&config).unwrap()).unwrap(), config);
    let baseline = config.clone();
    config.visualization_db_range = 95.0;
    assert_eq!(config.spectrum_db_range(), 65.0);
    assert_eq!(config.spectrogram_db_range(), 105.0);
    assert_eq!(config.reactive_db_range(), 85.0);
    let saved = update_toml(original, Some(&baseline), &config).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), config);
    let baseline = config.clone();
    config.reactive_db_range = None;
    let saved = update_toml(&saved, Some(&baseline), &config).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert_eq!(config.reactive_db_range(), 95.0);
    let table: toml::Value = toml::from_str(&saved).unwrap();
    assert!(
        table["visualizations"]["reactive"]
            .get("db_range")
            .is_none()
    );
}

#[test]
fn visualization_groups_reject_misplaced_and_unknown_keys() {
    for original in [
        "visualizations.db_range = 85.0",
        "visualizations.general.labels = true",
        "visualizations.general.rotation_speed = 1.0",
        "visualizations.radial.sensitivity = 1.0",
        "visualizations.reactive.style = 'oscilloscope'",
        "visualizations.general.unknown = true",
        "visualizations.radial.unknown = true",
        "visualizations.reactive.unknown = true",
    ] {
        assert!(from_toml(original).is_err(), "accepted {original}");
    }
}

#[test]
fn oscilloscope_style_roundtrips_global_defaults_and_incremental_edits() {
    let original = "[visualizations.general]\n[visualizations.oscilloscope]\ntime_span_ms = 25.0\ngain = 2.5\n[visualizations.defaults]\nstyle = 'oscilloscope'\n";
    let config = from_toml(original).unwrap();
    assert_eq!(
        config.default_visualizer_style,
        AudioVisualizerStyle::Oscilloscope
    );
    assert_eq!(config.oscilloscope_time_span_ms, 25.0);
    assert_eq!(config.oscilloscope_gain, 2.5);
    config.validate().unwrap();
    assert_eq!(from_toml(&to_toml(&config).unwrap()).unwrap(), config);
    let baseline = Config::default();
    let saved = update_toml("", Some(&baseline), &config).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), config);
}

#[test]
fn oscilloscope_configuration_rejects_nonfinite_and_out_of_range_values() {
    for (name, valid, invalid) in [
        (
            "time_span_ms",
            ["1.0", "50.0"],
            ["0.99", "50.01", "nan", "inf"],
        ),
        ("gain", ["0.1", "10.0"], ["0.09", "10.01", "nan", "inf"]),
    ] {
        for value in valid {
            let config = from_toml(&format!(
                "[visualizations.oscilloscope]\n{name} = {value}\n"
            ))
            .unwrap();
            assert_eq!(from_toml(&to_toml(&config).unwrap()).unwrap(), config);
        }
        for value in invalid {
            assert!(
                from_toml(&format!(
                    "[visualizations.oscilloscope]\n{name} = {value}\n"
                ))
                .is_err()
            );
        }
    }
}

#[test]
fn file_and_direct_configuration_inputs_share_the_byte_budget() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let link = directory.path().join("linked.toml");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    for size in [0, MAX_CONFIG_BYTES, MAX_CONFIG_BYTES + 1] {
        let text = " ".repeat(size);
        std::fs::write(&path, &text).unwrap();
        if size <= MAX_CONFIG_BYTES {
            assert_eq!(read_toml(&link).unwrap(), text);
            assert_eq!(from_toml(&text).unwrap(), Config::default());
        } else {
            let file_error = read_toml(&link).unwrap_err();
            let direct_error = from_toml(&text).unwrap_err();
            assert_eq!(file_error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(
                direct_error.downcast_ref::<io::Error>().unwrap().kind(),
                io::ErrorKind::InvalidData
            );
        }
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size as u64);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    std::fs::write(&path, [0xff]).unwrap();
    assert_eq!(
        read_toml(&path).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn serialized_configuration_enforces_the_exact_byte_budget() {
    let mut config = Config {
        output_device: Some(String::new()),
        ..Config::default()
    };
    let overhead = to_toml(&config).unwrap().len();
    for size in [MAX_CONFIG_BYTES - 1, MAX_CONFIG_BYTES, MAX_CONFIG_BYTES + 1] {
        config.output_device = Some("x".repeat(size - overhead));
        let result = to_toml(&config);
        if size <= MAX_CONFIG_BYTES {
            let saved = result.unwrap();
            assert_eq!(saved.len(), size);
            assert_eq!(from_toml(&saved).unwrap(), config);
        } else {
            let Err(error) = result else {
                panic!("oversized serialization was accepted");
            };
            assert_eq!(
                error.downcast_ref::<io::Error>().unwrap().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}

#[test]
fn compact_configuration_cannot_expand_past_the_output_budget() {
    let prefix = "[playback]\noutput_device = '";
    let suffix = "'\n";
    let input = format!(
        "{prefix}{}{suffix}",
        "x".repeat(MAX_CONFIG_BYTES - prefix.len() - suffix.len())
    );
    assert_eq!(input.len(), MAX_CONFIG_BYTES);
    let config = from_toml(&input).unwrap();
    let Err(error) = to_toml(&config) else {
        panic!("oversized serialization was accepted");
    };
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn escaped_configuration_cannot_expand_past_the_output_budget() {
    let config = Config {
        output_device: Some("\0".repeat(MAX_CONFIG_BYTES / 6 + 1)),
        ..Config::default()
    };
    config.validate().unwrap();
    assert!(config.output_device.as_ref().unwrap().len() < MAX_CONFIG_BYTES);
    let Err(error) = to_toml(&config) else {
        panic!("oversized serialization was accepted");
    };
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn validated_roots_preserve_order_and_reject_normalized_overlap() {
    let mut config = Config {
        library_roots: vec!["/Music/Z/../Z".into(), "/Music/A/./".into()],
        ..Config::default()
    };
    assert_eq!(
        config.validate_settings().unwrap(),
        vec![PathBuf::from("/Music/Z"), PathBuf::from("/Music/A")]
    );
    config
        .library_roots
        .push("/Music/Elsewhere/../Z/Live".into());
    assert!(config.validate_settings().is_err());
}

#[test]
fn current_toml_contract_rejects_explicit_noncurrent_versions_and_invalid_values() {
    for input in [
        "schema_version = 0\n",
        "schema_version = 3\n",
        "schema_version = 1\n",
        "schema_version = '1'\n",
        "schema_version = true\n",
        "schema_version = 1.0\n",
        "schema_version = -1\n",
        "schema_version = {}\n",
        "schema_version = 2\nunknown = true\n",
        "schema_version = 2\n[interface]\ngpu_filter_budget_mib = 15\n",
        "schema_version = 2\n[interface]\ngpu_filter_budget_mib = 1025\n",
        "schema_version = 2\n[interface]\ngpu_filter_max_depth = 0\n",
        "schema_version = 2\n[interface]\ngpu_filter_max_depth = 5\n",
        "schema_version = 2\n[playback]\nvolume_percent = 101\n",
        "schema_version = 2\n[library]\nroots = ['/Music', '/Music/Live']\n",
    ] {
        assert!(validate_toml(input).is_err(), "accepted {input}");
        assert!(
            update_toml(input, None, &Config::default()).is_err(),
            "accepted {input}"
        );
    }
    assert_eq!(
        from_toml("schema_version = 2\n").unwrap(),
        Config::default()
    );
}

#[test]
fn absent_version_uses_current_schema_and_serializes_explicitly() {
    for input in ["", "[interface]\n"] {
        assert_eq!(from_toml(input).unwrap(), Config::default());
    }
    let config = from_toml(
        "[interface]\nlanguage = 'ZH_cn'\nquery_batch_size = 42\n[playback]\nvolume_percent = 25\nrepeat = 'one'\n",
    )
    .unwrap();
    assert_eq!(config.ui_language, "zh-CN");
    assert_eq!(config.query_batch_size, 42);
    assert_eq!(config.volume, 0.25);
    assert_eq!(config.repeat, RepeatMode::One);
    let saved = to_toml(&config).unwrap();
    let document: toml::Value = toml::from_str(&saved).unwrap();
    assert_eq!(
        document["schema_version"].as_integer(),
        Some(i64::from(CONFIG_SCHEMA_VERSION))
    );
    assert_eq!(from_toml(&saved).unwrap(), config);
}

#[test]
fn explicit_versions_gate_current_fields_and_syntax_errors_keep_the_parse_cause() {
    let input = "schema_version = 3\n[playback]\nvolume_percent = 'future'\n";
    for error in [
        from_toml(input).unwrap_err(),
        update_toml(input, None, &Config::default()).unwrap_err(),
    ] {
        assert!(
            error
                .to_string()
                .contains("unsupported configuration schema_version 3")
        );
        assert!(error.downcast_ref::<toml_edit::de::Error>().is_none());
    }
    for input in ["[interface", "schema_version = 2\n[interface"] {
        for error in [
            from_toml(input).unwrap_err(),
            update_toml(input, None, &Config::default()).unwrap_err(),
        ] {
            assert!(error.downcast_ref::<toml_edit::TomlError>().is_some());
        }
    }
}

#[test]
fn grouped_round_trip_uses_shared_defaults_validation_and_normalization() {
    let text = "schema_version = 2\n[interface]\nlanguage = 'ZH_cn'\nfont = ''\n[playback]\nrepeat = 'one'\nvolume_percent = 25\n";
    let config = from_toml(text).unwrap();
    assert_eq!(config.ui_language, "zh-CN");
    assert_eq!(config.ui_font, Config::default().ui_font);
    assert_eq!(config.repeat, RepeatMode::One);
    assert_eq!(config.volume, 0.25);
    let saved = to_toml(&config).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), config);
    let empty_font = Config {
        ui_font: String::new(),
        ..config.clone()
    };
    assert!(empty_font.validate().is_err());
    assert!(to_toml(&empty_font).is_err());
    assert!(update_toml(text, Some(&config), &empty_font).is_err());
    for (font, valid) in [("字".repeat(42), true), ("字".repeat(43), false)] {
        let direct = Config {
            ui_font: font,
            ..config.clone()
        };
        assert_eq!(direct.validate().is_ok(), valid);
        assert_eq!(to_toml(&direct).is_ok(), valid);
        assert_eq!(update_toml(text, Some(&config), &direct).is_ok(), valid);
        if valid {
            assert_eq!(from_toml(&to_toml(&direct).unwrap()).unwrap(), direct);
        }
    }
    assert!(
        to_toml(&Config {
            volume: f32::NAN,
            ..config
        })
        .is_err()
    );
}

#[test]
fn animation_fps_validates_and_persists_boundaries() {
    assert_eq!(Config::default().animation_fps, 0);
    for fps in [0, 1, 60, 240] {
        let config = from_toml(&format!("[interface]\nanimation_fps = {fps}\n")).unwrap();
        assert_eq!(config.animation_fps, fps);
        let saved = to_toml(&config).unwrap();
        let document = saved.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(
            document["interface"]["animation_fps"].as_integer(),
            Some(i64::from(fps))
        );
        assert!(document["interface"]["artwork"].get("fps").is_none());
        assert!(
            document["visualizations"]["general"]
                .get("animation_fps")
                .is_none()
        );
        assert_eq!(from_toml(&saved).unwrap(), config);
        let patched = update_toml("", None, &config).unwrap();
        assert_eq!(from_toml(&patched).unwrap(), config);
    }
    for fps in [241, u16::MAX] {
        let config = Config {
            animation_fps: fps,
            ..Config::default()
        };
        assert!(config.validate().is_err());
        assert!(to_toml(&config).is_err());
    }
    for value in ["-1", "241", "65536", "1.5", "'60'"] {
        assert!(from_toml(&format!("[interface]\nanimation_fps = {value}\n")).is_err());
    }
    for old_key in [
        "interface.artwork.fps = 60",
        "visualizations.general.animation_fps = 60",
    ] {
        assert!(from_toml(old_key).is_err());
    }
}

#[test]
fn processing_tables_roundtrip_and_reject_invalid_controls() {
    let mut config = Config::default();
    config.equalizer.enabled = true;
    config
        .equalizer
        .bands
        .push(rivu_dsp::equalizer::config::EqBand::default());
    config.equalizer.bands[0].gain_db = 6.0;
    config.denoise.mode = rivu_dsp::denoise::DenoiseMode::Adaptive;
    config.denoise.spectral_gain_floor_db = -18.5;
    config.neural.speech_interpreter = "/models/df-env/bin/python".into();
    config.neural.separation_interpreter = "/models/unblend-env/bin/python".into();
    config.equalizer_presets.push(EqualizerPreset {
        name: "Voice".into(),
        config: config.equalizer.clone(),
    });
    config.equalizer_ab = Some(EqualizerConfig::default());
    let text = to_toml(&config).unwrap();
    assert_eq!(from_toml(&text).unwrap(), config);
    let input = "[denoise]\nspectral_gain_floor_db = -31.5\n";
    let mut edited = from_toml(input).unwrap();
    assert_eq!(edited.denoise.spectral_gain_floor_db, -31.5);
    let baseline = edited.clone();
    edited.denoise.spectral_gain_floor_db = -18.5;
    let saved = update_toml(input, Some(&baseline), &edited).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), edited);
    assert!(from_toml("[neural]\nthreads = 9\n").is_err());
    assert!(from_toml("[neural]\nspeech_wet_mix = 1.5\n").is_err());
    assert!(from_toml("[denoise]\nstrength = -1\n").is_err());
    assert!(from_toml("[equalizer]\nunknown = true\n").is_err());
}

#[test]
fn graph_patch_saves_edits_without_runtime_revision() {
    let original = "# graph settings\n[playback]\nvolume_percent = 25\n";
    let baseline = from_toml(original).unwrap();
    let mut config = baseline.clone();
    config
        .processing_graph
        .edit(
            0,
            processing_graph::GraphEdit::Move {
                id: 1,
                position: [800.0, 500.0],
            },
        )
        .unwrap();
    let saved = update_toml(original, Some(&baseline), &config).unwrap();
    let document: toml::Value = toml::from_str(&saved).unwrap();
    assert!(document["processing_graph"].get("revision").is_none());
    let loaded = from_toml(&saved).unwrap();
    assert_eq!(loaded.processing_graph.revision, 0);
    assert_eq!(
        loaded.processing_graph.node(1).unwrap().position,
        [800.0, 500.0]
    );
    assert_eq!(config.processing_graph.revision, 1);
    assert!(saved.starts_with("# graph settings\n"));
    assert_eq!(loaded.volume, 0.25);
}

#[test]
fn incremental_edits_validate_the_composed_document_not_only_the_requested_config() {
    let original = to_toml(&Config::default()).unwrap();
    let mut baseline = from_toml(&original).unwrap();
    // A valid saved baseline may differ from independently edited file contents.
    // Remap the baseline graph consistently, leaving the original file untouched.
    for node in &mut baseline.processing_graph.nodes {
        node.id += 100;
    }
    for edge in &mut baseline.processing_graph.edges {
        edge.from.node += 100;
        edge.to.node += 100;
    }
    baseline.processing_graph.next_id += 100;
    baseline.validate().unwrap();
    let mut config = baseline.clone();
    config.processing_graph.nodes[0].position = [800.0, 500.0];
    config.validate().unwrap();
    // Only nodes changed relative to baseline; retaining original edges/next_id
    // alongside those nodes would produce an invalid graph despite valid config.
    assert!(update_toml(&original, Some(&baseline), &config).is_err());
}

#[test]
fn processing_graph_roundtrip_preserves_node_local_settings_and_layout() {
    use processing_graph::{EqImplementation, GraphNode, GraphNodeKind, NoiseSelection};

    let mut config = Config::default();
    let neural = NeuralConfig {
        speech_interpreter: "/node-local/python".into(),
        speech_manifest: Some("/models/speech.toml".into()),
        separation_manifest: Some("/models/separate.toml".into()),
        speech_wet_mix: 0.25,
        ..Default::default()
    };
    let kinds = [
        GraphNodeKind::Equalizer {
            config: EqualizerConfig::default(),
            implementation: EqImplementation::LinearPhase { taps: 257 },
        },
        GraphNodeKind::Denoise {
            config: DenoiseConfig {
                spectral_gain_floor_db: -37.5,
                ..Default::default()
            },
            profile: Some(NoiseSelection {
                start_seconds: 0.5,
                end_seconds: 2.0,
            }),
        },
        GraphNodeKind::Speech {
            config: neural.clone(),
        },
        GraphNodeKind::Separate { config: neural },
        GraphNodeKind::Mix {
            gains_db: vec![-6.0, 3.0],
            muted: vec![false, true],
            solo: vec![true, false],
        },
    ];
    for kind in kinds {
        let id = config.processing_graph.next_id;
        config.processing_graph.next_id += 1;
        config.processing_graph.nodes.push(GraphNode {
            id,
            position: [id as f32 * 40.0, -25.0],
            kind,
        });
    }
    config.processing_graph.revision = 42;
    let saved = to_toml(&config).unwrap();
    let document: toml::Value = toml::from_str(&saved).unwrap();
    assert!(document["processing_graph"].get("revision").is_none());
    config.processing_graph.revision = 0;
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert_ne!(
        config.neural.speech_interpreter,
        PathBuf::from("/node-local/python")
    );
    for index in 2..config.processing_graph.nodes.len() {
        let mut document: toml::Value = toml::from_str(&saved).unwrap();
        let variant = document["processing_graph"]["nodes"][index]["kind"]
            .as_table_mut()
            .unwrap()
            .iter_mut()
            .next()
            .unwrap()
            .1
            .as_table_mut()
            .unwrap();
        variant.insert("unknown_setting".into(), toml::Value::Boolean(true));
        assert!(from_toml(&toml::to_string(&document).unwrap()).is_err());
    }
}

#[test]
fn processing_graph_defaults_and_validation_apply_at_persistence_boundary() {
    assert_eq!(
        from_toml("").unwrap().processing_graph,
        ProcessingGraph::default()
    );
    assert_eq!(
        from_toml("[processing_graph]\n").unwrap().processing_graph,
        ProcessingGraph::default()
    );
    assert!(from_toml("[processing_graph]\nunknown = true\n").is_err());
    assert!(from_toml("[processing_graph]\nnodes = []\n").is_err());
    let mut config = Config::default();
    config.processing_graph.edges[0].to.node = 999;
    assert!(config.validate().is_err());
    assert!(to_toml(&config).is_err());
}

#[test]
fn graph_neural_validation_is_node_local_and_shared_with_global_settings() {
    use processing_graph::{GraphNode, GraphNodeKind};

    let neural = NeuralConfig {
        threads: 0,
        ..Default::default()
    };
    assert!(neural.validate().is_err());
    let mut config = Config::default();
    config.processing_graph.nodes.push(GraphNode {
        id: 3,
        position: [0.0, 0.0],
        kind: GraphNodeKind::Speech {
            config: neural.clone(),
        },
    });
    config.processing_graph.next_id = 4;
    assert!(config.validate().is_err());
    assert!(to_toml(&config).is_err());
    config.processing_graph = ProcessingGraph::default();
    config.neural = neural;
    assert!(config.validate().is_err());
}

#[test]
fn changed_fields_preserve_unrelated_toml_and_explicit_defaults() {
    let original = "# Hand maintained\n[playback]\nvolume_percent=70 # chosen volume\nshuffle = false # explicit default\n[interface]\nlanguage = 'ZH_cn'\ntheme = 'rivu_dark' # pinned\ngpu_filter_budget_mib=128 # texture budget\ngpu_filter_max_depth=4 # nesting\n";
    let baseline = from_toml(original).unwrap();
    let mut config = baseline.clone();
    config.volume = 0.25;
    config.gpu_filter_budget_mib = 256;
    config.gpu_filter_max_depth = 2;
    let saved = update_toml(original, Some(&baseline), &config).unwrap();
    assert_eq!(
        saved,
        original
            .replace("volume_percent=70", "volume_percent=25.0")
            .replace("gpu_filter_budget_mib=128", "gpu_filter_budget_mib=256")
            .replace("gpu_filter_max_depth=4", "gpu_filter_max_depth=2")
    );
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert_eq!(update_toml(&saved, None, &config).unwrap(), saved);
}

#[test]
fn visualization_shared_groups_patch_preserves_format_and_independent_defaults() {
    let original = "# Shared controls\n[visualizations.defaults]\nstyle='spectrum' # new panels\nreference_lines=true\nlabels=false\n[visualizations.effects]\nprimary_color='#112233' # chosen color\nsecondary_color='#445566'\n[visualizations.reactive]\nsensitivity=2.0 # chosen gain\nbloom_intensity=1.0\n[visualizations.oscilloscope]\noverlay_channels=true\n";
    let baseline = from_toml(original).unwrap();
    let mut config = baseline.clone();
    config.reactive_sensitivity = 3.0;
    let saved = update_toml(original, Some(&baseline), &config).unwrap();
    assert_eq!(
        saved,
        original.replace("sensitivity=2.0", "sensitivity=3.0")
    );
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert_eq!(from_toml(&to_toml(&config).unwrap()).unwrap(), config);
    let baseline = config.clone();
    config.default_visualizer_labels = true;
    let saved = update_toml(&saved, Some(&baseline), &config).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert!(saved.contains("primary_color='#112233' # chosen color"));
    assert!(saved.contains("style='spectrum' # new panels"));
}

#[test]
fn new_configuration_only_materializes_changed_nested_fields() {
    let baseline = Config::default();
    let mut config = baseline.clone();
    config.neural.threads = 2;
    config.ui_scale = 1.25;
    config.gpu_filter_budget_mib = 256;
    config.gpu_filter_max_depth = 2;
    let saved = update_toml("", None, &config).unwrap();
    let expected: toml::Value = toml::from_str(
        "[interface]\nscale = 1.25\ngpu_filter_budget_mib = 256\ngpu_filter_max_depth = 2\n[neural]\nthreads = 2\n",
    )
    .unwrap();
    let actual: toml::Value = toml::from_str(&saved).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert_eq!(update_toml("", None, &baseline).unwrap(), "");
}

#[test]
fn incremental_edits_preserve_inline_and_dotted_tables() {
    for original in [
        "interface = { scale = 1.0, font = 'monospace' } # inline\n",
        "interface.scale = 1.0 # dotted\ninterface.font = 'monospace'\n",
    ] {
        let baseline = from_toml(original).unwrap();
        let mut config = baseline.clone();
        config.ui_scale = 1.25;
        let saved = update_toml(original, Some(&baseline), &config).unwrap();
        assert_eq!(saved, original.replace("1.0", "1.25"));
        assert_eq!(from_toml(&saved).unwrap(), config);
    }
    let original =
        "visualizations = { spectrum = { interpolate = true }, general = { enabled = false } }\n";
    let baseline = from_toml(original).unwrap();
    let mut config = baseline.clone();
    config.spectrum_interpolate = false;
    config.waveform_seek_enabled = !baseline.waveform_seek_enabled;
    let saved = update_toml(original, Some(&baseline), &config).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), config);
}

#[test]
fn clearing_optional_fields_removes_old_values_without_expanding_defaults() {
    let original = "# Keep header\n[playback]\noutput_device = 'old device'\n[neural]\nspeech_manifest = '/models/speech.json'\n[visualizations.spectrum]\ndb_range = 80.0\n";
    let baseline = from_toml(original).unwrap();
    let mut config = baseline.clone();
    config.output_device = None;
    config.neural.speech_manifest = None;
    config.spectrum_db_range = None;
    let saved = update_toml(original, Some(&baseline), &config).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert!(saved.contains("# Keep header"));
    for key in [
        "output_device",
        "speech_manifest",
        "db_range",
        "threads",
        "shuffle",
    ] {
        assert!(!saved.contains(key), "unexpected {key}: {saved}");
    }
}

#[test]
fn structured_arrays_replace_as_units_and_leave_other_fields_intact() {
    let original = "# My presets\n[interface]\nfont = 'monospace' # keep\n";
    let baseline = from_toml(original).unwrap();
    let mut config = baseline.clone();
    config.equalizer_presets.push(EqualizerPreset {
        name: "Custom".into(),
        config: EqualizerConfig::default(),
    });
    let saved = update_toml(original, Some(&baseline), &config).unwrap();
    assert!(saved.contains("font = 'monospace' # keep"));
    assert_eq!(from_toml(&saved).unwrap(), config);
    let mut changed = config.clone();
    changed.equalizer_presets[0].name = "Renamed".into();
    let saved = update_toml(&saved, Some(&config), &changed).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), changed);
    changed.equalizer_presets.clear();
    let cleared = update_toml(&saved, None, &changed).unwrap();
    assert_eq!(from_toml(&cleared).unwrap(), changed);
}

#[test]
fn inline_structured_arrays_remain_valid_when_updated_and_cleared() {
    let original = "equalizer_presets = [{ name = 'Custom', config = {} }] # presets\n";
    let baseline = from_toml(original).unwrap();
    let mut config = baseline.clone();
    config.equalizer_presets[0].name = "Renamed".into();
    let saved = update_toml(original, Some(&baseline), &config).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert!(saved.ends_with(" # presets\n"));
    let mut cleared = config.clone();
    cleared.equalizer_presets.clear();
    let saved = update_toml(&saved, Some(&config), &cleared).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), cleared);
    assert!(saved.ends_with(" # presets\n"));
}

#[test]
fn incremental_output_budget_includes_retained_comments() {
    let original = format!("#{}\n", "x".repeat(MAX_CONFIG_BYTES - 2));
    let baseline = from_toml(&original).unwrap();
    let mut config = baseline.clone();
    config.shuffle = true;
    let error = update_toml(&original, Some(&baseline), &config).unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::InvalidData
    );
    assert!(update_toml("[playback", Some(&baseline), &config).is_err());
}

#[test]
fn spectrum_layout_validates_each_active_payload() {
    for layout in [
        SpectrumBarLayout::Automatic { width: 1.0 },
        SpectrumBarLayout::Automatic { width: 20.0 },
        SpectrumBarLayout::Fixed { count: 1 },
        SpectrumBarLayout::Fixed { count: 512 },
    ] {
        let config = Config {
            spectrum_bar_layout: layout,
            ..Config::default()
        };
        config.validate().unwrap();
        assert_eq!(from_toml(&to_toml(&config).unwrap()).unwrap(), config);
    }
    for width in [0.0, 0.99, 20.01, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let config = Config {
            spectrum_bar_layout: SpectrumBarLayout::Automatic { width },
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }
    for count in [0, 513, u32::MAX] {
        let config = Config {
            spectrum_bar_layout: SpectrumBarLayout::Fixed { count },
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }
}

#[test]
fn spectrum_layout_toml_is_strict_and_mode_switch_removes_inactive_payload() {
    for body in [
        "mode = 'automatic'\nwidth = 3.0\ncount = 64",
        "mode = 'fixed'\ncount = 64\nwidth = 3.0",
        "mode = 'automatic'",
        "mode = 'fixed'",
        "mode = 'unknown'\nwidth = 3.0",
        "width = 3.0",
        "mode = 'fixed'\ncount = 0",
        "mode = 'automatic'\nwidth = nan",
    ] {
        assert!(
            from_toml(&format!("[visualizations.spectrum.bar_layout]\n{body}\n")).is_err(),
            "accepted {body}"
        );
    }
    let mut original =
        "[visualizations.spectrum.bar_layout]\nmode = 'automatic'\nwidth = 4.25 # chosen width\n"
            .to_owned();
    let mut config = from_toml(&original).unwrap();
    for layout in [
        SpectrumBarLayout::Fixed { count: 73 },
        SpectrumBarLayout::Automatic { width: 5.75 },
        SpectrumBarLayout::Fixed { count: 64 },
    ] {
        let baseline = config.clone();
        config.spectrum_bar_layout = layout;
        original = update_toml(&original, Some(&baseline), &config).unwrap();
        assert_eq!(from_toml(&original).unwrap(), config);
        let document: toml::Value = toml::from_str(&original).unwrap();
        let table = document["visualizations"]["spectrum"]["bar_layout"]
            .as_table()
            .unwrap();
        assert_eq!(table.len(), 2);
        match layout {
            SpectrumBarLayout::Automatic { width } => {
                assert_eq!(table["mode"].as_str(), Some("automatic"));
                assert_eq!(table["width"].as_float(), Some(f64::from(width)));
                assert!(!table.contains_key("count"));
            }
            SpectrumBarLayout::Fixed { count } => {
                assert_eq!(table["mode"].as_str(), Some("fixed"));
                assert_eq!(table["count"].as_integer(), Some(i64::from(count)));
                assert!(!table.contains_key("width"));
            }
        }
    }
}
