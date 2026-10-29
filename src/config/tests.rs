use super::*;
use std::{fs, path::Path};

fn write_current_config(path: &Path, contents: impl AsRef<str>) -> std::io::Result<()> {
    fs::write(
        path,
        format!(
            "schema_version = {CONFIG_SCHEMA_VERSION}\n{}",
            contents.as_ref()
        ),
    )
}

#[test]
fn missing_save_writes_only_overrides_and_clears_optional_keys() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut config = Config {
        volume: 0.25,
        output_device: Some("USB DAC".into()),
        ..Config::default()
    };
    config.save(&path).unwrap();
    let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["playback"].as_table().unwrap().len(), 2);
    assert!(saved.get("interface").is_none());
    assert!(saved.get("visualizations").is_none());
    assert!(saved.get("library").is_none());
    assert!(saved.get("logging").is_none());
    assert_eq!(Config::load(&path).unwrap(), config);

    let original = "# Keep explicit defaults and unrelated formatting\n[playback]\nvolume_percent=25 # chosen volume\noutput_device = 'USB DAC'\n[interface]\npage_size = 64 # explicit default\n";
    fs::write(&path, original).unwrap();
    config = Config::load(&path).unwrap();
    config.output_device = None;
    config.save(&path).unwrap();
    let saved = fs::read_to_string(&path).unwrap();
    assert_eq!(saved, original.replace("output_device = 'USB DAC'\n", ""));
    assert_eq!(Config::load(&path).unwrap(), config);

    fs::remove_file(&path).unwrap();
    Config::default().save(&path).unwrap();
    let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(saved.keys().all(|key| key == "schema_version"));
    assert_eq!(Config::load(&path).unwrap(), Config::default());
}

#[test]
fn invalid_existing_files_are_rejected_without_being_rewritten() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for contents in [
        "schema_version = 0\n",
        "schema_version = 2\n",
        "schema_version = '1'\n",
        "[interface]\ntheme = 'unknown'\n",
        "[interface]\npage_size = 0\n",
        "[playback]\nvolume_percent = 101\n",
        "volume = 0.25\n",
        "[interface",
    ] {
        fs::write(&path, contents).unwrap();
        assert!(Config::load(&path).is_err());
        assert!(Config::default().save(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }
}

#[test]
fn unversioned_current_files_load_without_being_rewritten() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for contents in ["", "# Keep this comment\n[interface]\n"] {
        fs::write(&path, contents).unwrap();
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        assert_eq!(fs::read(&path).unwrap(), contents.as_bytes());
    }
    let contents = "# Hand-edited settings\n[playback]\nvolume_percent = 25\nrepeat = 'one'\n[interface]\nlanguage = 'ZH_cn'\npage_size = 42\n";
    fs::write(&path, contents).unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.volume, 0.25);
    assert_eq!(config.repeat, RepeatMode::One);
    assert_eq!(config.ui_language, "zh-CN");
    assert_eq!(config.page_size, 42);
    assert_eq!(fs::read(&path).unwrap(), contents.as_bytes());
    config.save(&path).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    assert_eq!(Config::load(&path).unwrap(), config);
}

#[test]
fn artwork_file_limit_round_trips_and_rejects_out_of_range_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    write_current_config(&path, "[interface]\n").unwrap();
    assert_eq!(Config::load(&path).unwrap().artwork_file_limit_mb, 16);
    for limit in [1, 16, 32, 64] {
        write_current_config(
            &path,
            format!("[interface]\nartwork_file_limit_mb = {limit}\n"),
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.artwork_file_limit_mb, limit);
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }
    for limit in [0, 65, u32::MAX] {
        let config = Config {
            artwork_file_limit_mb: limit,
            ..Config::default()
        };
        assert!(config.validate().is_err());
        write_current_config(
            &path,
            format!("[interface]\nartwork_file_limit_mb = {limit}\n"),
        )
        .unwrap();
        assert!(Config::load(&path).is_err());
    }
}

#[test]
fn artwork_decode_budget_round_trips_and_rejects_out_of_range_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    write_current_config(&path, "[interface]\n").unwrap();
    assert_eq!(Config::load(&path).unwrap().artwork_decode_budget_mb, 64);
    for budget in [1, 64, 257, 1024] {
        write_current_config(
            &path,
            format!("[interface]\nartwork_decode_budget_mb = {budget}\n"),
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.artwork_decode_budget_mb, budget);
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }
    for budget in [0, 1025, u32::MAX] {
        let config = Config {
            artwork_decode_budget_mb: budget,
            ..Config::default()
        };
        assert!(config.validate().is_err());
        write_current_config(
            &path,
            format!("[interface]\nartwork_decode_budget_mb = {budget}\n"),
        )
        .unwrap();
        assert!(Config::load(&path).is_err());
    }
}

#[test]
fn artwork_decode_budget_patch_preserves_concurrent_unedited_fields() {
    let baseline = Config::default();
    let draft = Config {
        artwork_decode_budget_mb: 128,
        ..baseline.clone()
    };
    let mut latest = Config {
        volume: 0.25,
        album_artwork_fps: 30,
        artwork_file_limit_mb: 32,
        ..baseline.clone()
    };
    let expected = Config {
        artwork_decode_budget_mb: 128,
        ..latest.clone()
    };
    ConfigPatch::between(&baseline, &draft).apply_to(&mut latest);
    assert_eq!(latest, expected);

    latest.artwork_decode_budget_mb = 256;
    ConfigPatch::between(&baseline, &baseline).apply_to(&mut latest);
    assert_eq!(latest.artwork_decode_budget_mb, 256);
    assert_eq!(latest.artwork_file_limit_mb, 32);
}

#[test]
fn patch_sets_and_clears_optional_fields_without_overwriting_unedited_values() {
    let baseline = Config {
        output_device: Some("original device".to_owned()),
        ..Config::default()
    };
    let draft = Config {
        output_device: None,
        spectrum_db_range: Some(80.0),
        ..baseline.clone()
    };
    let mut latest = Config {
        output_device: Some("concurrent device".to_owned()),
        spectrum_db_range: Some(90.0),
        spectrogram_db_range: Some(100.0),
        volume: 0.25,
        ..baseline.clone()
    };
    let expected = Config {
        output_device: None,
        spectrum_db_range: Some(80.0),
        ..latest.clone()
    };
    ConfigPatch::between(&baseline, &draft).apply_to(&mut latest);
    assert_eq!(latest, expected);

    let expected = Config {
        output_device: baseline.output_device.clone(),
        spectrum_db_range: None,
        ..latest.clone()
    };
    ConfigPatch::between(&draft, &baseline).apply_to(&mut latest);
    assert_eq!(latest, expected);
}

