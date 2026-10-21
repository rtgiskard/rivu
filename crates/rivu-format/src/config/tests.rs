use super::*;

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
fn processing_tables_roundtrip_and_reject_invalid_controls() {
    let mut config = Config::default();
    config.equalizer.enabled = true;
    config
        .equalizer
        .bands
        .push(rivu_dsp::equalizer::EqBand::default());
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
