use super::*;

#[test]
fn configuration_only_publication_wakes_watchers_without_invalidating_runtime() {
    let (_directory, mut core) = fixture();
    core.publish();
    let (handle, _requests) = super::task_wait::wait_handle(&core);
    let updates = handle.subscribe_revisions();
    let initial = handle.state(StateSections::ALL);
    let mut snapshot = ClientSnapshot::default();
    initial.clone().apply_to(&mut snapshot);
    let before = initial.revisions;
    core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
        .unwrap();
    // Desired state does not leak before the owner's publication boundary.
    assert_eq!(handle.state_revisions(), before);
    assert!(updates.try_recv().is_err());
    core.publish();
    assert_eq!(updates.try_recv(), Ok(()));
    let changed = handle.state_revisions().changed_since(before);
    assert_eq!(
        changed,
        StateSections {
            configuration: true,
            ..StateSections::default()
        }
    );
    let response = handle.state(changed);
    assert_eq!(response.sections(), changed);
    assert!(response.system.is_none());
    response.apply_to(&mut snapshot);
    assert_ne!(
        snapshot.configuration.config.waveform_seek_enabled,
        initial.configuration.unwrap().config.waveform_seek_enabled
    );
    assert_eq!(snapshot.system.revision, initial.system.unwrap().revision);
    let projected = handle.gui_snapshot();
    assert!(Arc::ptr_eq(
        &projected.system.config,
        &snapshot.configuration.config
    ));
    core.publish();
    assert!(updates.try_recv().is_err());

    let before = handle.state_revisions();
    core.state.configuration.config_path = "/relocated/config.toml".into();
    core.publish();
    assert_eq!(updates.try_recv(), Ok(()));
    assert_eq!(handle.state_revisions().changed_since(before), changed);
    assert_eq!(
        handle.state(changed).configuration.unwrap().config_path,
        std::path::Path::new("/relocated/config.toml")
    );
}

#[test]
fn runtime_and_configuration_responses_derive_from_one_publication() {
    let (_directory, mut core) = fixture();
    core.publish();
    let (handle, _requests) = super::task_wait::wait_handle(&core);
    let initial = handle.state(StateSections::ALL);
    Arc::make_mut(&mut core.state.configuration.config).page_size = 91;
    Arc::make_mut(&mut core.state.configuration.config).ui_theme = crate::theme::Theme::TokyoNight;
    core.state.system.last_error = Some("new runtime".into());
    let unpublished = handle.state(StateSections::ALL);
    assert_eq!(unpublished.revisions, initial.revisions);
    assert_eq!(
        unpublished.configuration.unwrap().config.page_size,
        initial.configuration.as_ref().unwrap().config.page_size
    );
    assert_eq!(
        unpublished.system.unwrap().last_error,
        initial.system.unwrap().last_error
    );

    core.publish();
    let response = handle.state(StateSections {
        system: true,
        configuration: true,
        ..Default::default()
    });
    assert_eq!(response.revisions, handle.state_revisions());
    assert_eq!(
        response.revisions.changed_since(initial.revisions),
        StateSections {
            system: true,
            configuration: true,
            ..StateSections::default()
        }
    );
    assert_eq!(
        response.system.as_ref().unwrap().last_error.as_deref(),
        Some("new runtime")
    );
    assert_eq!(
        response.configuration.as_ref().unwrap().config.page_size,
        91
    );
    let mut snapshot = ClientSnapshot::default();
    response.apply_to(&mut snapshot);
    let tui = crate::projection::TuiSnapshot::from_client(&snapshot, None);
    assert_eq!(tui.system.page_size, 91);
    assert_eq!(tui.system.ui_theme, crate::theme::Theme::TokyoNight);
    assert_eq!(tui.system.last_error.as_deref(), Some("new runtime"));
}

#[test]
fn runtime_rejects_non_utf8_config_paths_before_creating_storage() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let directory = tempfile::tempdir().unwrap();
    let invalid = OsString::from_vec(b"invalid-\xff".to_vec());
    let explicit_config = directory.path().join(&invalid).with_extension("toml");
    let derived_data_dir = directory.path().join(&invalid);
    for (data_dir, config_path) in [
        (directory.path().join("explicit-data"), explicit_config),
        (
            derived_data_dir.clone(),
            derived_data_dir.join("config.toml"),
        ),
    ] {
        let error = CoreRuntime::start(&data_dir, &config_path).err().unwrap();
        assert!(
            error
                .to_string()
                .contains("Configuration path is not valid UTF-8")
        );
        assert!(!config_path.exists());
        assert!(!data_dir.exists());
    }
}

#[test]
fn runtime_validates_config_path_before_reading_or_rewriting_config() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join(OsString::from_vec(b"config-\xff.toml".to_vec()));
    let original = b"intentionally not valid TOML";
    std::fs::write(&path, original).unwrap();
    let error = CoreRuntime::start(directory.path(), &path).err().unwrap();
    assert!(
        error
            .to_string()
            .contains("Configuration path is not valid UTF-8")
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!directory.path().join("library.db").exists());
}

#[test]
fn startup_config_accepts_unicode_without_creating_a_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("配置.toml");
    assert_eq!(
        CoreRuntime::load_startup_config(&path).unwrap(),
        Config::default()
    );
    assert!(!path.exists());
}