#[test]
fn queue_replace_confirmation_defaults_and_round_trips() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    assert!(Config::load(&path).unwrap().confirm_queue_replace);
    write_current_config(&path, "[interface]\n").unwrap();
    assert!(Config::load(&path).unwrap().confirm_queue_replace);
    for enabled in [true, false] {
        write_current_config(
            &path,
            format!("[interface]\nconfirm_queue_replace = {enabled}\n"),
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.confirm_queue_replace, enabled);
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }
}

#[test]
fn interface_theme_names_round_trip_strictly() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    assert_eq!(Config::load(&path).unwrap().ui_theme, Theme::RivuDark);
    write_current_config(&path, "[interface]\nfont = 'sans-serif'\n").unwrap();
    assert_eq!(Config::load(&path).unwrap().ui_theme, Theme::RivuDark);
    let names = ["rivu_dark", "tokyo_night", "tokyo_night_day"];
    for (name, theme) in names.into_iter().zip(Theme::ALL) {
        write_current_config(&path, format!("[interface]\ntheme = '{name}'\n")).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.ui_theme, theme);
        config.save(&path).unwrap();
        let saved: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["interface"]["theme"].as_str(), Some(name));
        assert_eq!(Config::load(&path).unwrap(), config);
    }
    for invalid in [
        "[interface]\ntheme = 'unknown'\n",
        "[interface]\ntheme = 'TokyoNight'\n",
        "[interface]\nui_theme = 'rivu_dark'\n",
        "theme = 'rivu_dark'\n",
    ] {
        write_current_config(&path, invalid).unwrap();
        assert!(Config::load(&path).is_err(), "{invalid}");
    }
}

#[test]
fn theme_patch_preserves_unrelated_concurrent_changes() {
    let baseline = Config::default();
    let draft = Config {
        ui_theme: Theme::TokyoNightDay,
        ..baseline.clone()
    };
    let mut latest = Config {
        volume: 0.25,
        output_device: Some("concurrent output".into()),
        ui_language: "zh-CN".into(),
        still_mode: true,
        ..baseline.clone()
    };
    let expected = Config {
        ui_theme: Theme::TokyoNightDay,
        ..latest.clone()
    };
    ConfigPatch::between(&baseline, &draft).apply_to(&mut latest);
    assert_eq!(latest, expected);
}

#[test]
fn interface_language_normalizes_and_round_trips_without_resolving_auto() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    assert_eq!(Config::load(&path).unwrap().ui_language, "auto");
    for (input, expected) in [("AUTO", "auto"), ("en_us", "en-US"), ("ZH_cn", "zh-CN")] {
        write_current_config(&path, format!("[interface]\nlanguage = '{input}'\n")).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.ui_language, expected);
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }
    for value in ["", "fr-FR", "zh-TW", "zh-Hant"] {
        write_current_config(&path, format!("[interface]\nlanguage = '{value}'\n")).unwrap();
        let error = Config::load(&path).unwrap_err();
        assert!(format!("{error:#}").contains("interface.language"));
    }
}

#[test]
fn library_column_selections_preserve_partial_defaults_and_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for (contents, expected) in [
        ("", LibraryColumns::default()),
        (
            "[interface.library_columns]\nartist = true\n",
            LibraryColumns {
                artist: true,
                ..LibraryColumns::default()
            },
        ),
        (
            "[interface.library_columns]\nduration = false\n",
            LibraryColumns {
                duration: false,
                ..LibraryColumns::default()
            },
        ),
        (
            "[interface.library_columns]\nartist = false\nalbum = false\nduration = false\ncodec = false\nbitrate = false\n",
            LibraryColumns {
                artist: false,
                album: false,
                duration: false,
                codec: false,
                bitrate: false,
            },
        ),
        (
            "[interface.library_columns]\nartist = true\nalbum = true\nduration = false\ncodec = true\nbitrate = true\n",
            LibraryColumns {
                artist: true,
                album: true,
                duration: false,
                codec: true,
                bitrate: true,
            },
        ),
    ] {
        write_current_config(&path, contents).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.library_columns, expected);
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }
}

