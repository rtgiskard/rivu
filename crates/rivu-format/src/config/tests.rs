use super::*;

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
            assert_eq!(file_error.to_string(), direct_error.to_string());
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
        error.to_string(),
        check_config_size((MAX_CONFIG_BYTES + 1) as u64)
            .unwrap_err()
            .to_string()
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
fn current_toml_contract_rejects_explicit_noncurrent_versions_and_invalid_values() {
    for input in [
        "schema_version = 0\n",
        "schema_version = 2\n",
        "schema_version = '1'\n",
        "schema_version = true\n",
        "schema_version = 1.0\n",
        "schema_version = -1\n",
        "schema_version = {}\n",
        "schema_version = 1\nunknown = true\n",
        "schema_version = 1\n[interface]\nui_theme = 'rivu_dark'\n",
        "schema_version = 1\n[playback]\nvolume_percent = 101\n",
        "schema_version = 1\n[library]\nroots = ['/Music', '/Music/Live']\n",
    ] {
        assert!(validate_toml(input).is_err(), "accepted {input}");
    }
    assert_eq!(
        from_toml("schema_version = 1\n").unwrap(),
        Config::default()
    );
}

#[test]
fn absent_version_uses_current_schema_and_serializes_explicitly() {
    for input in ["", "[interface]\n"] {
        assert_eq!(from_toml(input).unwrap(), Config::default());
    }
    let config = from_toml(
        "[interface]\nlanguage = 'ZH_cn'\npage_size = 42\n[playback]\nvolume_percent = 25\nrepeat = 'one'\n",
    )
    .unwrap();
    assert_eq!(config.ui_language, "zh-CN");
    assert_eq!(config.page_size, 42);
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
fn absent_version_keeps_strict_parse_and_validation_causes() {
    for input in [
        "unknown = true\n",
        "[interface]\nui_theme = 'rivu_dark'\n",
        "[playback]\nvolume_percent = 101\n",
        "[library]\nroots = ['/Music', '/Music/Live']\n",
    ] {
        let unversioned = from_toml(input).unwrap_err();
        let versioned = from_toml(&format!(
            "schema_version = {CONFIG_SCHEMA_VERSION}\n{input}"
        ))
        .unwrap_err();
        // Parse locations differ after adding the version line; compare the
        // structured TOML message, or the shared validation root cause.
        if let Some(cause) = unversioned.downcast_ref::<toml::de::Error>() {
            assert_eq!(
                cause.message(),
                versioned
                    .downcast_ref::<toml::de::Error>()
                    .unwrap()
                    .message()
            );
        } else {
            assert_eq!(
                unversioned.root_cause().to_string(),
                versioned.root_cause().to_string()
            );
        }
    }
}

#[test]
fn explicit_versions_gate_current_fields_and_syntax_errors_keep_the_parse_cause() {
    let error =
        from_toml("schema_version = 2\n[playback]\nvolume_percent = 'future'\n").unwrap_err();
    assert!(error.downcast_ref::<toml::de::Error>().is_none());
    for input in ["[interface", "schema_version = 1\n[interface"] {
        let error = from_toml(input).unwrap_err();
        assert!(error.downcast_ref::<toml::de::Error>().is_some());
    }
}

#[test]
fn grouped_round_trip_uses_shared_defaults_validation_and_normalization() {
    let text = "schema_version = 1\n[interface]\nlanguage = 'ZH_cn'\nfont = ''\n[playback]\nrepeat = 'one'\nvolume_percent = 25\n";
    let config = from_toml(text).unwrap();
    assert_eq!(config.ui_language, "zh-CN");
    assert_eq!(config.ui_font, Config::default().ui_font);
    assert_eq!(config.repeat, RepeatMode::One);
    assert_eq!(config.volume, 0.25);
    let saved = to_toml(&config).unwrap();
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert!(
        to_toml(&Config {
            volume: f32::NAN,
            ..config
        })
        .is_err()
    );
}

#[test]
fn visualization_animation_fps_validates_and_persists_boundaries() {
    for fps in [0, 1, 60, 240] {
        let config = from_toml(&format!("[visualizations]\nfps = {fps}\n")).unwrap();
        assert_eq!(config.visualization_fps, fps);
        let saved = to_toml(&config).unwrap();
        assert_eq!(from_toml(&saved).unwrap(), config);
        let patched = update_toml("", None, &config).unwrap();
        assert_eq!(from_toml(&patched).unwrap(), config);
    }
    for fps in [241, u16::MAX] {
        let config = Config {
            visualization_fps: fps,
            ..Config::default()
        };
        assert!(config.validate().is_err());
        assert!(to_toml(&config).is_err());
    }
    for value in ["-1", "241", "65536", "1.5", "'60'"] {
        assert!(from_toml(&format!("[visualizations]\nfps = {value}\n")).is_err());
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
    config.neural.speech_interpreter = "/models/df-env/bin/python".into();
    config.neural.separation_interpreter = "/models/unblend-env/bin/python".into();
    config.equalizer_presets.push(EqualizerPreset {
        name: "Voice".into(),
        config: config.equalizer.clone(),
    });
    config.equalizer_ab = Some(EqualizerConfig::default());
    let text = to_toml(&config).unwrap();
    assert_eq!(from_toml(&text).unwrap(), config);
    assert!(from_toml("[neural]\nthreads = 9\n").is_err());
    assert!(from_toml("[neural]\nspeech_wet_mix = 1.5\n").is_err());
    assert!(from_toml("[denoise]\nstrength = -1\n").is_err());
    assert!(from_toml("[equalizer]\nunknown = true\n").is_err());
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
            config: DenoiseConfig::default(),
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
    assert!(saved.contains("[processing_graph]"));
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
    let original = "# Hand maintained\n[playback]\nvolume_percent=70 # chosen volume\nshuffle = false # explicit default\n[interface]\nlanguage = 'ZH_cn'\ntheme = 'rivu_dark' # pinned\n";
    let baseline = from_toml(original).unwrap();
    let mut config = baseline.clone();
    config.volume = 0.25;
    let saved = update_toml(original, Some(&baseline), &config).unwrap();
    assert_eq!(
        saved,
        original.replace("volume_percent=70", "volume_percent=25.0")
    );
    assert_eq!(from_toml(&saved).unwrap(), config);
    assert_eq!(update_toml(&saved, None, &config).unwrap(), saved);
}

#[test]
fn new_configuration_only_materializes_changed_nested_fields() {
    let baseline = Config::default();
    let mut config = baseline.clone();
    config.neural.threads = 2;
    config.ui_scale = 1.25;
    let saved = update_toml("", None, &config).unwrap();
    let expected: toml::Value =
        toml::from_str("[interface]\nscale = 1.25\n[neural]\nthreads = 2\n").unwrap();
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
    let original = "visualizations = { spectrum = { labels = true }, cache = true }\n";
    let baseline = from_toml(original).unwrap();
    let mut config = baseline.clone();
    config.spectrum_labels = false;
    config.waveform_labels = !baseline.waveform_labels;
    let saved = update_toml(original, Some(&baseline), &config).unwrap();
    assert!(saved.contains("spectrum = { labels = false }"));
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