#[test]
fn startup_and_unchanged_exit_preserve_config_bytes_and_comments() {
    use std::os::unix::fs::MetadataExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let original = format!(
        "# Keep user comments and formatting\nschema_version = 1\n[playback]\nvolume_percent = 75 # chosen volume\n[library]\nroots = [\"{}/unused/..\"]\n",
        directory.path().display(),
    ).into_bytes();
    std::fs::write(&path, &original).unwrap();
    let inode = std::fs::metadata(&path).unwrap().ino();
    let mut runtime = CoreRuntime::start(directory.path(), &path).unwrap();
    assert_eq!(
        runtime.handle.config_snapshot().library_roots,
        [directory.path().to_path_buf()]
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    runtime.control.request_shutdown().unwrap();
    runtime.join().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
}

#[test]
fn missing_config_is_created_only_at_apply_or_orderly_exit() {
    for apply in [false, true] {
        let (_directory, mut core) = fixture();
        let _audio = recording_audio(&mut core);
        let path = core.state.configuration.config_path.clone();
        assert!(!path.exists());
        core.command(Command::Volume { value: 0.25 }).unwrap();
        core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
            .unwrap();
        core.publish();
        assert!(!path.exists());
        if apply {
            let baseline = core.state.configuration.config.clone();
            core.update_gui_config(GuiConfigChange::Settings(Box::new(ConfigPatch::between(
                &baseline, &baseline,
            ))))
            .unwrap();
        } else {
            let (sender, receiver) = async_channel::bounded(1);
            drop(sender);
            core = run_until_disconnected(core, receiver);
        }
        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.volume, 0.25);
        assert_eq!(
            saved.waveform_seek_enabled,
            core.state.configuration.config.waveform_seek_enabled
        );
    }
}

#[test]
fn changed_apply_and_exit_preserve_comments_and_untouched_normalized_settings() {
    for apply in [false, true] {
        let (directory, mut core) = fixture();
        let _audio = recording_audio(&mut core);
        let path = core.state.configuration.config_path.clone();
        let original = format!(
            "# Hand-edited configuration\n[playback]\nvolume_percent = 75 # chosen volume\n[interface]\nlanguage = 'ZH_cn' # preserve spelling\npage_size=42\n[library]\nroots = ['{}/unused/..'] # preserve logical spelling\n",
            directory.path().display(),
        );
        std::fs::write(&path, &original).unwrap();
        let mut config = Config::load(&path).unwrap();
        config.library_roots = vec![directory.path().to_path_buf()];
        core.state.playback.volume = config.volume;
        core.state.playback.shuffle = config.shuffle;
        core.state.playback.repeat = config.repeat;
        core.state.system.selected_device = config.output_device.clone();
        core.saved_config = Some(config.clone());
        core.state.configuration.config = Arc::new(config);

        core.command(Command::Volume { value: 0.25 }).unwrap();
        if apply {
            let baseline = core.state.configuration.config.clone();
            core.update_gui_config(GuiConfigChange::Settings(Box::new(ConfigPatch::between(
                &baseline, &baseline,
            ))))
            .unwrap();
        } else {
            let (sender, receiver) = async_channel::bounded(1);
            drop(sender);
            core = run_until_disconnected(core, receiver);
        }
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# Hand-edited configuration\n"));
        assert!(saved.contains("# chosen volume\n"));
        assert!(saved.contains("language = 'ZH_cn' # preserve spelling\npage_size=42\n"));
        assert!(saved.contains(&format!(
            "roots = ['{}/unused/..'] # preserve logical spelling\n",
            directory.path().display(),
        )));
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.volume, 0.25);
        assert_eq!(loaded.page_size, 42);
        assert_eq!(loaded.ui_language, "zh-CN");
        let document: toml::Table = toml::from_str(&saved).unwrap();
        assert!(document.get("visualizations").is_none());
        assert!(document.get("logging").is_none());
        assert_eq!(core.saved_config.as_ref().unwrap().volume, 0.25);
    }
}

#[test]
fn changed_save_recreates_missing_file_from_defaults_not_saved_snapshot() {
    let (_directory, mut core) = fixture();
    let _audio = recording_audio(&mut core);
    let mut config = core.effective_config();
    config.page_size = 42;
    config.output_device = Some("retained-output".into());
    core.command(Command::Configure {
        config: Arc::new(config),
    })
    .unwrap();
    let path = core.state.configuration.config_path.clone();
    std::fs::remove_file(&path).unwrap();
    core.command(Command::Volume { value: 0.25 }).unwrap();
    assert!(core.flush_config().unwrap());
    let loaded = Config::load(&path).unwrap();
    assert_eq!(loaded, core.effective_config());
    assert_eq!(loaded.page_size, 42);
    assert_eq!(loaded.output_device.as_deref(), Some("retained-output"));
    assert_eq!(loaded.volume, 0.25);
    let saved: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(saved.get("logging").is_none());
    assert!(saved.get("visualizations").is_none());
}

#[test]
fn apply_clears_optional_device_without_rewriting_unrelated_settings() {
    let (_directory, mut core) = fixture();
    let _audio = recording_audio(&mut core);
    let mut config = core.effective_config();
    config.output_device = Some("USB DAC".into());
    core.command(Command::Configure {
        config: Arc::new(config),
    })
    .unwrap();
    let path = core.state.configuration.config_path.clone();
    let original = "# Keep these comments\n[playback]\noutput_device = 'USB DAC'\n[interface]\npage_size = 64 # explicit default\n";
    std::fs::write(&path, original).unwrap();
    let baseline = core.state.configuration.config.clone();
    let mut draft = baseline.as_ref().clone();
    draft.output_device = None;
    core.update_gui_config(GuiConfigChange::Settings(Box::new(ConfigPatch::between(
        &baseline, &draft,
    ))))
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        original.replace("output_device = 'USB DAC'\n", ""),
    );
    assert!(Config::load(&path).unwrap().output_device.is_none());
    assert!(core.state.system.selected_device.is_none());
    assert!(core.saved_config.as_ref().unwrap().output_device.is_none());
}