#[test]
fn album_artwork_settings_round_trip_and_reject_invalid_motion_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for (name, style) in [
        ("plain", AlbumArtworkStyle::Plain),
        ("vinyl", AlbumArtworkStyle::Vinyl),
        ("glass", AlbumArtworkStyle::Glass),
        ("halo", AlbumArtworkStyle::Halo),
        ("metal", AlbumArtworkStyle::Metal),
        ("card", AlbumArtworkStyle::Card),
        ("duotone", AlbumArtworkStyle::Duotone),
    ] {
        write_current_config(&path, format!("interface.album_artwork.style = '{name}'\n")).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.album_artwork_style, style);
        config.save(&path).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        let saved: toml::Value = toml::from_str(&saved).unwrap();
        assert_eq!(
            saved["interface"]["album_artwork"]["style"].as_str(),
            Some(name)
        );
        assert_eq!(Config::load(&path).unwrap(), config);
    }
    for mode in [AlbumArtworkMode::Static, AlbumArtworkMode::Rotating] {
        for direction in [
            AlbumArtworkDirection::Clockwise,
            AlbumArtworkDirection::Counterclockwise,
        ] {
            for period in [f32::MIN_POSITIVE, 0.125, 20.0, f32::MAX] {
                let config = Config {
                    album_artwork_mode: mode,
                    album_artwork_period_seconds: period,
                    album_artwork_direction: direction,
                    ..Config::default()
                };
                config.save(&path).unwrap();
                assert_eq!(Config::load(&path).unwrap(), config);
            }
        }
    }
    for period in ["0", "-1", "nan", "inf", "-inf"] {
        write_current_config(
            &path,
            format!("interface.album_artwork.period_seconds = {period}\n"),
        )
        .unwrap();
        let error = Config::load(&path).unwrap_err();
        assert!(format!("{error:#}").contains("interface.album_artwork.period_seconds"));
    }
    fs::remove_file(&path).unwrap();
    Config::default().save(&path).unwrap();
    let original = fs::read(&path).unwrap();
    for period in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let config = Config {
            album_artwork_period_seconds: period,
            ..Config::default()
        };
        assert!(config.save(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }
    for fps in [0, 1, 60, 240] {
        write_current_config(&path, format!("interface.album_artwork.fps = {fps}\n")).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.album_artwork_fps, fps);
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }
    Config::default().save(&path).unwrap();
    let original = fs::read(&path).unwrap();
    for fps in [241, u16::MAX] {
        let config = Config {
            album_artwork_fps: fps,
            ..Config::default()
        };
        assert!(config.validate().is_err());
        assert!(config.save(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }
    for invalid in [
        "style = 'unknown'",
        "mode = 'spinning'",
        "direction = 'left'",
        "unknown = true",
        "fps = -1",
        "fps = 241",
        "fps = 65536",
        "fps = 1.5",
        "fps = '60'",
    ] {
        write_current_config(&path, format!("[interface.album_artwork]\n{invalid}\n")).unwrap();
        assert!(Config::load(&path).is_err());
    }
}

#[test]
fn grouped_file_defaults_partial_tables_and_font_normalization() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for contents in [
        "",
        "[library]\n[playback]\n[interface]\n[logging]\n[visualizations]\n",
        "[visualizations.spectrum]\n[visualizations.spectrogram]\n[visualizations.audio_visualizer]\n[visualizations.waveform]\n",
    ] {
        write_current_config(&path, contents).unwrap();
        assert_eq!(Config::load(&path).unwrap(), Config::default());
    }
    write_current_config(&path, "[playback]\nshuffle = true\nrepeat = 'one'\n[interface]\nfont = ''\n[visualizations]\ndb_range = 95\n[visualizations.spectrum]\nbars = 42\n[visualizations.waveform]\nseek_enabled = false\n").unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(
        config,
        Config {
            shuffle: true,
            repeat: RepeatMode::One,
            visualization_db_range: 95.0,
            spectrum_bars: 42,
            waveform_seek_enabled: false,
            ..Config::default()
        }
    );
    assert_eq!(config.spectrum_db_range(), 95.0);
    assert_eq!(config.spectrogram_db_range(), 95.0);
    assert_eq!(config.audio_visualizer_db_range(), 95.0);
}

#[test]
fn spectrogram_flow_defaults_nested_round_trip_and_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    write_current_config(
        &path,
        "[visualizations.spectrogram]\nhistory_seconds = 30\n",
    )
    .unwrap();
    let old = Config::load(&path).unwrap();
    assert_eq!(old.spectrogram_flow_speed, 0.75);
    assert_eq!(old.spectrogram_accumulation, 75);
    assert_eq!(old.spectrogram_history_seconds, 30);

    for (speed, accumulation) in [(0.1, 10), (0.75, 75), (2.0, 95)] {
        write_current_config(
            &path,
            format!("[visualizations.spectrogram]\nflow_speed = {speed}\naccumulation = {accumulation}\n"),
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.spectrogram_flow_speed, speed);
        assert_eq!(config.spectrogram_accumulation, accumulation);
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        let saved = fs::read_to_string(&path).unwrap();
        let table: toml::Value = toml::from_str(&saved).unwrap();
        let nested = &table["visualizations"]["spectrogram"];
        let flow_speed = nested["flow_speed"]
            .as_float()
            .or_else(|| nested["flow_speed"].as_integer().map(|value| value as f64))
            .unwrap();
        assert_eq!(flow_speed as f32, speed);
        assert_eq!(
            nested["accumulation"].as_integer(),
            Some(i64::from(accumulation))
        );
        assert!(table.get("spectrogram_flow_speed").is_none());
        assert!(table.get("spectrogram_accumulation").is_none());

        for speed in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            0.0,
            0.099,
            2.001,
        ] {
            let invalid = Config {
                spectrogram_flow_speed: speed,
                ..config.clone()
            };
            let error = invalid.save(&path).unwrap_err();
            assert!(format!("{error:#}").contains("visualizations.spectrogram.flow_speed"));
            assert_eq!(fs::read_to_string(&path).unwrap(), saved);
        }
        for accumulation in [0, 9, 96, u32::MAX] {
            let invalid = Config {
                spectrogram_accumulation: accumulation,
                ..config.clone()
            };
            let error = invalid.save(&path).unwrap_err();
            assert!(format!("{error:#}").contains("visualizations.spectrogram.accumulation"));
            assert_eq!(fs::read_to_string(&path).unwrap(), saved);
        }
    }

    for invalid in [
        "flow_speed = nan",
        "flow_speed = inf",
        "flow_speed = -inf",
        "flow_speed = 0.099",
        "flow_speed = 2.001",
        "flow_speed = '0.75'",
        "accumulation = 9",
        "accumulation = 96",
        "accumulation = -1",
        "accumulation = 75.5",
        "accumulation = '75'",
    ] {
        write_current_config(&path, format!("[visualizations.spectrogram]\n{invalid}\n")).unwrap();
        assert!(Config::load(&path).is_err(), "accepted {invalid}");
    }
}

#[test]
fn spectrogram_flow_patch_preserves_unedited_and_concurrent_values() {
    let baseline = Config::default();
    let draft = Config {
        spectrogram_flow_speed: 1.5,
        ..baseline.clone()
    };
    let mut latest = Config {
        spectrogram_flow_speed: 0.5,
        spectrogram_accumulation: 90,
        spectrogram_history_seconds: 40,
        ..baseline.clone()
    };
    let expected = Config {
        spectrogram_flow_speed: 1.5,
        ..latest.clone()
    };
    ConfigPatch::between(&baseline, &draft).apply_to(&mut latest);
    assert_eq!(latest, expected);

    let accumulation_draft = Config {
        spectrogram_accumulation: 10,
        ..draft.clone()
    };
    latest.spectrogram_flow_speed = 2.0;
    ConfigPatch::between(&draft, &accumulation_draft).apply_to(&mut latest);
    assert_eq!(latest.spectrogram_flow_speed, 2.0);
    assert_eq!(latest.spectrogram_accumulation, 10);
    ConfigPatch::between(&accumulation_draft, &baseline).apply_to(&mut latest);
    assert_eq!(latest.spectrogram_flow_speed, 0.75);
    assert_eq!(latest.spectrogram_accumulation, 75);
    assert_eq!(latest.spectrogram_history_seconds, 40);
    ConfigPatch::between(&baseline, &baseline).apply_to(&mut latest);
    assert_eq!(latest.spectrogram_history_seconds, 40);
}

#[test]
fn volume_accepts_numeric_percent() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for (percent, level) in [
        ("0", 0.0),
        ("0.0", 0.0),
        ("1", 0.01),
        ("12.3456789", 0.123456789_f64 as f32),
        ("50", 0.5),
        ("100", 1.0),
        ("100.0", 1.0),
    ] {
        write_current_config(&path, format!("[playback]\nvolume_percent = {percent}\n")).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.volume, level);
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().volume, level);
    }
}

#[test]
fn volume_percent_round_trip_preserves_continuous_level_precision() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for volume in [
        0.0,
        f32::from_bits(1),
        f32::MIN_POSITIVE,
        0.12345679,
        f32::from_bits(0.5_f32.to_bits() + 1),
        f32::from_bits(1.0_f32.to_bits() - 1),
        1.0,
    ] {
        let config = Config {
            volume,
            ..Config::default()
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().volume, volume);
    }
}

#[test]
fn volume_file_rejects_invalid_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for value in [
        "-1",
        "-0.000001",
        "100.000001",
        "101",
        "nan",
        "inf",
        "-inf",
        "'50'",
        "true",
        "[]",
    ] {
        write_current_config(&path, format!("[playback]\nvolume_percent = {value}\n")).unwrap();
        assert!(
            Config::load(&path).is_err(),
            "accepted volume_percent = {value}"
        );
    }
}

#[test]
fn unknown_and_misplaced_file_keys_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for contents in [
        "unknown = true",
        "[unknown]",
        "[library]\nvolume_percent = 50",
        "[playback]\nscale = 1.0",
        "[interface]\nunknown = true",
        "[logging]\nunknown = true",
        "[visualizations]\nbars = 42",
        "[visualizations.spectrum]\nunknown = true",
        "[visualizations.spectrogram]\nunknown = true",
        "[visualizations.audio_visualizer]\nunknown = true",
        "[visualizations.waveform]\nunknown = true",
        "[visualizations.unknown]",
        "[visualizations.spectrum]\nvolume_percent = 50",
    ] {
        write_current_config(&path, contents).unwrap();
        assert!(Config::load(&path).is_err(), "accepted config: {contents}");
    }
}

#[test]
fn library_roots_reject_logical_overlap_before_loading_or_saving() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let valid = Config {
        library_roots: vec!["/Music".into(), "/Music2".into()],
        ..Config::default()
    };
    valid.save(&path).unwrap();
    assert_eq!(Config::load(&path).unwrap(), valid);
    let original = fs::read(&path).unwrap();
    for roots in [
        vec!["/Music", "/Music/Jazz"],
        vec!["/Music/Jazz", "/Music"],
        vec!["/Music", "/Music/unused/.."],
        vec!["/", "/Music"],
        vec!["Music", "Music/./Jazz"],
    ] {
        let invalid = Config {
            library_roots: roots.iter().map(PathBuf::from).collect(),
            ..Config::default()
        };
        assert!(invalid.save(&path).is_err(), "{roots:?}");
        assert_eq!(fs::read(&path).unwrap(), original);
        let text = format!("[library]\nroots = {roots:?}\n");
        let input = directory.path().join("invalid.toml");
        write_current_config(&input, text).unwrap();
        assert!(Config::load(&input).is_err(), "{roots:?}");
    }
}

fn entries(path: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    paths
}

#[test]
fn missing_file_returns_defaults_without_creating_directories() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("missing/config.toml");
    assert_eq!(Config::load(&path).unwrap(), Config::default());
    assert!(entries(directory.path()).is_empty());
    directory.close().unwrap();
}