#[test]
fn changed_apply_and_exit_reject_invalid_existing_input_without_overwriting_it() {
    for apply in [false, true] {
        let (_directory, mut core) = fixture();
        let _audio = recording_audio(&mut core);
        core.flush_config().unwrap();
        let saved_config = core.saved_config.clone();
        let path = core.state.configuration.config_path.clone();
        let original = "# Preserve invalid user input\n[playback]\nvolume_percent = 101\n";
        std::fs::write(&path, original).unwrap();
        core.command(Command::Volume { value: 0.25 }).unwrap();
        if apply {
            let config = Arc::new(core.effective_config());
            assert!(core.command(Command::Configure { config }).is_err());
        } else {
            let (sender, receiver) = async_channel::bounded(1);
            drop(sender);
            core = run_until_disconnected(core, receiver);
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(core.saved_config, saved_config);
        assert!(core.state.system.last_error.is_some());
        assert_eq!(core.state.playback.volume, 0.25);
    }
}

#[test]
fn runtime_start_with_missing_config_waits_until_exit_to_create_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut runtime = CoreRuntime::start(directory.path(), &path).unwrap();
    assert!(!path.exists());
    runtime.control.request_shutdown().unwrap();
    runtime.join().unwrap();
    assert_eq!(Config::load(&path).unwrap(), Config::default());
    assert!(path.is_file());
}

#[test]
fn no_op_apply_and_change_back_exit_do_not_replace_saved_config() {
    use std::os::unix::fs::MetadataExt;

    let (_directory, mut core) = fixture();
    let audio = recording_audio(&mut core);
    core.flush_config().unwrap();
    let path = core.state.configuration.config_path.clone();
    let mut original = std::fs::read(&path).unwrap();
    original.extend_from_slice(b"\n# Preserve this comment on no-op saves\n");
    std::fs::write(&path, &original).unwrap();
    let inode = std::fs::metadata(&path).unwrap().ino();
    let baseline = core.state.configuration.config.clone();
    core.command(Command::Configure {
        config: baseline.clone(),
    })
    .unwrap();
    // Configure enqueues synchronously, but the recorder forwards on another thread.
    audio
        .recv_timeout(Duration::from_secs(2))
        .expect("No-op Apply still reapplies audio settings");
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    core.command(Command::Volume { value: 0.25 }).unwrap();
    core.command(Command::Shuffle {
        enabled: !baseline.shuffle,
    })
    .unwrap();
    core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
        .unwrap();
    core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(LibraryColumn::Artist))
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    core.command(Command::Volume {
        value: baseline.volume,
    })
    .unwrap();
    core.command(Command::Shuffle {
        enabled: baseline.shuffle,
    })
    .unwrap();
    core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
        .unwrap();
    core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(LibraryColumn::Artist))
        .unwrap();
    core.update_gui_config(GuiConfigChange::Settings(Box::new(ConfigPatch::between(
        &baseline, &baseline,
    ))))
    .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    let (sender, receiver) = async_channel::bounded(1);
    drop(sender);
    drop(run_until_disconnected(core, receiver));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
}

#[test]
fn queue_save_failure_does_not_prevent_final_config_save() {
    let (directory, mut core) = fixture_with_database(Some(10.0));
    let _audio = recording_audio(&mut core);
    core.command(Command::Volume { value: 0.25 }).unwrap();
    core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
        .unwrap();
    core.state.queue.current_id = Some(core.store.queue_id_at(0).unwrap().unwrap());
    let connection = rusqlite::Connection::open(directory.path().join("library.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER fail_final_queue_save BEFORE UPDATE ON rivu_meta
         BEGIN SELECT RAISE(FAIL, 'queue save blocked'); END;",
        )
        .unwrap();
    let (sender, receiver) = async_channel::bounded(1);
    drop(sender);
    core = run_until_disconnected(core, receiver);
    assert!(
        core.shared
            .read()
            .system
            .last_error
            .as_deref()
            .unwrap()
            .contains("queue save blocked")
    );
    let saved = Config::load(&core.state.configuration.config_path).unwrap();
    assert_eq!(saved.volume, 0.25);
    assert_eq!(
        saved.waveform_seek_enabled,
        core.state.configuration.config.waveform_seek_enabled
    );
}

#[test]
fn busy_audio_configuration_rejects_the_whole_batch_before_persistence() {
    for available in [0, 1, 5, 6, 7] {
        let (_directory, mut core) = fixture();
        core.state
            .configuration
            .config
            .save(&core.state.configuration.config_path)
            .unwrap();
        let baseline = core.state.clone();
        let saved = std::fs::read(&core.state.configuration.config_path).unwrap();
        let (commands, receive) = async_channel::bounded(64);
        for index in 0..64 - available {
            commands
                .try_send(AudioCommand::Volume(index as f32))
                .unwrap();
        }
        core.engine.replace_commands(commands);
        let mut config = core.state.configuration.config.as_ref().clone();
        config.volume = 0.25;
        config.shuffle = true;
        config.visualization_enabled = false;
        config.ffmpeg_enabled = false;
        config.output_device = Some("configured-output".into());
        let result = core.command(Command::Configure {
            config: Arc::new(config.clone()),
        });
        if available < 7 {
            assert!(result.unwrap_err().to_string().contains("not queued"));
            assert_eq!(
                *core.state.configuration.config,
                *baseline.configuration.config
            );
            assert_eq!(core.state.playback.volume, baseline.playback.volume);
            assert_eq!(core.state.playback.shuffle, baseline.playback.shuffle);
            assert_eq!(
                core.state.system.selected_device,
                baseline.system.selected_device
            );
            assert_eq!(core.output_settings_request, 0);
            assert_eq!(
                std::fs::read(&core.state.configuration.config_path).unwrap(),
                saved
            );
            assert_eq!(receive.len(), 64 - available);
            if available == 0 {
                assert!(
                    core.update_gui_config(GuiConfigChange::ToggleVisualization)
                        .is_err()
                );
                assert_eq!(
                    *core.state.configuration.config,
                    *baseline.configuration.config
                );
                assert_eq!(
                    std::fs::read(&core.state.configuration.config_path).unwrap(),
                    saved
                );
                assert_eq!(receive.len(), 64);
            }
        } else {
            result.unwrap();
            assert_eq!(*core.state.configuration.config, config);
            assert_eq!(
                Config::load(&core.state.configuration.config_path).unwrap(),
                config
            );
            assert_eq!(core.output_settings_request, 1);
            assert_eq!(receive.len(), 64);
        }
        for index in 0..64 - available {
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::Volume(value) if value == index as f32
            ));
        }
        if available == 7 {
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::VisualizationEnabled(false)
            ));
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::MediaReadBuffer(_)
            ));
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::Volume(0.25)
            ));
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::AnalysisSettings(_)
            ));
            assert!(
                matches!(receive.try_recv().unwrap(), AudioCommand::Equalizer(config) if *config == core.state.configuration.config.equalizer)
            );
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::FfmpegEnabled(false)
            ));
            assert!(matches!(
                receive.try_recv().unwrap(),
                AudioCommand::OutputSettings { request: 1, device: Some(device), .. }
                    if device == "configured-output"
            ));
        }
        assert!(receive.try_recv().is_err());
    }
}