#[test]
fn configuration_load_follows_regular_links_and_rejects_fifos_without_a_writer() {
    use std::{ffi::CString, os::unix::fs::symlink};

    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target.toml");
    let link = directory.path().join("config.toml");
    symlink("target.toml", &link).unwrap();
    assert_eq!(Config::load(&link).unwrap(), Config::default());
    assert!(!target.exists());
    fs::write(&target, "[playback]\nvolume_percent=37\n").unwrap();
    assert_eq!(Config::load(&link).unwrap().volume, 0.37);
    fs::remove_file(&target).unwrap();
    let name = CString::new(target.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: name is NUL terminated and mkfifo retains no pointer.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    for path in [&target, &link] {
        let error = Config::load(path).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(Config::default().save(path).is_err());
    }
    use std::os::unix::fs::FileTypeExt;
    assert!(fs::symlink_metadata(&target).unwrap().file_type().is_fifo());
}

#[test]
fn save_reload_replaces_file_and_leaves_no_temporary_files() {
    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path().join("settings");
    let path = parent.join("config.toml");
    Config::default().save(&path).unwrap();
    let config = Config {
        library_roots: vec![
            PathBuf::from("Music/Live recordings"),
            PathBuf::from("音楽"),
        ],
        scan_max_depth: 32,
        scan_min_duration_secs: 45,
        output_device: Some("USB DAC".to_owned()),
        volume: 0.25,
        shuffle: true,
        repeat: RepeatMode::All,
        play_count_threshold_percent: 35.5,
        tray_enabled: false,
        confirm_queue_replace: false,
        mpris_enabled: false,
        log_level: LogLevel::Debug,
        log_to_file: true,
        log_retention_weeks: 8,
        library_columns: LibraryColumns {
            artist: true,
            album: true,
            duration: false,
            codec: true,
            bitrate: true,
        },
        ui_font: "Noto Sans".to_owned(),
        ui_font_size: 18,
        ui_language: "zh-CN".to_owned(),
        ui_theme: Theme::TokyoNight,
        ui_scale: 1.5,
        layout_snap_enabled: false,
        layout_snap_grid_px: 12,
        still_mode: true,
        visualization_signal: crate::config::VisualizationSignal::Before,
        analysis_fps: 30,
        visualization_fps: 120,
        visualization_db_range: 90.0,
        spectrum_db_range: Some(80.0),
        audio_visualizer_db_range: Some(60.0),
        media_read_buffer_mb: 8,
        ffmpeg_enabled: true,
        pipewire_auto_mix: false,
        spectrum_style: SpectrumStyle::Solid,
        visual_palette: VisualizationPalette::TokyoNight,
        visual_background: "#101820".parse().unwrap(),
        analysis_fft_size: 16384,
        analysis_window: AnalysisWindow::Hann,
        spectrum_interpolate: false,
        spectrum_bar_width: 4.0,
        spectrum_bars: 48,
        spectrum_gap: 0.5,
        spectrum_peaks: false,
        spectrum_peak_hold_ms: 500,
        spectrum_peak_gravity: 30.0,
        spectrum_bar_hold_ms: 100,
        spectrum_bar_gravity: 25.0,
        spectrum_smoothing_ms: 120,
        spectrum_grid: false,
        spectrum_labels: false,
        audio_visualizer_style: AudioVisualizerStyle::Rings,
        audio_visualizer_grid: true,
        audio_visualizer_labels: true,
        audio_visualizer_sensitivity: 2.0,
        audio_visualizer_rotation_speed: 1.0,
        audio_visualizer_bar_width: 0.8,
        audio_visualizer_bar_glow_layers: 3,
        audio_visualizer_ring_opacity: 0.7,
        audio_visualizer_bloom_intensity: 1.0,
        audio_visualizer_inner_diameter: 0.8,
        audio_visualizer_fade_when_idle: true,
        audio_visualizer_primary_color: "#123456".parse().unwrap(),
        audio_visualizer_secondary_color: "#654321".parse().unwrap(),
        spectrogram_labels: false,
        spectrogram_interpolate: false,
        spectrogram_interpolation_points: 2048,
        spectrogram_sampling_points_scale: 1.0,
        spectrogram_db_range: Some(100.0),
        spectrogram_history_seconds: 30,
        spectrogram_flow_speed: 1.25,
        spectrogram_accumulation: 85,
        waveform_labels: false,
        waveform_seek_enabled: false,
        waveform_cursor_color: "#eeaa66".parse().unwrap(),
        waveform_glow: 0.0,
        waveform_rms_gain: 2.0,
        waveform_peak_gain: 0.5,
        waveform_peak_gamma: 1.25,
        album_artwork_style: AlbumArtworkStyle::Glass,
        album_artwork_mode: AlbumArtworkMode::Rotating,
        album_artwork_period_seconds: 12.5,
        album_artwork_direction: AlbumArtworkDirection::Counterclockwise,
        album_artwork_fps: 120,
        artwork_decode_budget_mb: 128,
        artwork_file_limit_mb: 32,
        gpu_material_budget_mb: 64,
        page_size: 32,
        equalizer: Default::default(),
        denoise: Default::default(),
        neural: Default::default(),
        equalizer_presets: Vec::new(),
        equalizer_ab: None,
        processing_graph: Default::default(),
    };
    config.save(&path).unwrap();
    assert_eq!(Config::load(&path).unwrap(), config);
    assert_eq!(entries(&parent), vec![path.clone()]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        assert_eq!(
            fs::metadata(&parent).unwrap().permissions().mode() & 0o077,
            0
        );
    }
    directory.close().unwrap();
}

#[test]
fn scan_max_depth_round_trips_and_rejects_zero() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");

    let config = Config {
        scan_max_depth: 123,
        ..Config::default()
    };
    config.save(&path).unwrap();
    assert_eq!(Config::load(&path).unwrap().scan_max_depth, 123);

    write_current_config(&path, "library.scan_max_depth = 0\n").unwrap();
    assert!(Config::load(&path).is_err());
    directory.close().unwrap();
}

#[test]
fn scan_min_duration_defaults_round_trips_and_rejects_non_u32_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    write_current_config(&path, "[library]\n").unwrap();
    assert_eq!(Config::load(&path).unwrap().scan_min_duration_secs, 0);

    for seconds in [0, 1, 45, u32::MAX] {
        let config = Config {
            scan_min_duration_secs: seconds,
            ..Config::default()
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().scan_min_duration_secs, seconds);
    }

    for value in ["-1", "1.5", "4294967296", "\"30\""] {
        write_current_config(&path, format!("library.scan_min_duration_secs = {value}\n")).unwrap();
        assert!(Config::load(&path).is_err(), "accepted {value}");
    }
    directory.close().unwrap();
}

#[test]
fn scan_min_duration_patch_preserves_concurrent_unedited_settings() {
    let baseline = Config::default();
    let draft = Config {
        scan_min_duration_secs: 30,
        ..baseline.clone()
    };
    let mut latest = Config {
        scan_max_depth: 7,
        ..baseline.clone()
    };
    ConfigPatch::between(&baseline, &draft).apply_to(&mut latest);
    assert_eq!(latest.scan_min_duration_secs, 30);
    assert_eq!(latest.scan_max_depth, 7);

    ConfigPatch::between(&draft, &baseline).apply_to(&mut latest);
    assert_eq!(latest.scan_min_duration_secs, 0);
    assert_eq!(latest.scan_max_depth, 7);
}

#[test]
fn base_font_size_defaults_round_trips_and_rejects_invalid_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");

    for size in [8, 11, 18] {
        let config = Config {
            ui_font_size: size,
            ui_scale: 1.5,
            ..Config::default()
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        let saved = fs::read_to_string(&path).unwrap();
        let document: toml::Value = toml::from_str(&saved).unwrap();
        assert_eq!(
            document["interface"]["font_size"].as_integer(),
            Some(i64::from(size))
        );
        for invalid_size in [0, 7, 19, u32::MAX] {
            let invalid = Config {
                ui_font_size: invalid_size,
                ..config.clone()
            };
            let error = invalid.save(&path).unwrap_err();
            assert!(format!("{error:#}").contains("interface.font_size"));
            assert_eq!(fs::read_to_string(&path).unwrap(), saved);
        }
    }

    for value in ["7", "19", "-1", "11.5", "'11'", "true", "4294967296"] {
        write_current_config(&path, format!("interface.font_size = {value}\n")).unwrap();
        assert!(Config::load(&path).is_err(), "accepted font size {value}");
    }
}

#[test]
fn base_font_size_patch_preserves_owner_changes() {
    let baseline = Config::default();
    let draft = Config {
        ui_font_size: 18,
        ..baseline.clone()
    };
    let mut latest = Config {
        ui_scale: 1.5,
        ui_font: "Noto Sans".to_owned(),
        volume: 0.25,
        ..baseline.clone()
    };
    let expected = Config {
        ui_font_size: 18,
        ..latest.clone()
    };
    ConfigPatch::between(&baseline, &draft).apply_to(&mut latest);
    assert_eq!(latest, expected);
    ConfigPatch::between(&draft, &baseline).apply_to(&mut latest);
    assert_eq!(latest.ui_font_size, baseline.ui_font_size);
    assert_eq!(latest.ui_scale, 1.5);
    assert_eq!(latest.ui_font, "Noto Sans");
    assert_eq!(latest.volume, 0.25);
}

#[test]
fn layout_snapping_round_trips_limits_and_rejects_invalid_values_without_overwriting() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for (enabled, spacing) in [(false, 1), (true, 64)] {
        let config = Config {
            layout_snap_enabled: enabled,
            layout_snap_grid_px: spacing,
            ..Config::default()
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        let saved = fs::read_to_string(&path).unwrap();
        for invalid_spacing in [0, 65, u32::MAX] {
            let invalid = Config {
                layout_snap_grid_px: invalid_spacing,
                ..config.clone()
            };
            let error = invalid.save(&path).unwrap_err();
            assert!(format!("{error:#}").contains("interface.layout_snap_grid_px"));
            assert_eq!(fs::read_to_string(&path).unwrap(), saved);
        }
    }

    for value in ["0", "65", "-1", "1.5", "'8'"] {
        write_current_config(&path, format!("interface.layout_snap_grid_px = {value}\n")).unwrap();
        assert!(Config::load(&path).is_err(), "accepted spacing {value}");
    }
    write_current_config(&path, "interface.layout_snap_enabled = 'true'\n").unwrap();
    assert!(Config::load(&path).is_err());
}

#[test]
fn malformed_unknown_and_invalid_values_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for contents in [
        "playback.volume_percent = [",
        "playback.volume_percent = 'loud'",
        "playback.volume_percent = -0.01",
        "playback.volume_percent = 100.01",
        "playback.volume_percent = nan",
        "playback.volume_percent = inf",
        "playback.volume_percent = -inf",
        "playback.play_count_threshold_percent = -0.01",
        "playback.play_count_threshold_percent = 100.0",
        "playback.play_count_threshold_percent = nan",
        "playback.play_count_threshold_percent = inf",
        "logging.retention_weeks = 0",
        "interface.scale = 0.74",
        "interface.scale = 2.01",
        "interface.scale = nan",
        "interface.scale = inf",
        "visualizations.analysis_fps = 4",
        "visualizations.analysis_fps = 61",
        "visualizations.analysis_fps = -1",
        "visualizations.analysis_fps = 5.5",
        "visualizations.fps = -1",
        "visualizations.fps = 241",
        "visualizations.fps = 65536",
        "visualizations.fps = 1.5",
        "visualizations.fps = '60'",
        "visualizations.spectrum.db_range = 0",
        "visualizations.spectrum.db_range = 161",
        "visualizations.spectrogram.db_range = inf",
        "visualizations.analysis_fft_size = 256",
        "visualizations.analysis_fft_size = 65536",
        "visualizations.analysis_fft_size = 1000",
        "visualizations.analysis_window = 'hamming'",
        "visualizations.signal = 'After'",
        "visualizations.signal = 'unknown'",
        "visualization_signal = 'before'",
        "visualizations.spectrum.signal = 'before'",
        "visualizations.spectrum.bar_width = 0.9",
        "visualizations.spectrum.bar_width = 21",
        "visualizations.spectrum.bar_width = nan",
        "visualizations.spectrum.bars = 513",
        "visualizations.spectrum.gap = -1",
        "visualizations.spectrum.gap = 9",
        "visualizations.spectrum.gap = nan",
        "visualizations.audio_visualizer.sensitivity = -1",
        "visualizations.audio_visualizer.sensitivity = 6",
        "visualizations.audio_visualizer.bar_glow_layers = 5",
        "visualizations.audio_visualizer.ring_opacity = 2",
        "visualizations.spectrum.peak_hold_ms = 2001",
        "visualizations.spectrum.peak_gravity = -1",
        "visualizations.spectrum.peak_gravity = 501",
        "visualizations.spectrum.peak_gravity = inf",
        "visualizations.spectrum.bar_hold_ms = 2001",
        "visualizations.spectrum.bar_gravity = -1",
        "visualizations.spectrum.bar_gravity = 501",
        "visualizations.spectrum.bar_gravity = nan",
        "visualizations.spectrum.smoothing_ms = 1001",
        "visualizations.spectrogram.history_seconds = 0",
        "visualizations.waveform.glow = -1",
        "visualizations.waveform.glow = 3",
        "visualizations.waveform.rms_gain = -1",
        "visualizations.waveform.rms_gain = 4",
        "visualizations.waveform.peak_gain = -1",
        "visualizations.waveform.peak_gain = 3",
        "visualizations.waveform.peak_gamma = 0.1",
        "visualizations.waveform.peak_gamma = 3.1",
        "visualizations.background = '#12345'",
        "visualizations.background = '#gg0000'",
        "visualizations.background = '#00000000'",
        "visualizations.waveform.cursor_color = 123456",
        "visualizations.spectrum.style = 'diagonal'",
        "playback.repeat = 'forever'",
        "interface.library_columns = false",
        "interface.library_columns.artist = 'true'",
        "interface.library_columns.album = 1",
        "interface.library_columns.duration = []",
        "interface.library_columns.codec = 'flac'",
        "interface.library_columns.bitrate = 320",
        "interface.library_columns.title = true",
        "library.library_columns.duration = false",
    ] {
        write_current_config(&path, contents).unwrap();
        assert!(
            Config::load(&path).is_err(),
            "accepted invalid config: {contents}"
        );
    }
}