#[test]
fn config_failure_retains_values_and_recovers_without_clearing_other_errors() {
    for unrelated in [false, true] {
        let (directory, mut core) = fixture();
        let path = core.state.configuration.config_path.clone();
        core.flush_config().unwrap();
        let backup = directory.path().join("config.good");
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        core.command(Command::Volume { value: 0.25 }).unwrap();
        assert!(core.flush_config().is_err());
        core.publish();
        assert!(core.shared.read().system.last_error.is_some());
        assert_eq!(core.state.playback.volume, 0.25);
        assert_ne!(core.state.configuration.config.volume, 0.25);
        assert_ne!(Config::load(&backup).unwrap().volume, 0.25);
        if unrelated {
            core.state.system.last_error = Some("Unrelated failure".into());
        } else {
            core.command(Command::DismissError).unwrap();
        }
        // Retrying the same failure must not replace another error or a dismissal.
        let expected = core.state.system.last_error.clone();
        assert!(core.flush_config().is_err());
        assert_eq!(core.state.system.last_error, expected);
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&backup, &path).unwrap();
        assert!(core.flush_config().unwrap());
        assert_eq!(Config::load(&path).unwrap().volume, 0.25);
        assert_eq!(core.state.system.last_error, expected);
        assert!(!core.flush_config().unwrap());
    }
}

#[test]
fn successful_config_retry_clears_only_its_own_failure() {
    let (_directory, mut core) = fixture();
    let path = core.state.configuration.config_path.clone();
    std::fs::create_dir(&path).unwrap();
    core.command(Command::Shuffle { enabled: true }).unwrap();
    assert!(core.flush_config().is_err());
    assert!(core.state.system.last_error.is_some());
    std::fs::remove_dir(&path).unwrap();
    assert!(core.flush_config().unwrap());
    assert!(core.state.system.last_error.is_none());
    assert!(Config::load(&path).unwrap().shuffle);
}

#[test]
fn shutdown_flush_reports_failure_and_retains_live_settings() {
    let (_directory, mut core) = fixture();
    let path = core.state.configuration.config_path.clone();
    std::fs::create_dir(&path).unwrap();
    core.command(Command::Repeat {
        mode: RepeatMode::All,
    })
    .unwrap();
    let (sender, receiver) = async_channel::bounded(1);
    drop(sender);
    core = run_until_disconnected(core, receiver);
    let state = core.shared.read();
    assert!(state.system.shutting_down);
    assert!(state.system.last_error.is_some());
    assert_eq!(state.playback.repeat, RepeatMode::All);
    assert_eq!(state.configuration.config.repeat, RepeatMode::Off);
}

#[test]
fn visualization_enabled_toggle_preserves_unsaved_playback_and_device_settings() {
    let (_directory, mut core) = fixture();
    core.command(Command::Volume { value: 0.25 }).unwrap();
    core.command(Command::Shuffle { enabled: true }).unwrap();
    core.command(Command::Repeat {
        mode: RepeatMode::All,
    })
    .unwrap();
    // A live output selection must survive unrelated quick changes.
    core.state.system.selected_device = Some("pending-output".into());
    for enabled in [false, true] {
        core.update_gui_config(GuiConfigChange::ToggleVisualization)
            .unwrap();
        assert_eq!(
            core.state.configuration.config.visualization_enabled,
            enabled
        );
        assert_eq!(core.state.playback.volume, 0.25);
        assert!(core.state.playback.shuffle);
        assert_eq!(core.state.playback.repeat, RepeatMode::All);
        assert_eq!(
            core.state.system.selected_device.as_deref(),
            Some("pending-output")
        );
        assert!(!core.state.configuration.config_path.exists());
    }
}

#[test]
fn library_column_toggles_merge_latest_config_without_audio_side_effects() {
    let (_directory, mut core) = fixture();
    core.command(Command::Volume { value: 0.25 }).unwrap();
    core.command(Command::Shuffle { enabled: true }).unwrap();
    core.command(Command::Repeat {
        mode: RepeatMode::All,
    })
    .unwrap();
    core.state.system.selected_device = Some("pending-output".into());
    let config = Arc::make_mut(&mut core.state.configuration.config);
    config.ui_scale = 1.25;
    config.waveform_seek_enabled = false;
    config.library_columns = crate::config::LibraryColumns {
        artist: false,
        album: false,
        duration: true,
        codec: false,
        bitrate: false,
    };
    let (commands, audio) = async_channel::bounded(1);
    core.engine.replace_commands(commands);
    let mut expected = core.state.configuration.config.as_ref().clone();
    expected.volume = 0.25;
    expected.shuffle = true;
    expected.repeat = RepeatMode::All;
    expected.output_device = Some("pending-output".into());
    for (column, columns) in [
        (
            LibraryColumn::Duration,
            crate::config::LibraryColumns {
                artist: false,
                album: false,
                duration: false,
                codec: false,
                bitrate: false,
            },
        ),
        (
            LibraryColumn::Artist,
            crate::config::LibraryColumns {
                artist: true,
                album: false,
                duration: false,
                codec: false,
                bitrate: false,
            },
        ),
        (
            LibraryColumn::Album,
            crate::config::LibraryColumns {
                artist: true,
                album: true,
                duration: false,
                codec: false,
                bitrate: false,
            },
        ),
        (
            LibraryColumn::Codec,
            crate::config::LibraryColumns {
                artist: true,
                album: true,
                duration: false,
                codec: true,
                bitrate: false,
            },
        ),
        (
            LibraryColumn::Bitrate,
            crate::config::LibraryColumns {
                artist: true,
                album: true,
                duration: false,
                codec: true,
                bitrate: true,
            },
        ),
        (
            LibraryColumn::Artist,
            crate::config::LibraryColumns {
                artist: false,
                album: true,
                duration: false,
                codec: true,
                bitrate: true,
            },
        ),
    ] {
        core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(column))
            .unwrap();
        expected.library_columns = columns;
        assert_eq!(*core.state.configuration.config, expected);
        assert!(!core.state.configuration.config_path.exists());
        assert_eq!(core.state.playback.volume, expected.volume);
        assert_eq!(core.state.playback.shuffle, expected.shuffle);
        assert_eq!(core.state.playback.repeat, expected.repeat);
        assert_eq!(core.state.system.selected_device, expected.output_device);
        core.publish();
        assert_eq!(*core.shared.read().configuration.config, expected);
        assert!(matches!(
            audio.try_recv(),
            Err(async_channel::TryRecvError::Empty)
        ));
    }
}