#[test]
fn analysis_windows_and_fft_sizes_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for analysis_window in [
        AnalysisWindow::Hann,
        AnalysisWindow::BlackmanHarris,
        AnalysisWindow::None,
    ] {
        for analysis_fft_size in [512, 1024, 2048, 4096, 8192, 16384, 32768] {
            let config = Config {
                analysis_window,
                analysis_fft_size,
                ..Config::default()
            };
            config.save(&path).unwrap();
            assert_eq!(Config::load(&path).unwrap(), config);
        }
    }
}

#[test]
fn visualization_signal_defaults_and_round_trips_in_shared_group() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    write_current_config(&path, "[visualizations]\n").unwrap();
    assert_eq!(
        Config::load(&path).unwrap().visualization_signal,
        VisualizationSignal::After
    );
    for (visualization_signal, name) in [
        (VisualizationSignal::Before, "before"),
        (VisualizationSignal::After, "after"),
    ] {
        write_current_config(&path, format!("[visualizations]\nsignal = '{name}'\n")).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.visualization_signal, visualization_signal);
        config.save(&path).unwrap();
        let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["visualizations"]["signal"].as_str(), Some(name));
        assert_eq!(Config::load(&path).unwrap(), config);
    }
}

#[test]
fn visualization_signal_patch_preserves_unrelated_concurrent_changes() {
    let baseline = Config::default();
    let mut draft = baseline.clone();
    draft.visualization_signal = VisualizationSignal::Before;
    let patch = ConfigPatch::between(&baseline, &draft);
    let mut current = baseline.clone();
    current.analysis_fps = 30;
    patch.apply_to(&mut current);
    assert_eq!(current.visualization_signal, VisualizationSignal::Before);
    assert_eq!(current.analysis_fps, 30);

    let mut draft = baseline.clone();
    draft.analysis_fps = 40;
    ConfigPatch::between(&baseline, &draft).apply_to(&mut current);
    assert_eq!(current.visualization_signal, VisualizationSignal::Before);
    assert_eq!(current.analysis_fps, 40);
}

#[test]
fn visualization_ranges_inherit_and_preserve_explicit_overrides() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut config = Config {
        visualization_db_range: 85.0,
        ..Config::default()
    };
    assert_eq!(config.spectrum_db_range(), 85.0);
    assert_eq!(config.spectrogram_db_range(), 85.0);
    assert_eq!(config.audio_visualizer_db_range(), 85.0);
    config.save(&path).unwrap();
    let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    for view in ["spectrum", "spectrogram", "audio_visualizer"] {
        assert!(
            saved["visualizations"]
                .get(view)
                .and_then(|table| table.get("db_range"))
                .is_none()
        );
    }
    assert!(saved.get("playback").is_none());
    assert_eq!(Config::load(&path).unwrap(), config);

    write_current_config(&path, "visualizations.db_range = 85\nvisualizations.spectrum.db_range = 70\nvisualizations.spectrogram.db_range = 110\nvisualizations.audio_visualizer.db_range = 85\n")
    .unwrap();
    config = Config::load(&path).unwrap();
    assert_eq!(config.spectrum_db_range, Some(70.0));
    assert_eq!(config.spectrogram_db_range, Some(110.0));
    assert_eq!(config.audio_visualizer_db_range, Some(85.0));
    config.visualization_db_range = 95.0;
    assert_eq!(config.spectrum_db_range(), 70.0);
    assert_eq!(config.spectrogram_db_range(), 110.0);
    assert_eq!(config.audio_visualizer_db_range(), 85.0);
    config.save(&path).unwrap();
    assert_eq!(Config::load(&path).unwrap(), config);
    config.spectrum_db_range = None;
    assert_eq!(config.spectrum_db_range(), 95.0);
    config.save(&path).unwrap();
    assert_eq!(Config::load(&path).unwrap(), config);
    let saved: toml::Table = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        saved["visualizations"]
            .get("spectrum")
            .and_then(|table| table.get("db_range"))
            .is_none()
    );
    assert_eq!(
        saved["visualizations"]["spectrogram"]["db_range"].as_integer(),
        Some(110)
    );
}

#[test]
fn visualization_ranges_reject_nonfinite_and_out_of_range_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for key in [
        "visualizations.db_range",
        "visualizations.spectrum.db_range",
        "visualizations.spectrogram.db_range",
        "visualizations.audio_visualizer.db_range",
    ] {
        for value in ["nan", "inf", "-inf", "0", "0.99", "160.01", "161"] {
            write_current_config(&path, format!("{key} = {value}\n")).unwrap();
            assert!(Config::load(&path).is_err(), "accepted {key} = {value}");
        }
    }
}

#[test]
fn visual_parameter_boundaries_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for high in [false, true] {
        let config = Config {
            visualization_db_range: if high { 160.0 } else { 1.0 },
            spectrum_db_range: Some(if high { 160.0 } else { 1.0 }),
            spectrogram_db_range: Some(if high { 1.0 } else { 160.0 }),
            audio_visualizer_db_range: Some(if high { 160.0 } else { 1.0 }),
            spectrum_gap: if high { 8.0 } else { 0.0 },
            spectrum_peak_hold_ms: if high { 2000 } else { 0 },
            analysis_fft_size: if high { 32768 } else { 512 },
            spectrum_bar_width: if high { 20.0 } else { 1.0 },
            spectrum_bars: if high { 512 } else { 1 },
            spectrum_peak_gravity: if high { 500.0 } else { 0.0 },
            spectrum_bar_hold_ms: if high { 2000 } else { 0 },
            spectrum_bar_gravity: if high { 500.0 } else { 0.0 },
            spectrum_smoothing_ms: if high { 1000 } else { 0 },
            spectrogram_history_seconds: if high { 60 } else { 1 },
            spectrogram_interpolation_points: if high { 4096 } else { 64 },
            waveform_glow: if high { 2.0 } else { 0.0 },
            waveform_rms_gain: if high { 3.0 } else { 0.0 },
            waveform_peak_gain: if high { 2.0 } else { 0.0 },
            waveform_peak_gamma: if high { 3.0 } else { 0.25 },
            visual_background: if high {
                "#ffffff".parse().unwrap()
            } else {
                "#000000".parse().unwrap()
            },
            ..Config::default()
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }
}