#[test]
fn library_column_toggle_commits_without_accessing_unwritable_config_path() {
    let (_directory, mut core) = fixture();
    let (commands, audio) = async_channel::bounded(1);
    core.engine.replace_commands(commands);
    Arc::make_mut(&mut core.state.configuration.config)
        .library_columns
        .duration = true;
    std::fs::create_dir(&core.state.configuration.config_path).unwrap();
    core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(
        LibraryColumn::Duration,
    ))
    .unwrap();
    core.publish();
    assert!(
        !core
            .shared
            .read()
            .configuration
            .config
            .library_columns
            .duration
    );
    assert!(core.state.system.last_error.is_none());
    assert!(matches!(
        audio.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
    assert!(core.state.configuration.config_path.is_dir());
}

#[test]
fn overlapping_root_changes_leave_published_and_saved_configuration_unchanged() {
    let (directory, mut core) = fixture();
    core.state
        .configuration
        .config
        .save(&core.state.configuration.config_path)
        .unwrap();
    let before = core.state.configuration.config.clone();
    let saved = std::fs::read(&core.state.configuration.config_path).unwrap();
    for root in [directory.path().join("child"), directory.path().join("..")] {
        assert!(
            core.update_gui_config(GuiConfigChange::AddLibraryRoot(root.clone()))
                .is_err()
        );
        let mut config = before.as_ref().clone();
        config.library_roots.push(root);
        assert!(
            core.command(Command::Configure {
                config: Arc::new(config)
            })
            .is_err()
        );
        assert_eq!(*core.state.configuration.config, *before);
        assert_eq!(
            std::fs::read(&core.state.configuration.config_path).unwrap(),
            saved
        );
    }
}

#[test]
fn desktop_config_edits_preserve_live_playback_and_merge_roots() {
    let (directory, mut core) = fixture();
    Arc::make_mut(&mut core.state.configuration.config).library_roots[0] =
        directory.path().join("existing");
    core.command(Command::Volume { value: 0.25 }).unwrap();
    core.command(Command::Shuffle { enabled: true }).unwrap();
    core.command(Command::Repeat {
        mode: RepeatMode::All,
    })
    .unwrap();
    core.state.system.selected_device = Some("pending-output".into());
    let root = directory.path().join("added");
    let seek = core.state.configuration.config.waveform_seek_enabled;
    core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
        .unwrap();
    core.update_gui_config(GuiConfigChange::AddLibraryRoot(root.clone()))
        .unwrap();
    core.update_gui_config(GuiConfigChange::AddLibraryRoot(root))
        .unwrap();
    assert_eq!(core.state.configuration.config.library_roots.len(), 2);
    assert_eq!(core.state.configuration.config.waveform_seek_enabled, !seek);
    assert_eq!(core.state.playback.volume, 0.25);
    assert!(core.state.playback.shuffle);
    assert_eq!(core.state.playback.repeat, RepeatMode::All);
    assert_eq!(
        core.state.system.selected_device.as_deref(),
        Some("pending-output")
    );
    assert!(!core.state.configuration.config_path.exists());
    assert!(core.flush_config().unwrap());
    let saved = Config::load(&core.state.configuration.config_path).unwrap();
    assert_eq!(saved.volume, 0.25);
    assert!(saved.shuffle);
    assert_eq!(saved.repeat, RepeatMode::All);
    assert_eq!(saved.output_device.as_deref(), Some("pending-output"));
    assert_eq!(saved.library_roots.len(), 2);
}

#[test]
fn settings_apply_merges_every_unedited_field_at_execution() {
    let (directory, mut core) = fixture();
    Arc::make_mut(&mut core.state.configuration.config).library_roots[0] =
        directory.path().join("existing");
    let _audio = recording_audio(&mut core);
    let baseline = core.state.configuration.config.clone();
    let mut draft = baseline.as_ref().clone();
    draft.scan_max_depth = 7;
    let patch = ConfigPatch::between(&baseline, &draft);
    core.command(Command::Volume { value: 0.2 }).unwrap();
    core.command(Command::Device {
        name: Some("live-output".into()),
    })
    .unwrap();
    core.command(Command::Shuffle { enabled: true }).unwrap();
    core.command(Command::Repeat {
        mode: RepeatMode::One,
    })
    .unwrap();
    let added = directory.path().join("added");
    core.update_gui_config(GuiConfigChange::AddLibraryRoot(added.clone()))
        .unwrap();
    core.update_gui_config(GuiConfigChange::ToggleVisualization)
        .unwrap();
    core.update_gui_config(GuiConfigChange::ToggleWaveformSeek)
        .unwrap();
    for column in LibraryColumn::ALL {
        core.update_gui_config(GuiConfigChange::ToggleLibraryColumn(column))
            .unwrap();
    }
    let mut expected = core.state.configuration.config.as_ref().clone();
    expected.scan_max_depth = 7;
    // These playback changes are still newer than the in-memory config snapshot.
    core.command(Command::Volume { value: 0.3 }).unwrap();
    core.command(Command::Device {
        name: Some("pending-output".into()),
    })
    .unwrap();
    expected.volume = 0.3;
    expected.output_device = Some("pending-output".into());
    assert!(!core.state.configuration.config_path.exists());
    core.update_gui_config(GuiConfigChange::Settings(Box::new(patch)))
        .unwrap();
    core.publish();
    let visible = GuiSnapshot::from_core(&core.shared.read());
    assert_eq!(*visible.system.config, expected);
    assert_eq!(visible.playback.volume, 0.3);
    assert!(visible.playback.shuffle);
    assert_eq!(visible.playback.repeat, RepeatMode::One);
    assert!(visible.system.config.library_roots.contains(&added));
    assert_eq!(
        Config::load(&core.state.configuration.config_path).unwrap(),
        expected
    );
}

#[test]
fn settings_apply_explicit_fields_override_newer_conflicting_values() {
    let (directory, mut core) = fixture();
    Arc::make_mut(&mut core.state.configuration.config).library_roots[0] =
        directory.path().join("existing");
    let _audio = recording_audio(&mut core);
    core.command(Command::Device {
        name: Some("original-output".into()),
    })
    .unwrap();
    core.flush_config().unwrap();
    let baseline = core.state.configuration.config.clone();
    let mut draft = baseline.as_ref().clone();
    draft.volume = 0.4;
    draft.output_device = None;
    draft.library_roots.clear();
    draft.visualization_enabled = false;
    let patch = ConfigPatch::between(&baseline, &draft);
    core.command(Command::Volume { value: 0.2 }).unwrap();
    core.command(Command::Device {
        name: Some("external-output".into()),
    })
    .unwrap();
    core.update_gui_config(GuiConfigChange::AddLibraryRoot(
        directory.path().join("external"),
    ))
    .unwrap();
    core.update_gui_config(GuiConfigChange::Settings(Box::new(patch)))
        .unwrap();
    core.publish();
    let visible = GuiSnapshot::from_core(&core.shared.read());
    assert_eq!(visible.playback.volume, 0.4);
    assert_eq!(visible.system.selected_device, None);
    assert!(visible.system.config.library_roots.is_empty());
    assert!(!visible.system.config.visualization_enabled);
    assert_eq!(
        Config::load(&core.state.configuration.config_path).unwrap(),
        *visible.system.config
    );
}

#[test]
fn settings_failed_save_retry_merges_against_latest_runtime() {
    let (_directory, mut core) = fixture();
    let (commands, audio) = async_channel::bounded(8);
    core.engine.replace_commands(commands);
    let baseline = core.state.configuration.config.clone();
    let mut draft = baseline.as_ref().clone();
    draft.scan_max_depth = 7;
    let patch = ConfigPatch::between(&baseline, &draft);
    std::fs::create_dir(&core.state.configuration.config_path).unwrap();
    assert!(
        core.update_gui_config(GuiConfigChange::Settings(Box::new(patch.clone())))
            .is_err()
    );
    assert_eq!(*core.state.configuration.config, *baseline);
    assert!(matches!(
        audio.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
    std::fs::remove_dir(&core.state.configuration.config_path).unwrap();
    core.command(Command::Volume { value: 0.2 }).unwrap();
    core.update_gui_config(GuiConfigChange::Settings(Box::new(patch)))
        .unwrap();
    core.publish();
    let visible = GuiSnapshot::from_core(&core.shared.read());
    assert_eq!(visible.playback.volume, 0.2);
    assert_eq!(visible.system.config.scan_max_depth, 7);
    assert_eq!(
        Config::load(&core.state.configuration.config_path).unwrap(),
        *visible.system.config
    );
}

#[test]
fn settings_saved_audio_failure_keeps_committed_desired_configuration() {
    let (_directory, mut core) = fixture();
    let (commands, receiver) = async_channel::bounded(64);
    drop(receiver);
    core.engine.replace_commands(commands);
    let baseline = core.state.configuration.config.clone();
    let mut draft = baseline.as_ref().clone();
    draft.scan_max_depth = 7;
    let error = core
        .update_gui_config(GuiConfigChange::Settings(Box::new(ConfigPatch::between(
            &baseline, &draft,
        ))))
        .unwrap_err();
    assert!(format!("{error:#}").contains("Configuration saved, but audio application failed"));
    core.publish();
    let visible = GuiSnapshot::from_core(&core.shared.read());
    assert_eq!(visible.system.config.scan_max_depth, 7);
    assert_eq!(
        Config::load(&core.state.configuration.config_path).unwrap(),
        *visible.system.config
    );
}

#[test]
fn configure_failed_save_keeps_desired_settings_and_does_not_deliver_audio() {
    let (_directory, mut core) = fixture();
    let (commands, audio) = async_channel::bounded(64);
    let _idle_commands = core.engine.replace_commands(commands);
    let previous = core.state.configuration.config.clone();
    std::fs::create_dir(&core.state.configuration.config_path).unwrap();
    let mut config = previous.as_ref().clone();
    config.volume = 0.25;
    config.shuffle = true;
    config.repeat = RepeatMode::All;
    config.visualization_enabled = false;
    config.ffmpeg_enabled = false;
    config.page_size = 32;
    config.output_device = Some("configured-output".into());
    Arc::make_mut(&mut core.state.system.devices).push("configured-output".into());
    core.played = vec![1, 2];
    core.played_cursor = 2;
    let playback = core.state.playback.clone();
    let device = core.state.system.selected_device.clone();
    let ffmpeg_status = core.state.system.ffmpeg_status.clone();

    core.command(Command::Configure {
        config: std::sync::Arc::new(config),
    })
    .unwrap_err();

    assert_eq!(*core.state.configuration.config, *previous);
    assert_eq!(core.state.playback.volume, playback.volume);
    assert_eq!(core.state.playback.shuffle, playback.shuffle);
    assert_eq!(core.state.playback.repeat, playback.repeat);
    assert_eq!(core.state.system.selected_device, device);
    assert_eq!(core.state.system.ffmpeg_status, ffmpeg_status);
    assert_eq!(core.played, [1, 2]);
    assert_eq!(core.played_cursor, 2);
    assert!(matches!(
        audio.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
}

#[test]
fn configure_rejected_preparation_keeps_saved_and_desired_settings() {
    let (_directory, mut core) = fixture();
    let (commands, audio) = async_channel::bounded(64);
    let _idle_commands = core.engine.replace_commands(commands);
    let previous = core.state.configuration.config.clone();
    previous
        .save(&core.state.configuration.config_path)
        .unwrap();
    let mut config = previous.as_ref().clone();
    config.volume = 0.25;
    config.visualization_enabled = false;
    config.page_size = 0;

    core.command(Command::Configure {
        config: std::sync::Arc::new(config),
    })
    .unwrap_err();

    assert_eq!(*core.state.configuration.config, *previous);
    assert_eq!(core.state.playback.volume, previous.volume);
    assert_eq!(
        Config::load(&core.state.configuration.config_path).unwrap(),
        *previous
    );
    assert!(matches!(
        audio.try_recv(),
        Err(async_channel::TryRecvError::Empty)
    ));
}

#[test]
fn output_settings_errors_follow_current_request_after_stop() {
    let (_directory, mut core) = fixture();
    let _audio = recording_audio(&mut core);
    core.command(Command::Stop).unwrap();
    core.command(Command::Device {
        name: Some("PipeWire: desired".into()),
    })
    .unwrap();
    let auto_mix = core.state.configuration.config.pipewire_auto_mix;
    let request = core.output_settings_request;
    core.command(Command::Device {
        name: Some("PipeWire: other".into()),
    })
    .unwrap();
    core.command(Command::Device {
        name: Some("PipeWire: desired".into()),
    })
    .unwrap();
    core.audio_event(AudioEvent::OutputSettingsFailed {
        request,
        device: Some("PipeWire: desired".into()),
        auto_mix,
        message: "superseded same-target failure".into(),
    })
    .unwrap();
    assert!(core.state.system.last_error.is_none());
    for (device, mix) in [
        ("PipeWire: superseded", auto_mix),
        ("PipeWire: desired", !auto_mix),
    ] {
        core.audio_event(AudioEvent::OutputSettingsFailed {
            request: core.output_settings_request,
            device: Some(device.into()),
            auto_mix: mix,
            message: "obsolete failure".into(),
        })
        .unwrap();
        assert!(core.state.system.last_error.is_none());
    }
    core.audio_event(AudioEvent::OutputSettingsFailed {
        request: core.output_settings_request,
        device: Some("PipeWire: desired".into()),
        auto_mix,
        message: "desired output unavailable".into(),
    })
    .unwrap();
    assert_eq!(
        core.state.system.last_error.as_deref(),
        Some("desired output unavailable")
    );
    assert_eq!(
        core.state.system.selected_device.as_deref(),
        Some("PipeWire: desired")
    );
    assert_eq!(core.state.playback.status, PlaybackStatus::Stopped);

    core.command(Command::Stop).unwrap();
    let queue_id = core.store.queue_id_at(0).unwrap().unwrap();
    playing(&mut core, queue_id);
    let playback_generation = core.generation;
    core.state.system.last_error = None;
    core.audio_event(AudioEvent::OutputSettingsFailed {
        request: core.output_settings_request,
        device: Some("PipeWire: desired".into()),
        auto_mix,
        message: "configuration failed before the newer playback".into(),
    })
    .unwrap();
    assert_eq!(
        core.state.system.last_error.as_deref(),
        Some("configuration failed before the newer playback")
    );
    assert_eq!(core.state.playback.status, PlaybackStatus::Playing);
    assert_eq!(core.generation, playback_generation);
}

#[test]
fn unavailable_output_targets_remain_desired_and_persist_across_entry_points() {
    let (_directory, mut core) = fixture();
    let _audio = recording_audio(&mut core);
    Arc::make_mut(&mut core.state.system.devices).clear();
    let mut config = core.state.configuration.config.as_ref().clone();
    config.ffmpeg_enabled = false;
    config.output_device = Some("PipeWire: unavailable-configured-sink".into());
    core.command(Command::Configure {
        config: Arc::new(config),
    })
    .unwrap();
    assert_eq!(
        Config::load(&core.state.configuration.config_path)
            .unwrap()
            .output_device,
        core.state.system.selected_device
    );
    assert_eq!(
        core.state.system.selected_device.as_deref(),
        Some("PipeWire: unavailable-configured-sink")
    );
    core.command(Command::Device {
        name: Some("PipeWire: unavailable-replacement-sink".into()),
    })
    .unwrap();
    core.flush_config().unwrap();
    assert_eq!(
        Config::load(&core.state.configuration.config_path)
            .unwrap()
            .output_device
            .as_deref(),
        Some("PipeWire: unavailable-replacement-sink")
    );
}

#[test]
fn configure_saved_but_disconnected_audio_retains_desired_settings() {
    let (_directory, mut core) = fixture();
    let (commands, receiver) = async_channel::bounded(64);
    drop(receiver);
    core.engine.replace_commands(commands);
    let mut config = core.state.configuration.config.as_ref().clone();
    config.volume = 0.25;
    config.shuffle = true;
    config.repeat = RepeatMode::All;
    config.visualization_enabled = false;
    config.ffmpeg_enabled = false;
    config.page_size = 32;
    config.output_device = Some("configured-output".into());
    Arc::make_mut(&mut core.state.system.devices).push("configured-output".into());
    core.played = vec![1, 2];
    core.played_cursor = 2;
    let expected = config.clone();

    core.command(Command::Configure {
        config: std::sync::Arc::new(config),
    })
    .unwrap_err();

    assert_eq!(*core.state.configuration.config, expected);
    assert_eq!(core.state.playback.volume, expected.volume);
    assert_eq!(core.state.playback.shuffle, expected.shuffle);
    assert_eq!(core.state.playback.repeat, expected.repeat);
    assert_eq!(core.state.system.selected_device, expected.output_device);
    assert_eq!(core.state.system.ffmpeg_status, "disabled");
    assert!(core.played.is_empty());
    assert_eq!(core.played_cursor, 0);
    assert_eq!(
        Config::load(&core.state.configuration.config_path).unwrap(),
        expected
    );
    assert!(!core.flush_config().unwrap());
    core.publish();
    assert_eq!(*core.shared.read().configuration.config, expected);
}

#[test]
fn visualization_enabled_toggle_commits_without_accessing_unwritable_config_path() {
    let (_directory, mut core) = fixture();
    let _audio = recording_audio(&mut core);
    let previous = core.state.configuration.config.visualization_enabled;
    std::fs::create_dir(&core.state.configuration.config_path).unwrap();
    core.update_gui_config(GuiConfigChange::ToggleVisualization)
        .unwrap();
    assert_eq!(
        core.state.configuration.config.visualization_enabled,
        !previous
    );
    assert!(core.state.configuration.config_path.is_dir());
}

#[test]
fn visualization_enabled_applied_but_disconnected_audio_retains_unsaved_settings() {
    let (_directory, mut core) = fixture();
    let (commands, receiver) = async_channel::bounded(1);
    drop(receiver);
    core.engine.replace_commands(commands);
    // Pending playback settings must be incorporated in the same memory snapshot.
    core.state.playback.volume = 0.25;
    core.state.playback.shuffle = true;
    core.state.playback.repeat = RepeatMode::All;
    core.state.system.selected_device = Some("pending-output".into());
    let mut expected = core.state.configuration.config.as_ref().clone();
    expected.volume = 0.25;
    expected.shuffle = true;
    expected.repeat = RepeatMode::All;
    expected.output_device = Some("pending-output".into());

    for enabled in [false, true] {
        expected.visualization_enabled = enabled;
        core.update_gui_config(GuiConfigChange::ToggleVisualization)
            .unwrap_err();

        assert_eq!(*core.state.configuration.config, expected);
        assert_eq!(core.state.playback.volume, expected.volume);
        assert_eq!(core.state.playback.shuffle, expected.shuffle);
        assert_eq!(core.state.playback.repeat, expected.repeat);
        assert_eq!(core.state.system.selected_device, expected.output_device);
        assert!(!core.state.configuration.config_path.exists());
        core.publish();
        assert_eq!(*core.shared.read().configuration.config, expected);
    }
}

#[test]
fn ffmpeg_external_load_result_survives_runtime_disable_and_reenable() {
    let (directory, mut core) = fixture();
    let _audio = recording_audio(&mut core);
    Arc::make_mut(&mut core.state.configuration.config).ffmpeg_enabled = false;
    core.state.system.ffmpeg_status = "disabled".into();
    let (handle, control, worker, _publications) = navigation_runtime(core);
    let changes = handle.subscribe_revisions();
    let configure = |enabled| {
        let mut config = handle.config_snapshot().as_ref().clone();
        config.ffmpeg_enabled = enabled;
        let ack = futures::executor::block_on(handle.operation_ack_async(
            Operation::Command(Command::Configure {
                config: Arc::new(config),
            }),
            Duration::from_secs(5),
        ));
        assert!(ack.is_success(), "{:?}", ack.error);
    };

    configure(true);
    // An enhanced probe attempts the same process-wide load as GUI waveform
    // work, without an audio-engine event. A missing file avoids decoder I/O;
    // both successful and failed library loads must become terminal status.
    assert!(audio::probe_details(&directory.path().join("missing.wav"), true, true).is_err());
    let expected = audio::ffmpeg_status().unwrap_or_else(|error| format!("unavailable: {error:#}"));
    assert_ne!(expected, "not loaded");
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle.state.read().system.ffmpeg_status != expected {
        futures::executor::block_on(rivu_protocol::transport::timeout_at(
            deadline,
            changes.recv(),
        ))
        .expect("external decoder load result must reach core state before deadline")
        .expect("core revision subscription must remain connected");
    }

    configure(false);
    assert_eq!(handle.state.read().system.ffmpeg_status, "disabled");
    configure(true);
    assert_eq!(handle.state.read().system.ffmpeg_status, expected);
    control.request_shutdown().unwrap();
    worker.join().unwrap();
}

#[test]
fn configure_shuffle_keeps_current_occurrence_as_history_start() {
    let (directory, mut core) = fixture();
    for id in 1..=4 {
        std::fs::write(directory.path().join(format!("{id}.wav")), []).unwrap();
    }
    let _audio = recording_audio(&mut core);
    let first = core.store.queue_id_at(0).unwrap().unwrap();
    let next = core.store.queue_id_at(4).unwrap().unwrap();
    core.start(first, true).unwrap();
    core.command(Command::Pause).unwrap();
    let mut config = core.state.configuration.config.as_ref().clone();
    config.shuffle = true;
    let config = Arc::new(config);
    core.command(Command::Configure {
        config: Arc::clone(&config),
    })
    .unwrap();
    // Choose a non-adjacent draw deterministically, then reapply the same mode.
    core.start(next, true).unwrap();
    core.command(Command::Pause).unwrap();
    core.command(Command::Configure { config }).unwrap();
    core.navigate(Navigation::Previous).unwrap();
    assert_eq!(core.state.queue.current_id, Some(first));
    assert_eq!(core.state.playback.status, PlaybackStatus::Paused);
}