#[test]
fn gpu_budget_and_history_limits_validate_and_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for budget in [16, 64, 1024] {
        let config = Config {
            gpu_material_budget_mb: budget,
            spectrogram_history_seconds: 60,
            ..Config::default()
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }
    for budget in [0, 15, 1025, u32::MAX] {
        assert!(
            Config {
                gpu_material_budget_mb: budget,
                ..Config::default()
            }
            .validate()
            .is_err()
        );
    }
    assert!(
        Config {
            spectrogram_history_seconds: 61,
            ..Config::default()
        }
        .validate()
        .is_err()
    );
}

#[test]
fn boundary_values_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for volume in [0.0, 1.0] {
        for ui_scale in [0.75, 2.0] {
            for analysis_fps in [5, 60] {
                let config = Config {
                    volume,
                    ui_scale,
                    analysis_fps,
                    ..Config::default()
                };
                config.save(&path).unwrap();
                assert_eq!(Config::load(&path).unwrap(), config);
            }
        }
    }
    directory.close().unwrap();
}

#[test]
fn play_count_threshold_boundaries_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for play_count_threshold_percent in [0.0, f64::from_bits(100.0_f64.to_bits() - 1)] {
        let config = Config {
            play_count_threshold_percent,
            ..Config::default()
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }
    directory.close().unwrap();
}

#[test]
fn custom_spectrum_range_round_trips() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for spectrum_style in [
        SpectrumStyle::Bars,
        SpectrumStyle::Outline,
        SpectrumStyle::Led,
        SpectrumStyle::Line,
        SpectrumStyle::Solid,
    ] {
        let config = Config {
            spectrum_style,
            spectrum_db_range: Some(55.0),
            ..Config::default()
        };
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
    }
}

#[cfg(unix)]
#[test]
fn saving_configuration_follows_links_and_resolves_each_save() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let links = directory.path().join("links");
    fs::create_dir(&links).unwrap();
    let first = directory.path().join("managed.toml");
    let second = directory.path().join("other.toml");
    let link = directory.path().join("config.toml");
    let intermediate = links.join("active.toml");
    Config::default().save(&first).unwrap();
    symlink(&first, &intermediate).unwrap();
    symlink("links/active.toml", &link).unwrap();

    let config = Config {
        volume: 0.2,
        ..Config::default()
    };
    config.save(&link).unwrap();
    assert_eq!(Config::load(&first).unwrap(), config);
    assert_eq!(
        fs::read_link(&link).unwrap(),
        Path::new("links/active.toml")
    );
    assert_eq!(fs::read_link(&intermediate).unwrap(), first);

    fs::remove_file(&intermediate).unwrap();
    symlink(&second, &intermediate).unwrap();
    let changed = Config {
        volume: 0.8,
        ..config.clone()
    };
    changed.save(&link).unwrap();
    assert_eq!(Config::load(&first).unwrap(), config);
    assert_eq!(Config::load(&second).unwrap(), changed);
    assert_eq!(fs::read_link(&intermediate).unwrap(), second);
    assert_eq!(entries(&links), vec![intermediate]);
}

#[cfg(unix)]
#[test]
fn saving_configuration_creates_a_dangling_link_target() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let link = directory.path().join("config.toml");
    let target = Path::new("managed/nested/config.toml");
    symlink(target, &link).unwrap();
    let config = Config {
        volume: 0.3,
        ..Config::default()
    };
    config.save(&link).unwrap();
    assert_eq!(fs::read_link(&link).unwrap(), target);
    assert_eq!(
        Config::load(&directory.path().join(target)).unwrap(),
        config
    );
    assert_eq!(
        entries(&directory.path().join("managed/nested")),
        vec![directory.path().join(target)]
    );
}

#[cfg(unix)]
#[test]
fn saving_configuration_preserves_cyclic_links_and_failed_targets() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.toml");
    let second = directory.path().join("second.toml");
    symlink("second.toml", &first).unwrap();
    symlink("first.toml", &second).unwrap();
    assert!(Config::default().save(&first).is_err());
    assert_eq!(fs::read_link(&first).unwrap(), Path::new("second.toml"));
    assert_eq!(fs::read_link(&second).unwrap(), Path::new("first.toml"));

    fs::remove_file(&second).unwrap();
    let target = directory.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep"), "unchanged").unwrap();
    symlink(&target, &second).unwrap();
    assert!(Config::default().save(&first).is_err());
    assert_eq!(fs::read_link(&first).unwrap(), Path::new("second.toml"));
    assert_eq!(fs::read_link(&second).unwrap(), target);
    assert_eq!(
        fs::read_to_string(target.join("keep")).unwrap(),
        "unchanged"
    );
    assert_eq!(entries(directory.path()), vec![first, second, target]);
}

#[test]
fn invalid_save_preserves_previous_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    Config::default().save(&path).unwrap();
    let original = fs::read(&path).unwrap();
    let invalid = Config {
        volume: f32::NAN,
        ..Config::default()
    };
    assert!(invalid.save(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(entries(directory.path()), vec![path]);
    directory.close().unwrap();
}

#[test]
fn non_regular_destination_preserves_contents_without_temporary_files() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::create_dir(&path).unwrap();
    let sentinel = path.join("keep");
    fs::write(&sentinel, "unchanged").unwrap();
    let _error = Config::default().save(&path).unwrap_err();
    assert_eq!(entries(directory.path()), vec![path]);
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "unchanged");
    directory.close().unwrap();
}

#[test]
fn unreadable_configuration_is_not_treated_as_missing() {
    let directory = tempfile::tempdir().unwrap();
    assert!(Config::load(directory.path()).is_err());
    assert!(Config::default().save(directory.path()).is_err());
    directory.close().unwrap();
}

#[test]
fn log_retention_weeks_must_be_bounded() {
    for log_retention_weeks in [0, 521] {
        let config = Config {
            log_retention_weeks,
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }
}

#[test]
fn page_size_round_trips_and_rejects_out_of_range_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for page_size in [20, 64, 256] {
        write_current_config(&path, format!("interface.page_size = {page_size}\n")).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.page_size, page_size);
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().page_size, page_size);
    }
    for page_size in [0, 19, 257, u32::MAX] {
        write_current_config(&path, format!("interface.page_size = {page_size}\n")).unwrap();
        assert!(Config::load(&path).is_err());
    }
}
