use super::*;
use crate::test_support::CoreRuntime;
use gpui::{EntityInputHandler, TestAppContext};
use rivu::test_support::catalog::CatalogFixture;
use rivu_protocol::response::QueryError;
use rivu_protocol::wire::{
    CommandOutcome, CommandOutput, CommandReceipt, CommandReply, SourceTagOutput,
};

pub(super) fn track(id: i64, title: &str) -> Track {
    Track {
        id,
        title: title.into(),
        path: Default::default(),
        fingerprint: None,
        cue: None,
        artist: "artist".into(),
        album: "album".into(),
        duration: None,
        codec: String::new(),
        channels: 0,
        sample_rate: 0,
        bitrate_bps: None,
        bitrate_exact: false,
        track_number: None,
        disc_number: None,
        bits_per_sample: None,
        release_date: None,
        replaygain_track_gain: None,
        favorite: false,
        decoder_hint: DecoderHint::Auto,
        missing: false,
        play_count: 0,
        last_played: None,
    }
}

#[gpui::test]
fn source_peak_rejects_changed_selection_file_and_explicit_reload(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        this.metadata.select(1);
        let original = this.metadata.track().unwrap().clone();
        let peak = SourcePeak {
            track_id: original.id,
            path: original.path.clone(),
            source_serial: this.metadata.source.generation(),
            reload: this.source_peak_reload,
            eligible: true,
            value: Some(0.75),
        };
        assert!(this.source_peak_current(&peak));

        let mut relocated = original.clone();
        relocated.path = "relocated.flac".into();
        let read = this.metadata.begin_load(original.id);
        assert!(this.metadata.accept_load(read, Some(relocated)));
        assert!(!this.source_peak_current(&peak));

        let read = this.metadata.begin_load(original.id);
        assert!(this.metadata.accept_load(read, Some(original.clone())));
        assert!(this.source_peak_current(&peak));
        this.request_track(original.id, cx);
        assert!(!this.source_peak_current(&peak));

        let peak = SourcePeak {
            reload: this.source_peak_reload,
            source_serial: this.metadata.source.generation(),
            ..peak
        };
        assert!(this.source_peak_current(&peak));
        this.metadata.source.stage(file_tags());
        assert!(!this.source_peak_current(&peak));

        let peak = SourcePeak {
            source_serial: this.metadata.source.generation(),
            ..peak
        };
        assert!(this.source_peak_current(&peak));
        this.metadata.select(original.id + 1);
        assert!(!this.source_peak_current(&peak));
    });
}

#[gpui::test]
fn decoder_choice_does_not_close_or_submit_while_source_is_busy(cx: &mut TestAppContext) {
    use rivu_format::tags::TagEncoding;

    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
            this.hint_panel = Some(1);
            this.hint_choices
                .open([ChoiceItem::new(DecoderHint::Native, "Native")], 0);
            this.metadata
                .begin_source(source::SourceAction::Load(TagEncoding::Declared))
                .unwrap();
            this.choose_hint(0, window, cx);
            assert_eq!(this.hint_panel, Some(1));
            assert!(!this.metadata.saving(Field::DecoderHint));
            assert_eq!(
                this.metadata.track().unwrap().decoder_hint,
                DecoderHint::Auto
            );
        });
    })
    .unwrap();
}

fn fixture(cx: &mut TestAppContext) -> (tempfile::TempDir, CoreRuntime, Entity<Metadata>) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = CoreRuntime::start_quiet(directory.path());
    let metadata = metadata_owner(&directory, &runtime, cx);
    metadata.update(cx, |this, cx| {
        playback(this, track(1, "A"), cx);
    });
    (directory, runtime, metadata)
}

fn metadata_owner(
    directory: &tempfile::TempDir,
    runtime: &CoreRuntime,
    cx: &mut TestAppContext,
) -> Entity<Metadata> {
    cx.update(|cx| {
        crate::gui::appearance::theme::set_theme(rivu_presentation::theme::Theme::default(), cx);
        crate::gui::appearance::fonts::publish(&gpui::font("sans-serif"), 12, cx);
    });
    let context = FeatureContext::for_test(runtime.handle.clone());
    let default_artwork = cx.new(|cx| DefaultArtwork::new(&context.i18n, cx));
    cx.new(|cx| {
        Metadata::new(
            context,
            gpui::font("sans-serif"),
            Arc::new(ArtworkManager::new(directory.path())),
            ArtworkImageRegistry::default(),
            default_artwork,
            cx,
        )
    })
}

fn stopped_playback_fixture(
    cx: &mut TestAppContext,
) -> (tempfile::TempDir, CoreRuntime, Entity<Metadata>) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let path = root.join("unavailable.wav");
    let catalog = CatalogFixture::open(&root.join("library.db")).unwrap();
    catalog
        .seed_tracks(root.to_path_buf(), vec![(path.clone(), "playback".into())])
        .unwrap();
    let track_id = catalog.track_id(&path).unwrap().unwrap();
    drop(catalog);
    let runtime = CoreRuntime::start_quiet(root);
    // Selecting a missing queue item publishes its stopped track without starting audio.
    let delivery =
        futures::executor::block_on(runtime.handle.request_command(Command::Play { track_id }));
    assert!(delivery.error().unwrap().contains("Missing audio file"));
    let state = runtime.handle.gui_snapshot();
    assert_eq!(state.current_track().unwrap().id, track_id);
    let metadata = metadata_owner(&directory, &runtime, cx);
    (directory, runtime, metadata)
}

fn playback(owner: &mut Metadata, track: Track, cx: &mut Context<Metadata>) {
    let mut state = (*owner.state).clone();
    state.current_track = Some(Arc::new(track));
    owner.reconcile(Rc::new(state), cx);
}

#[gpui::test]
fn bitrate_revision_retains_inline_text_and_queues_hidden_reload(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
            artwork_selection(this, track(1, "original"), cx);
            this.begin_edit(Field::Title, window, cx);
            this.edit
                .as_ref()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_text("local edit", cx));
            let original = this.track_snapshot().unwrap();
            let mut state = (*this.state).clone();
            state.library.bitrate_revision += 1;
            this.reconcile(Rc::new(state), cx);
            assert!(Arc::ptr_eq(&this.track_snapshot().unwrap(), &original));
            assert!(!this.metadata.editable());
            assert_eq!(
                this.edit.as_ref().unwrap().input.read(cx).text(),
                "local edit"
            );
            assert_eq!(this.edit.as_ref().unwrap().baseline, "original");
            assert!(
                this.metadata_read
                    .prepare(&mut this.metadata, false, Instant::now())
                    .is_none()
            );
            assert!(
                this.metadata_read
                    .prepare(&mut this.metadata, true, Instant::now())
                    .is_some()
            );
        });
    })
    .unwrap();
}

#[gpui::test]
fn hidden_playback_advances_without_a_track_query(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        assert_eq!(this.metadata.selected(), None);
        assert_eq!(this.metadata.identity(), Some(1));
        assert!(!this.visible);
        let mut state = (*this.state).clone();
        state.library.bitrate_revision += 1;
        state.current_track = Some(Arc::new(track(2, "B")));
        this.reconcile(Rc::new(state), cx);
        assert_eq!(this.metadata.identity(), Some(2));
        let projected = this.state.current_track.as_ref().unwrap();
        assert!(Arc::ptr_eq(&this.track_snapshot().unwrap(), projected));
        this.demand_metadata(cx);
        assert!(
            this.metadata_read
                .prepare(&mut this.metadata, true, Instant::now())
                .is_none()
        );
        assert_eq!(this.metadata.read_state(), &MetadataReadState::Idle);
        assert!(this.metadata.deadline().is_none());
        assert!(this.metadata.editable());
    });
}

#[gpui::test]
fn playback_and_explicit_selection_own_target_not_read_completion(cx: &mut TestAppContext) {
    use rivu_protocol::response::{QueryResponse, ViewResponse};

    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        playback(this, track(2, "B"), cx);
        assert_eq!(this.metadata.identity(), Some(2));

        this.request_track(2, cx);
        let task = this
            .metadata_read
            .prepare(&mut this.metadata, true, Instant::now())
            .unwrap();
        assert_eq!(task.read.query(), Query::Track { track_id: 2 });
        let revisions = this.state.query_revisions_for(&task.read.query());
        assert!(this.metadata_read.submit(
            task.read,
            &this.metadata,
            ReadEnvironment {
                visible: true,
                revisions,
            },
        ));
        playback(this, track(3, "C"), cx);
        assert_eq!(this.metadata.selected(), Some(2));
        assert_eq!(this.metadata.track().unwrap().title, "B");

        playback(this, track(2, "new projected B"), cx);
        assert_eq!(this.metadata.track().unwrap().title, "B");
        this.clear_selection(cx);
        assert_eq!(this.metadata.identity(), Some(2));
        assert_eq!(this.metadata.track().unwrap().title, "new projected B");
        assert!(matches!(
            this.metadata_read.finish(
                task.read,
                &mut this.metadata,
                ReadEnvironment {
                    visible: true,
                    revisions,
                },
                QueryResponse {
                    revisions,
                    result: Ok(ViewResponse::Track(Some(track(2, "late B")))),
                },
                || unreachable!(),
            ),
            ReadCompletion::Retired
        ));
        assert_eq!(this.metadata.track().unwrap().title, "new projected B");
        assert!(
            this.metadata_read
                .prepare(&mut this.metadata, true, Instant::now())
                .is_none()
        );

        let mut state = (*this.state).clone();
        state.current_track = None;
        this.reconcile(Rc::new(state), cx);
        assert_eq!(this.metadata.identity(), None);
        assert!(this.metadata.track().is_none());
    });
}

fn edit_text(
    metadata: &Entity<Metadata>,
    field: Field,
    value: &str,
    window: gpui::AnyWindowHandle,
    cx: &mut TestAppContext,
) {
    let input = metadata.read_with(cx, |this, _| {
        let edit = this.edit.as_ref().unwrap();
        assert_eq!(edit.field, field);
        edit.input.clone()
    });
    cx.update_window(window, |_, window, cx| {
        input.update(cx, |input, cx| {
            let end = input.text().encode_utf16().count();
            input.replace_text_in_range(Some(0..end), value, window, cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
}

fn ack(ok: bool) -> crate::frontend::CommandDelivery {
    command_delivery(ok, None)
}

fn command_delivery(
    ok: bool,
    source_tags: Option<SourceTagOutput>,
) -> crate::frontend::CommandDelivery {
    let outcome = if ok {
        CommandOutcome::Succeeded(
            source_tags.map_or(CommandOutput::None, CommandOutput::SourceTags),
        )
    } else {
        CommandOutcome::Failed {
            error: "save failed".into(),
            source_tags,
        }
    };
    crate::frontend::CommandDelivery {
        result: Ok(CommandReply::Receipt(CommandReceipt {
            revision: 0,
            outcome,
        })),
        observation: crate::frontend::CommandObservation::Refreshed,
    }
}

fn file_tags() -> rivu_format::tags::TagValues {
    rivu_format::tags::TagValues {
        title: "file title".into(),
        artist: "file artist".into(),
        album: "file album".into(),
        release_date: Some("2026-11-14".into()),
        track_number: Some(7),
        disc_number: Some(2),
        replaygain_track_gain: Some(-4.5),
    }
}

fn source_ack(ok: bool, file_saved: bool) -> crate::frontend::CommandDelivery {
    command_delivery(
        ok,
        Some(SourceTagOutput {
            tags: Some(file_tags()),
            file_saved,
            refresh_error: None,
        }),
    )
}

#[test]
fn bulk_source_commands_include_seven_staged_values_and_database_expectation() {
    use source::{SourceAction, database_tags};
    let mut session = MetadataSession::default();
    let mut original = track(1, "database title");
    original.play_count = 42;
    original.favorite = true;
    original.decoder_hint = DecoderHint::Native;
    let original = Arc::new(original);
    session.sync_playback(Some(&original));
    session.source.stage(file_tags());
    assert_eq!(session.track(), Some(original.as_ref()));
    for action in [SourceAction::Sync, SourceAction::Save] {
        let (token, command) = session.begin_source(action).unwrap();
        match command {
            Command::SyncTrackTags {
                track_id,
                tags,
                expected,
            }
            | Command::SaveSourceTags {
                track_id,
                tags,
                expected,
            } => {
                assert_eq!(track_id, 1);
                assert_eq!(tags, file_tags());
                assert_eq!(*expected, database_tags(&original));
                assert_ne!(*expected, tags);
            }
            other => panic!("unexpected tag command: {other:?}"),
        }
        assert!(session.begin_save(&SavedValue::PlayCount(43)).is_none());
        assert_eq!(session.source.finish(token), Some(true));
    }
    let (save, _) = session
        .begin_save(&SavedValue::Title("single field".into()))
        .unwrap();
    assert!(session.begin_source(SourceAction::Sync).is_none());
    assert!(session.complete_save(save));
    assert_eq!(session.track(), Some(original.as_ref()));
}

#[test]
fn staged_replaygain_value_displays_and_clears_independently() {
    let original = track(1, "database title");
    let mut source = source::SourceTags::default();
    source.stage(file_tags());
    assert_eq!(
        source.field_text(Field::ReplayGainTrackGain, &original),
        "-4.5"
    );
    source.field_saved(Field::ReplayGainTrackGain);
    assert_eq!(
        source.values(&original).replaygain_track_gain,
        original.replaygain_track_gain
    );
    assert_eq!(source.values(&original).title, "file title");
    let mut cleared = file_tags();
    cleared.replaygain_track_gain = None;
    source.stage(cleared);
    assert_eq!(source.field_text(Field::ReplayGainTrackGain, &original), "");
    assert_eq!(source.values(&original).replaygain_track_gain, None);
}

#[test]
fn source_capability_uses_container_and_codec_hints_without_file_access() {
    let mut target = track(1, "supported");
    for (extension, codec, supported) in [
        ("flac", "FLAC", true),
        ("wav", "ADPCM_MS", true),
        ("wav", "ADPCM_IMA_WAV", true),
        ("aiff", "ADPCM_IMA_QT", true),
        ("aac", "aac (LC)", true),
        ("m4a", "aac (HE-AAC)", true),
        ("flac", "dts", false),
        ("flac", "dts (DTS-HD MA)", false),
        ("m4a", "opus", false),
    ] {
        target.path = format!("/does/not/exist/test.{extension}").into();
        target.codec = codec.into();
        assert_eq!(
            source::source_support(Some(&target)),
            if supported {
                Ok(())
            } else {
                Err(source::SupportReason::Unsupported)
            },
            "{extension} / {codec}"
        );
    }
    target.path = "/does/not/exist/test.flac".into();
    target.codec = "flac".into();
    target.missing = true;
    assert_eq!(
        source::source_support(Some(&target)),
        Err(source::SupportReason::Missing)
    );
    assert_eq!(
        source::source_support(None),
        Err(source::SupportReason::Unavailable)
    );
    target.missing = false;
    target.cue = Some(rivu_protocol::model::CueSegment {
        sheet: "/does/not/exist/disc.cue".into(),
        number: 1,
        start_frame: 0,
        end_frame: None,
    });
    assert_eq!(
        source::source_support(Some(&target)),
        Err(source::SupportReason::Cue)
    );
}

#[gpui::test]
fn file_load_stages_all_tags_even_when_observation_is_unavailable(cx: &mut TestAppContext) {
    use rivu_format::tags::TagEncoding;
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        let original = this.metadata.track().unwrap().clone();
        let (token, command) = this
            .metadata
            .begin_source(source::SourceAction::Load(TagEncoding::Gb18030))
            .unwrap();
        assert!(matches!(
            command,
            Command::LoadSourceTags {
                track_id: 1,
                encoding: TagEncoding::Gb18030
            }
        ));
        let mut delivery = source_ack(true, false);
        delivery.observation =
            crate::frontend::CommandObservation::Unavailable("refresh unavailable".into());
        this.finish_source_action(token, delivery, cx);
        assert_eq!(this.metadata.track(), Some(&original));
        assert_eq!(this.metadata.source.values(&original), file_tags());
        assert_eq!(
            this.metadata.source.status,
            Some(source::SourceStatus::Staged)
        );
        assert!(!this.metadata.any_saving());
        assert!(!this.metadata.source.busy());
        assert_eq!(this.error.as_deref(), Some("refresh unavailable"));
    });
}

#[gpui::test]
fn late_source_load_cannot_cross_selection_epoch_or_replace_newer_operation(
    cx: &mut TestAppContext,
) {
    use rivu_format::tags::TagEncoding;
    use source::SourceAction;
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        let (old, _) = this
            .metadata
            .begin_source(SourceAction::Load(TagEncoding::Declared))
            .unwrap();
        artwork_selection(this, track(2, "other"), cx);
        artwork_selection(this, track(1, "returned"), cx);
        let (new, _) = this
            .metadata
            .begin_source(SourceAction::Load(TagEncoding::Utf8))
            .unwrap();
        this.finish_source_action(old, source_ack(true, false), cx);
        assert!(this.metadata.source.busy());
        assert_eq!(
            this.metadata
                .source
                .values(this.metadata.track().unwrap())
                .title,
            "returned"
        );
        this.finish_source_action(new, source_ack(true, false), cx);
        assert_eq!(
            this.metadata.source.values(this.metadata.track().unwrap()),
            file_tags()
        );
        let (latest, _) = this
            .metadata
            .begin_source(SourceAction::Load(TagEncoding::Big5))
            .unwrap();
        this.finish_source_action(new, source_ack(true, false), cx);
        assert!(this.metadata.source.busy());
        this.finish_source_action(latest, source_ack(false, false), cx);
        assert!(!this.metadata.source.busy());
    });
}

#[gpui::test]
fn newer_inline_edit_rejects_pending_load_values(cx: &mut TestAppContext) {
    use rivu_format::tags::TagEncoding;
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
            let (token, _) = this
                .metadata
                .begin_source(source::SourceAction::Load(TagEncoding::Declared))
                .unwrap();
            this.begin_edit(Field::Title, window, cx);
            this.edit
                .as_ref()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_text("newer input", cx));
            this.finish_source_action(token, source_ack(true, false), cx);
            assert_eq!(
                this.metadata
                    .source
                    .values(this.metadata.track().unwrap())
                    .title,
                "A"
            );
            assert_eq!(
                this.edit.as_ref().unwrap().input.read(cx).text(),
                "newer input"
            );
            assert!(!this.metadata.source.busy());
        });
    })
    .unwrap();
}

#[gpui::test]
fn successful_inline_save_releases_only_its_staged_field_after_refresh(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        this.metadata.source.stage(file_tags());
        let (token, command) = this.metadata.begin_save(&SavedValue::Title("edited title".into())).unwrap();
        assert!(matches!(&command, Command::EditTrack {
            title: Some(title), artist: None, album: None, release_date: None, decoder_hint: None, ..
        } if title == "edited title"));
        this.finish_save(token, None, ack(true), cx);
        let mut refreshed = track(1, "edited title");
        refreshed.artist = "database artist changed".into();
        refreshed.track_number = Some(99);
        playback(this, refreshed, cx);
        let mut expected = file_tags();
        expected.title = "edited title".into();
        assert_eq!(this.metadata.source.values(this.metadata.track().unwrap()), expected);
        let (count, _) = this.metadata.begin_save(&SavedValue::PlayCount(4)).unwrap();
        this.finish_save(count, None, ack(true), cx);
        assert_eq!(this.metadata.source.values(this.metadata.track().unwrap()), expected);
    });
}

fn project_recovery(owner: &mut Metadata, error: Option<&str>, cx: &mut Context<Metadata>) {
    let mut state = (*owner.state).clone();
    state.system.source_tag_recovery = error.map(|error| {
        Arc::new(rivu_protocol::model::SourceTagRecovery {
            track_id: 1,
            tags: file_tags(),
            error: error.into(),
        })
    });
    owner.reconcile(Rc::new(state), cx);
}

#[gpui::test]
fn fresh_metadata_owner_routes_observed_save_to_database_only(cx: &mut TestAppContext) {
    use rivu_format::tags::TagEncoding;
    use source::SourceAction;

    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        // No local SaveSourceTags request or ACK: the recovery comes from state.
        project_recovery(this, Some("database write failed"), cx);
        let mut selected = track(1, "database title");
        selected.missing = true;
        artwork_selection(this, selected.clone(), cx);
        assert_eq!(this.metadata.track(), Some(&selected));
        assert_eq!(this.metadata.source.values(&selected), file_tags());
        assert!(this.source_status().unwrap().contains("database write failed"));
        assert!(
            this.metadata
                .begin_source(SourceAction::Load(TagEncoding::Declared))
                .is_none()
        );
        assert!(this.metadata.begin_source(SourceAction::Save).is_none());
        let (token, command) = this.metadata.begin_source(SourceAction::Sync).unwrap();
        assert!(
            matches!(command, Command::RetrySourceTagSync { track_id: 1, tags } if tags == file_tags())
        );
        this.metadata.source.finish(token).unwrap();

        // Another client can complete recovery without delivering this window an ACK.
        project_recovery(this, None, cx);
        assert!(this.source_status().is_none());
        assert!(matches!(
            this.metadata.begin_source(SourceAction::Sync).unwrap().1,
            Command::SyncTrackTags { track_id: 1, .. }
        ));
    });
}

#[gpui::test]
fn projected_save_survives_selection_changes_before_or_after_ack(cx: &mut TestAppContext) {
    use source::SourceAction;
    let (_directory, _runtime, metadata) = fixture(cx);
    for leave_before_ack in [false, true] {
        metadata.update(cx, |this, cx| {
            let original = track(1, "database title");
            let other = track(2, "other track");
            artwork_selection(this, original.clone(), cx);
            let (save, _) = this.metadata.begin_source(SourceAction::Save).unwrap();
            let mut delivery = source_ack(false, true);
            delivery.observation =
                crate::frontend::CommandObservation::Unavailable("observation also failed".into());
            if leave_before_ack {
                artwork_selection(this, other.clone(), cx);
                this.finish_source_action(save, delivery, cx);
                assert!(this.metadata.source.recovery(1).is_none());
                project_recovery(this, Some("save failed"), cx);
            } else {
                project_recovery(this, Some("save failed"), cx);
                this.finish_source_action(save, delivery, cx);
                artwork_selection(this, other.clone(), cx);
            }
            assert_eq!(this.metadata.track(), Some(&other));
            assert_eq!(
                this.metadata.source.values(&other),
                source::database_tags(&other)
            );
            assert!(this.source_status().is_none());
            assert!(this.metadata.source.recovery(2).is_none());
            let (sync, command) = this.metadata.begin_source(SourceAction::Sync).unwrap();
            assert!(
                matches!(command, Command::SyncTrackTags { track_id: 2, tags, .. }
                    if tags == source::database_tags(&other))
            );
            assert_eq!(this.metadata.source.finish(sync), Some(true));

            artwork_selection(this, original.clone(), cx);
            assert_eq!(this.metadata.track(), Some(&original));
            assert_eq!(this.metadata.source.values(&original), file_tags());
            assert!(this.source_status().unwrap().contains("save failed"));
            assert!(this.metadata.begin_source(SourceAction::Save).is_none());
            let (retry, command) = this.metadata.begin_source(SourceAction::Sync).unwrap();
            assert!(
                matches!(command, Command::RetrySourceTagSync { track_id: 1, tags }
                    if tags == file_tags())
            );

            artwork_selection(this, other.clone(), cx);
            project_recovery(this, Some("retry failed"), cx);
            this.finish_source_action(retry, ack(false), cx);
            assert_eq!(this.metadata.track(), Some(&other));
            assert!(this.source_status().is_none());
            artwork_selection(this, original.clone(), cx);
            assert!(this.source_status().unwrap().contains("retry failed"));
            let (retry, command) = this.metadata.begin_source(SourceAction::Sync).unwrap();
            assert!(
                matches!(command, Command::RetrySourceTagSync { track_id: 1, tags }
                    if tags == file_tags())
            );
            if leave_before_ack {
                artwork_selection(this, other.clone(), cx);
            }
            project_recovery(this, None, cx);
            this.finish_source_action(retry, source_ack(true, false), cx);
            assert!(this.metadata.source.recovery(1).is_none());
            if leave_before_ack {
                assert_eq!(this.metadata.track(), Some(&other));
                assert!(this.source_status().is_none());
            } else {
                assert_eq!(
                    source::database_tags(this.metadata.track().unwrap()),
                    file_tags()
                );
                assert_eq!(
                    this.metadata.source.status,
                    Some(source::SourceStatus::Synced)
                );
            }
        });
    }
}

#[gpui::test]
fn source_writes_without_observation_preserve_database_view_and_staging(cx: &mut TestAppContext) {
    use crate::frontend::CommandObservation;
    use source::{SourceAction, SourceStatus};

    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        let original = this.metadata.track().unwrap().clone();
        let mut staged = file_tags();
        staged.title = "staged title".into();
        for (action, file_saved, status) in [
            (SourceAction::Save, true, SourceStatus::Saved),
            (SourceAction::Sync, false, SourceStatus::Synced),
        ] {
            for (observation, error) in [
                (
                    CommandObservation::Unavailable("refresh unavailable; do not retry".into()),
                    "refresh unavailable; do not retry",
                ),
                (
                    CommandObservation::NotRequested,
                    "Authoritative state was not requested",
                ),
            ] {
                this.metadata.source.stage(staged.clone());
                let (token, _) = this.metadata.begin_source(action).unwrap();
                let mut delivery = source_ack(true, file_saved);
                delivery.observation = observation;
                this.finish_source_action(token, delivery, cx);

                let track = this.metadata.track().unwrap();
                assert_eq!(track, &original);
                assert_eq!(this.metadata.source.values(track), staged);
                assert_eq!(this.metadata.source.status.as_ref(), Some(&status));
                assert_eq!(this.error.as_deref(), Some(error));
                assert!(!this.metadata.source.busy());
                assert!(this.metadata.source.recovery(track.id).is_none());
            }
        }
    });
}

#[gpui::test]
fn source_save_acknowledges_all_tags_without_touching_user_state(cx: &mut TestAppContext) {
    use source::SourceAction;
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        let mut original = track(1, "database title");
        original.favorite = true;
        original.play_count = 37;
        original.decoder_hint = DecoderHint::Native;
        artwork_selection(this, original, cx);
        let (token, _) = this.metadata.begin_source(SourceAction::Save).unwrap();
        let delivery = source_ack(true, true);
        this.finish_source_action(token, delivery, cx);
        let saved = this.metadata.track().unwrap();
        assert_eq!(source::database_tags(saved), file_tags());
        assert!(saved.favorite);
        assert_eq!(saved.play_count, 37);
        assert_eq!(saved.decoder_hint, DecoderHint::Native);
        assert_eq!(
            this.metadata.source.status,
            Some(source::SourceStatus::Saved)
        );
    });
}

#[gpui::test]
fn committed_save_with_projection_warning_waits_for_playback_tags(cx: &mut TestAppContext) {
    use source::SourceAction;
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        let mut refreshed = track(1, "saved title");
        refreshed.album = "saved album".into();
        let committed = source::database_tags(&refreshed);
        this.metadata.source.stage(committed.clone());
        let (save, _) = this.metadata.begin_source(SourceAction::Save).unwrap();
        let warning = "File and database saved; refresh failed";
        let delivery = command_delivery(
            true,
            Some(SourceTagOutput {
                tags: Some(committed.clone()),
                file_saved: true,
                refresh_error: Some(warning.into()),
            }),
        );
        this.finish_source_action(save, delivery, cx);
        assert_eq!(
            this.metadata.source.status,
            Some(source::SourceStatus::Saved)
        );
        assert_eq!(this.metadata.track().unwrap().title, "A");
        assert_eq!(this.error.as_deref(), Some(warning));
        assert!(this.metadata.source.recovery(1).is_none());
        assert!(!this.metadata.source.busy());
        assert!(this.metadata.begin_source(SourceAction::Save).is_none());
        assert!(this.metadata.begin_source(SourceAction::Sync).is_none());
        assert!(
            this.metadata
                .begin_save(&SavedValue::PlayCount(2))
                .is_none()
        );

        // An unrelated change to the same playback snapshot is not a tag refresh.
        let mut stale = (*this.metadata.track().unwrap()).clone();
        stale.favorite = true;
        playback(this, stale, cx);
        assert!(!this.metadata.editable());
        assert!(
            this.metadata_read
                .prepare(&mut this.metadata, true, Instant::now())
                .is_none()
        );

        // An explicit selection has its own read authority, but clearing it
        // must not make the still-stale playback baseline writable.
        artwork_selection(this, refreshed.clone(), cx);
        assert!(this.metadata.editable());
        this.clear_selection(cx);
        assert!(!this.metadata.editable());

        playback(this, refreshed, cx);
        assert!(this.metadata.editable());
        let (_, command) = this.metadata.begin_source(SourceAction::Save).unwrap();
        assert!(
            matches!(command, Command::SaveSourceTags { expected, .. } if *expected == committed)
        );
        assert!(
            this.metadata_read
                .prepare(&mut this.metadata, true, Instant::now())
                .is_none()
        );
    });
}

#[gpui::test]
fn explicit_reload_requests_core_even_without_a_playback_tag_block(cx: &mut TestAppContext) {
    let (_directory, mut runtime, metadata) = stopped_playback_fixture(cx);
    runtime.disconnect_frontend();
    let errors = Rc::new(std::cell::RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let errors = errors.clone();
        cx.subscribe(&metadata, move |_, event, _| {
            if let MetadataEvent::Error(error) = event {
                errors.borrow_mut().push(error.clone());
            }
        })
    });
    metadata.update(cx, |this, cx| {
        let playback_id = this.metadata.track().unwrap().id;
        artwork_selection(this, track(2, "selected"), cx);
        this.reload_playback(cx);
        assert_eq!(this.metadata.selected(), None);
        assert_eq!(this.metadata.track().unwrap().id, playback_id);
    });
    cx.run_until_parked();
    assert_eq!(&*errors.borrow(), &["GUI query queue is busy or stopped"]);
    metadata.read_with(cx, |this, _| {
        assert_eq!(this.metadata.track().unwrap().title, "playback");
        assert!(this.metadata.editable());
    });
}

#[gpui::test]
async fn explicit_reload_observes_playback_tags_committed_by_another_client(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let (directory, _runtime, metadata) = stopped_playback_fixture(cx);
    let track_id = metadata.read_with(cx, |this, _| this.metadata.track().unwrap().id);
    let ack = rivu_client::request_command(
        directory.path(),
        Command::EditTrack {
            track_id,
            title: Some("edited by another client".into()),
            artist: None,
            album: None,
            release_date: None,
            decoder_hint: None,
        },
    )
    .unwrap();
    assert!(ack.is_success(), "{ack:?}");
    metadata.update(cx, |this, cx| {
        assert_eq!(this.metadata.track().unwrap().title, "playback");
        this.reload_playback(cx);
    });
    cx.condition(&metadata, |this, _| this.playback_reload.is_none())
        .await;
    metadata.read_with(cx, |this, _| {
        assert_eq!(
            this.metadata.track().unwrap().title,
            "edited by another client"
        );
        assert_eq!(this.error, None);
    });
}

fn playback_reload_failure(error: &str) -> QueryResponse {
    QueryResponse {
        revisions: Default::default(),
        result: Err(QueryError::Message(error.into())),
    }
}

#[gpui::test]
fn only_the_current_playback_reload_can_report_a_failure(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        let superseded = this.begin_playback_reload(cx).unwrap();
        let current = this.begin_playback_reload(cx).unwrap();
        this.finish_playback_reload(superseded, playback_reload_failure("old reload"), cx);
        assert_eq!(this.error, None);
        // Timeout notice and final reply can both arrive for the same request.
        this.finish_playback_reload(current, playback_reload_failure("timed out"), cx);
        this.finish_playback_reload(current, playback_reload_failure("late reply"), cx);
        assert_eq!(this.error.as_deref(), Some("timed out"));

        let same_track_selection = this.begin_playback_reload(cx).unwrap();
        this.request_track(1, cx);
        this.finish_playback_reload(
            same_track_selection,
            playback_reload_failure("selection changed"),
            cx,
        );
        assert_eq!(this.metadata.selected(), Some(1));
        assert_eq!(this.error, None);

        let old_source = this.begin_playback_reload(cx).unwrap();
        let mut next = (*this.state).clone();
        next.source_generation = next.source_generation.wrapping_add(1);
        this.reconcile(Rc::new(next), cx);
        this.finish_playback_reload(old_source, playback_reload_failure("source changed"), cx);
        assert_eq!(this.error, None);
    });
}

#[gpui::test]
fn obsolete_playback_query_results_do_not_report_a_failure(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        for error in [QueryError::RevisionChanged, QueryError::Cancelled] {
            let reload = this.begin_playback_reload(cx).unwrap();
            this.finish_playback_reload(
                reload,
                QueryResponse {
                    revisions: Default::default(),
                    result: Err(error),
                },
                cx,
            );
            assert_eq!(this.error, None);
            assert_eq!(this.metadata.track().unwrap().title, "A");
        }
    });
}

#[gpui::test]
fn playback_reload_failure_does_not_touch_a_newer_inline_editor(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
            let reload = this.begin_playback_reload(cx).unwrap();
            this.begin_edit(Field::Title, window, cx);
            let input = this.edit.as_ref().unwrap().input.clone();
            input.update(cx, |input, cx| input.set_text("draft", cx));
            this.finish_playback_reload(reload, playback_reload_failure("old failure"), cx);
            let edit = this.edit.as_ref().unwrap();
            assert_eq!(edit.input.read(cx).text(), "draft");
            assert_eq!(edit.error, None);
            assert_eq!(this.error, None);
        });
    })
    .unwrap();
}

#[gpui::test]
fn playback_reload_reports_tag_disagreement_without_replacing_core_details(
    cx: &mut TestAppContext,
) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        let reload = this.begin_playback_reload(cx).unwrap();
        assert_eq!(reload.query(), Query::Track { track_id: 1 });
        let revisions = this.state.query_revisions_for(&reload.query());
        let mut database = track(1, "A");
        database.replaygain_track_gain = Some(-6.0);
        this.finish_playback_reload(
            reload,
            QueryResponse {
                revisions,
                result: Ok(ViewResponse::Track(Some(database))),
            },
            cx,
        );
        assert_eq!(this.metadata.track().unwrap().replaygain_track_gain, None);
        assert!(this.metadata.editable());
        assert_eq!(
            this.error.as_deref(),
            Some(this.context.i18n.text("gl-reload-playback-stale"))
        );
    });
}

#[gpui::test]
fn playback_reload_accepts_different_decoder_source_facts(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        let reload = this.begin_playback_reload(cx).unwrap();
        let revisions = this.state.query_revisions_for(&reload.query());
        let mut database = track(1, "A");
        database.path = "new-source.wav".into();
        database.duration = Some(1.0);
        this.finish_playback_reload(
            reload,
            QueryResponse {
                revisions,
                result: Ok(ViewResponse::Track(Some(database))),
            },
            cx,
        );
        assert!(this.metadata.track().unwrap().path.as_os_str().is_empty());
        assert_eq!(this.metadata.track().unwrap().duration, None);
        assert_eq!(this.error, None);
    });
}

#[gpui::test]
fn old_playback_reload_cannot_confirm_a_newer_tag_block(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        let projected = Arc::clone(this.state.current_track.as_ref().unwrap());
        this.metadata.observe_tag_commit(
            1,
            &source::database_tags(&track(1, "first commit")),
            true,
            Some(&projected),
        );
        let retired = this.begin_playback_reload(cx).unwrap();
        let reload = this.begin_playback_reload(cx).unwrap();
        let revisions = this.state.query_revisions_for(&reload.query());
        let matching_reply = || QueryResponse {
            revisions,
            result: Ok(ViewResponse::Track(Some((*projected).clone()))),
        };
        this.finish_playback_reload(retired, matching_reply(), cx);
        assert!(!this.metadata.editable());

        // A newer write can block the same baseline under the same editor identity.
        this.metadata.observe_tag_commit(
            1,
            &source::database_tags(&track(1, "newer commit")),
            true,
            Some(&projected),
        );
        this.finish_playback_reload(reload, matching_reply(), cx);
        assert!(!this.metadata.editable());

        let retry = this.begin_playback_reload(cx).unwrap();
        this.finish_playback_reload(retry, playback_reload_failure("refresh unavailable"), cx);
        assert_eq!(this.error.as_deref(), Some("refresh unavailable"));
        assert!(!this.metadata.editable());
        this.finish_playback_reload(retry, matching_reply(), cx);
        assert!(!this.metadata.editable());
    });
}

#[gpui::test]
fn playback_reload_can_confirm_restored_tags_without_a_new_snapshot(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        let projected = Arc::clone(this.state.current_track.as_ref().unwrap());
        let track_id = projected.id;
        let query = Query::Track { track_id };
        let revisions = this.state.query_revisions_for(&query);
        let mut older = revisions;
        if let Some(scope) = &mut older.track {
            scope.revision = scope.revision.wrapping_sub(1);
        } else {
            older.library = older.library.wrapping_sub(1);
        }
        let committed = track(projected.id, "saved title");
        assert!(this.metadata.observe_tag_commit(
            track_id,
            &source::database_tags(&committed),
            true,
            Some(&projected),
        ));
        for (revisions, track) in [(revisions, committed), (older, (*projected).clone())] {
            let reload = this.begin_playback_reload(cx).unwrap();
            this.finish_playback_reload(
                reload,
                QueryResponse {
                    revisions,
                    result: Ok(ViewResponse::Track(Some(track))),
                },
                cx,
            );
            assert!(
                this.metadata
                    .begin_source(source::SourceAction::Save)
                    .is_none()
            );
        }

        let reload = this.begin_playback_reload(cx).unwrap();
        this.finish_playback_reload(
            reload,
            QueryResponse {
                revisions,
                result: Ok(ViewResponse::Track(Some((*projected).clone()))),
            },
            cx,
        );
        let (_, command) = this
            .metadata
            .begin_source(source::SourceAction::Save)
            .unwrap();
        assert!(matches!(command, Command::SaveSourceTags { expected, .. }
                if *expected == source::database_tags(&projected)));
    });
}

#[gpui::test]
fn selected_tag_write_warning_queues_an_authoritative_read(cx: &mut TestAppContext) {
    use source::SourceAction;

    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        artwork_selection(this, track(2, "selected"), cx);
        this.metadata.source.stage(file_tags());
        let (sync, _) = this.metadata.begin_source(SourceAction::Sync).unwrap();
        let mut delivery = command_delivery(
            true,
            Some(SourceTagOutput {
                tags: Some(file_tags()),
                file_saved: false,
                refresh_error: Some("Database tags saved; refresh failed".into()),
            }),
        );
        delivery.observation =
            crate::frontend::CommandObservation::Unavailable("refresh unavailable".into());
        this.finish_source_action(sync, delivery, cx);

        assert_eq!(this.metadata.track().unwrap().title, "selected");
        assert!(!this.metadata.editable());
        let task = this
            .metadata_read
            .prepare(&mut this.metadata, true, Instant::now())
            .unwrap();
        assert_eq!(task.read.query(), Query::Track { track_id: 2 });
        this.clear_selection(cx);
        assert_eq!(this.metadata.track().unwrap().title, "A");
        assert!(this.metadata.editable());
    });
}

#[gpui::test]
fn newer_database_read_and_inline_input_survive_old_bulk_completion(cx: &mut TestAppContext) {
    use source::SourceAction;
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
            this.metadata.select(1);
            this.metadata.source.stage(file_tags());
            let (token, _) = this.metadata.begin_source(SourceAction::Sync).unwrap();
            let read = this.metadata.begin_load(1);
            assert!(
                this.metadata
                    .accept_load(read, Some(track(1, "newer database")))
            );
            this.begin_edit(Field::Album, window, cx);
            this.edit
                .as_ref()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_text("newer album", cx));
            this.finish_source_action(token, source_ack(true, false), cx);
            assert_eq!(this.metadata.track().unwrap().title, "newer database");
            assert_eq!(
                this.metadata.source.values(this.metadata.track().unwrap()),
                file_tags()
            );
            assert_eq!(
                this.edit.as_ref().unwrap().input.read(cx).text(),
                "newer album"
            );
        });
    })
    .unwrap();
}

#[gpui::test]
fn selection_and_blur_cancel_without_submitting(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    for field in [
        Field::Title,
        Field::Artist,
        Field::Album,
        Field::ReleaseDate,
        Field::PlayCount,
    ] {
        cx.update_window(window, |_, window, cx| {
            metadata.update(cx, |this, cx| {
                playback(this, track(1, "A"), cx);
                this.begin_edit(field, window, cx);
            })
        })
        .unwrap();
        edit_text(&metadata, field, "draft", window, cx);
        metadata.update(cx, |this, cx| {
            playback(this, track(2, "B"), cx);
            assert!(this.edit.is_none());
            assert_eq!(this.metadata.identity(), Some(2));
            assert!(!this.metadata.saving(field));
        });
    }
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
            this.begin_edit(Field::Title, window, cx);
            let focus = cx.focus_handle();
            focus.focus(window, cx);
        })
    })
    .unwrap();
    cx.run_until_parked();
    metadata.read_with(cx, |this, _| assert!(this.edit.is_none()));
}

#[gpui::test]
fn replaygain_invalid_input_retains_editor_and_zero_gain_is_not_cleared(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
            let field = Field::ReplayGainTrackGain;
            for text in ["NaN", "inf", "bad"] {
                this.begin_edit(field, window, cx);
                this.edit
                    .as_ref()
                    .unwrap()
                    .input
                    .update(cx, |input, cx| input.set_text(text, cx));
                this.submit_field(field, cx);
                assert!(this.edit.as_ref().unwrap().error.is_some());
                assert_eq!(this.edit.as_ref().unwrap().input.read(cx).text(), text);
                assert!(!this.metadata.saving(field));
            }
            let mut source = track(1, "A");
            source.replaygain_track_gain = Some(0.0);
            playback(this, source, cx);
            this.begin_edit(Field::ReplayGainTrackGain, window, cx);
            assert_eq!(this.edit.as_ref().unwrap().input.read(cx).text(), "0");
            this.submit_field(Field::ReplayGainTrackGain, cx);
            assert!(this.edit.is_none());
            assert!(!this.metadata.saving(Field::ReplayGainTrackGain));
        });
    })
    .unwrap();
}

#[gpui::test]
fn unchanged_submit_and_invalid_count_do_not_save(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
            this.begin_edit(Field::Title, window, cx);
            this.submit_field(Field::Title, cx);
            assert!(this.edit.is_none());
            assert!(!this.metadata.saving(Field::Title));
            this.begin_edit(Field::ReleaseDate, window, cx);
            assert_eq!(this.edit.as_ref().unwrap().input.read(cx).text(), "");
            this.submit_field(Field::ReleaseDate, cx);
            assert!(this.edit.is_none());
            assert!(!this.metadata.saving(Field::ReleaseDate));
            this.begin_edit(Field::PlayCount, window, cx);
            this.edit
                .as_ref()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_text("-1", cx));
            this.submit_field(Field::PlayCount, cx);
            assert!(this.edit.as_ref().unwrap().error.is_some());
            assert_eq!(this.edit.as_ref().unwrap().input.read(cx).text(), "-1");
            assert!(!this.metadata.saving(Field::PlayCount));
        })
    })
    .unwrap();
}

#[gpui::test]
fn failure_retains_input_but_late_failure_cannot_touch_reopened_edit(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
            this.begin_edit(Field::Title, window, cx);
            this.edit
                .as_ref()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_text("submitted", cx));
            let serial = this.edit.as_ref().unwrap().serial;
            let (save, _) = this
                .metadata
                .begin_save(&SavedValue::Title("submitted".into()))
                .unwrap();
            this.finish_save(save, Some((serial, "submitted".into())), ack(false), cx);
            assert_eq!(
                this.edit.as_ref().unwrap().input.read(cx).text(),
                "submitted"
            );
            assert!(this.edit.as_ref().unwrap().error.is_some());
            let (save, _) = this
                .metadata
                .begin_save(&SavedValue::Title("submitted".into()))
                .unwrap();
            this.cancel_edit(cx);
            this.begin_edit(Field::Artist, window, cx);
            this.finish_save(save, Some((serial, "submitted".into())), ack(false), cx);
            assert_eq!(this.edit.as_ref().unwrap().field, Field::Artist);
            assert!(this.edit.as_ref().unwrap().error.is_none());
        })
    })
    .unwrap();
}

#[gpui::test]
fn successful_save_without_observation_preserves_newer_draft_and_baseline(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
            this.begin_edit(Field::Title, window, cx);
            let edit = this.edit.as_ref().unwrap();
            let serial = edit.serial;
            let baseline = edit.baseline.clone();
            edit.input
                .update(cx, |input, cx| input.set_text("submitted", cx));
            let (save, _) = this
                .metadata
                .begin_save(&SavedValue::Title("submitted".into()))
                .unwrap();
            this.edit
                .as_ref()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_text("newer draft", cx));
            let mut delivery = ack(true);
            delivery.observation = crate::frontend::CommandObservation::Unavailable(
                "read failed; do not retry".into(),
            );
            this.finish_save(save, Some((serial, "submitted".into())), delivery, cx);
            let edit = this
                .edit
                .as_ref()
                .expect("unobserved success must not clear the draft");
            assert_eq!(edit.serial, serial);
            assert_eq!(edit.input.read(cx).text(), "newer draft");
            assert_eq!(edit.baseline, baseline);
            assert!(edit.error.as_ref().unwrap().contains("do not retry"));
            assert!(!this.metadata.saving(Field::Title));
        })
    })
    .unwrap();
}

#[gpui::test]
fn fresh_success_rebases_only_the_submitted_editor_and_keeps_later_text(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
            this.begin_edit(Field::Title, window, cx);
            let serial = this.edit.as_ref().unwrap().serial;
            let (save, _) = this
                .metadata
                .begin_save(&SavedValue::Title("submitted".into()))
                .unwrap();
            this.edit
                .as_ref()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_text("later", cx));
            playback(this, track(1, "observed title"), cx);
            let mut state = (*this.state).clone();
            state.library.bitrate_revision += 1;
            this.reconcile(Rc::new(state), cx);
            assert_eq!(this.metadata.track().unwrap().title, "observed title");
            assert_eq!(this.edit.as_ref().unwrap().input.read(cx).text(), "later");
            this.finish_save(save, Some((serial, "submitted".into())), ack(true), cx);
            let edit = this.edit.as_ref().unwrap();
            assert_eq!(edit.input.read(cx).text(), "later");
            assert_eq!(edit.baseline, "submitted");
            assert!(
                this.metadata_read
                    .prepare(&mut this.metadata, true, Instant::now())
                    .is_none()
            );
        })
    })
    .unwrap();
}

#[gpui::test]
fn save_ack_preserves_newer_details_and_refreshes_an_unchanged_projection(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        artwork_selection(this, track(7, "original"), cx);
        let (save, _) = this
            .metadata
            .begin_save(&SavedValue::Title("submitted".into()))
            .unwrap();
        let read = this.metadata.begin_load(7);
        assert!(this.metadata.accept_load(read, Some(track(7, "newer"))));
        this.finish_save(save, None, ack(true), cx);
        assert_eq!(this.metadata.track().unwrap().title, "newer");
        let task = this
            .metadata_read
            .prepare(&mut this.metadata, true, Instant::now())
            .unwrap();
        assert!(matches!(task.read.query(), Query::Track { track_id: 7 }));
        assert!(
            this.metadata_read
                .prepare(&mut this.metadata, true, Instant::now())
                .is_none()
        );
    });
}

#[gpui::test]
fn selection_epoch_rejects_old_acknowledgement(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        artwork_selection(this, track(7, "original"), cx);
        let (save, _) = this
            .metadata
            .begin_save(&SavedValue::Title("submitted".into()))
            .unwrap();
        this.request_track(8, cx);
        artwork_selection(this, track(7, "returned"), cx);
        this.finish_save(save, None, ack(false), cx);
        assert_eq!(this.metadata.track().unwrap().title, "returned");
        assert!(this.error.is_none());
    });
}

#[gpui::test]
fn queued_submit_cannot_commit_a_replacement_editor(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
            this.begin_edit(Field::Title, window, cx);
            let old = this.edit.as_ref().unwrap().input.clone();
            old.update(cx, |input, cx| {
                input.set_text("obsolete", cx);
                cx.emit(InputEvent::Submit);
            });
            this.cancel_edit(cx);
            this.begin_edit(Field::Title, window, cx);
            this.edit
                .as_ref()
                .unwrap()
                .input
                .update(cx, |input, cx| input.set_text("replacement", cx));
        })
    })
    .unwrap();
    cx.run_until_parked();
    metadata.read_with(cx, |this, cx| {
        assert!(!this.metadata.saving(Field::Title));
        assert_eq!(
            this.edit.as_ref().unwrap().input.read(cx).text(),
            "replacement"
        );
    });
}

fn artwork_selection(owner: &mut Metadata, track: Track, cx: &mut Context<Metadata>) {
    owner.metadata.select(track.id);
    let token = owner.metadata.begin_load(track.id);
    assert!(owner.metadata.accept_load(token, Some(track)));
    owner.cancel_edit(cx);
}

#[gpui::test]
fn hiding_details_releases_cached_artwork_until_revealed(cx: &mut TestAppContext) {
    let (directory, _runtime, metadata) = fixture(cx);
    let path = directory.path().join("cover.wav");
    rivu_metadata::artwork::test_support::tagged_wav(&path);
    let mut selected = track(7, "Cover");
    selected.path = path;
    metadata.update(cx, |this, cx| {
        assert!(
            this.artwork_manager
                .load(crate::artwork_source(&selected))
                .is_some()
        );
        artwork_selection(this, selected, cx);
        this.visible = true;
        this.prepare_artwork(cx);
        assert!(this.artwork_view.sizing(0).is_some());

        this.set_visible(false, cx);
        assert!(this.artwork_view.sizing(0).is_none());
        this.prepare_artwork(cx);
        assert!(this.artwork_view.sizing(0).is_none());

        this.set_visible(true, cx);
        this.prepare_artwork(cx);
        assert!(this.artwork_view.sizing(0).is_some());
        assert_eq!(this.metadata.identity(), Some(7));
    });
}

#[gpui::test]
fn completed_negative_artwork_keeps_fallback_when_details_reopen(cx: &mut TestAppContext) {
    let (directory, _runtime, metadata) = fixture(cx);
    let mut selected = track(7, "Missing cover");
    selected.path = directory.path().join("missing-cover.wav");
    metadata.update(cx, |this, cx| {
        assert!(
            this.artwork_manager
                .load(crate::artwork_source(&selected))
                .is_none()
        );
        assert_eq!(
            this.artwork_manager
                .cached_result(crate::artwork_source(&selected)),
            Some(None)
        );
        artwork_selection(this, selected.clone(), cx);
        this.visible = true;
        this.prepare_artwork(cx);
        assert!(this.artwork_view.sizing(0).is_none());
        assert!(
            this.artwork_loader
                .prepare(crate::artwork_source(&selected))
                .is_none()
        );
        this.set_visible(false, cx);
    });
    // A completed miss is not a new acquisition demand on every render/reopen.
    rivu_metadata::artwork::test_support::tagged_wav(&selected.path);
    metadata.update(cx, |this, cx| {
        this.set_visible(true, cx);
        this.prepare_artwork(cx);
        assert!(this.artwork_view.sizing(0).is_none());
    });
    cx.run_until_parked();
    metadata.update(cx, |this, cx| {
        assert_eq!(
            this.artwork_manager
                .cached_result(crate::artwork_source(&selected)),
            Some(None)
        );
        assert!(this.artwork_view.sizing(0).is_none());
        this.prepare_artwork(cx);
        assert!(
            this.artwork_loader
                .prepare(crate::artwork_source(&selected))
                .is_none()
        );
        assert!(this.artwork_view.sizing(0).is_none());
    });
}

#[gpui::test]
fn visible_details_keep_negative_artwork_after_shared_cache_eviction(cx: &mut TestAppContext) {
    let (directory, _runtime, metadata) = fixture(cx);
    let mut selected = track(7, "Missing cover");
    selected.path = directory.path().join("missing-cover.wav");
    metadata.update(cx, |this, cx| {
        assert!(
            this.artwork_manager
                .load(crate::artwork_source(&selected))
                .is_none()
        );
        artwork_selection(this, selected.clone(), cx);
        this.visible = true;
        this.prepare_artwork(cx);
        for id in 100..170 {
            let mut pressure = track(id, "Missing");
            pressure.path = directory.path().join("pressure").join(format!("{id}.wav"));
            assert!(
                this.artwork_manager
                    .load(crate::artwork_source(&pressure))
                    .is_none()
            );
        }
        assert!(
            this.artwork_manager
                .cached_result(crate::artwork_source(&selected))
                .is_none()
        );
        rivu_metadata::artwork::test_support::tagged_wav(&selected.path);
        this.prepare_artwork(cx);
    });
    cx.run_until_parked();
    metadata.update(cx, |this, cx| {
        assert!(
            this.artwork_manager
                .cached_result(crate::artwork_source(&selected))
                .is_none()
        );
        assert!(this.artwork_view.sizing(0).is_none());
        // Hiding releases the negative identity. Revealing cold demand may probe again.
        this.set_visible(false, cx);
        this.set_visible(true, cx);
        this.prepare_artwork(cx);
    });
    cx.run_until_parked();
    metadata.update(cx, |this, _| {
        assert!(this.artwork_view.sizing(0).is_some());
    });
}

#[gpui::test]
fn cached_relocation_is_delivered_while_old_artwork_is_in_flight(cx: &mut TestAppContext) {
    let (directory, _runtime, metadata) = fixture(cx);
    let [before, after] = crate::test_support::relocation_artwork_paths(directory.path());
    let mut first = track(7, "Cover");
    first.path = before;
    let mut latest = first.clone();
    latest.path = after;
    let expected = metadata.update(cx, |this, cx| {
        artwork_selection(this, first.clone(), cx);
        this.visible = true;
        this.prepare_artwork(cx);
        assert!(
            this.artwork_loader
                .prepare(crate::artwork_source(&latest))
                .is_none()
        );
        let expected = this
            .artwork_manager
            .load(crate::artwork_source(&latest))
            .unwrap();
        artwork_selection(this, latest.clone(), cx);
        this.prepare_artwork(cx);
        assert!(!this.artwork_view.replace(Some(expected.clone()), cx));
        expected
    });
    cx.run_until_parked();
    metadata.update(cx, |this, cx| {
        assert!(!this.artwork_view.replace(Some(expected), cx));
        assert!(
            this.artwork_loader
                .prepare(crate::artwork_source(&latest))
                .is_some()
        );
        assert!(
            this.artwork_loader
                .finish(&rivu_metadata::artwork::ArtworkIdentity::new(
                    crate::artwork_source(&latest)
                ))
        );
        this.set_visible(false, cx);
    });
    cx.run_until_parked();
    metadata.update(cx, |this, cx| {
        for id in 100..170 {
            let mut pressure = track(id, "Missing");
            pressure.path = directory.path().join("pressure").join(format!("{id}.wav"));
            assert!(
                this.artwork_manager
                    .load(crate::artwork_source(&pressure))
                    .is_none()
            );
        }
        assert!(
            this.artwork_manager
                .cached_result(crate::artwork_source(&latest))
                .is_none()
        );
        this.set_visible(true, cx);
        this.prepare_artwork(cx);
        assert!(this.artwork_view.sizing(0).is_none());
    });
    cx.run_until_parked();
    metadata.update(cx, |this, cx| {
        let expected = this
            .artwork_manager
            .cached_result(crate::artwork_source(&latest))
            .unwrap()
            .unwrap();
        assert!(!this.artwork_view.replace(Some(expected), cx));
    });
}

#[gpui::test]
fn hidden_artwork_completion_retires_without_loading_replacement(cx: &mut TestAppContext) {
    let (directory, _runtime, metadata) = fixture(cx);
    let mut first = track(7, "First");
    first.path = directory.path().join("first.wav");
    let mut replacement = track(8, "Replacement");
    replacement.path = directory.path().join("replacement.wav");
    rivu_metadata::artwork::test_support::tagged_wav(&first.path);
    rivu_metadata::artwork::test_support::tagged_wav(&replacement.path);
    metadata.update(cx, |this, cx| {
        artwork_selection(this, first.clone(), cx);
        this.visible = true;
        this.prepare_artwork(cx);
        this.set_visible(false, cx);
        artwork_selection(this, replacement.clone(), cx);
        this.prepare_artwork(cx);
    });
    cx.run_until_parked();
    metadata.update(cx, |this, cx| {
        assert!(
            this.artwork_manager
                .cached_result(crate::artwork_source(&first))
                .is_some()
        );
        assert!(
            this.artwork_manager
                .cached_result(crate::artwork_source(&replacement))
                .is_none()
        );
        assert!(this.artwork_view.sizing(0).is_none());
        assert_eq!(this.metadata.identity(), Some(8));
        this.set_visible(true, cx);
        this.prepare_artwork(cx);
    });
    cx.run_until_parked();
    metadata.update(cx, |this, _| {
        assert!(
            this.artwork_manager
                .cached_result(crate::artwork_source(&replacement))
                .is_some()
        );
        assert!(this.artwork_view.sizing(0).is_some());
        assert_eq!(this.metadata.identity(), Some(8));
    });
}

#[gpui::test]
fn stale_inline_submission_preserves_the_dirty_draft(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
            this.metadata.select(1);
            this.begin_edit(Field::Title, window, cx);
            let input = this.edit.as_ref().unwrap().input.clone();
            input.update(cx, |input, cx| input.set_text("draft title", cx));
            this.metadata.invalidate_at(Instant::now());
            this.submit_field(Field::Title, cx);
            assert_eq!(
                this.edit.as_ref().unwrap().input.read(cx).text(),
                "draft title"
            );
            assert!(!this.metadata.saving(Field::Title));
            let deadline = this.metadata.deadline().unwrap();
            assert!(this.metadata.expire(deadline));
            this.submit_field(Field::Title, cx);
            assert_eq!(
                this.edit.as_ref().unwrap().input.read(cx).text(),
                "draft title"
            );
            assert!(!this.metadata.saving(Field::Title));
        });
    })
    .unwrap();
}

#[gpui::test]
fn metadata_retention_wake_expires_without_another_read_or_frame(cx: &mut TestAppContext) {
    use std::time::Duration;

    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        this.metadata.select(1);
        let now = Instant::now() + Duration::from_secs(60);
        let query = Query::Track { track_id: 1 };
        this.metadata_read
            .request(1, this.state.query_revisions_for(&query), now);
        let _quiet_reader = this
            .metadata_read
            .prepare(&mut this.metadata, true, now)
            .unwrap();
        assert!(this.metadata.track().is_some());
        assert!(!this.metadata.editable());
        this.schedule_metadata_expiry(cx);
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(61));
    cx.run_until_parked();
    metadata.update(cx, |this, _| {
        assert!(this.metadata.track().is_none());
        assert!(
            this.track().is_some(),
            "details expiry must not retire the selected source"
        );
        assert!(this.track_snapshot().is_some());
        assert!(!this.metadata.editable());
        assert!(this.metadata.deadline().is_none());
        assert!(
            this.metadata_read
                .prepare(&mut this.metadata, true, Instant::now())
                .is_none()
        );
    });
}

#[gpui::test]
fn same_track_detail_expiry_preserves_the_source_until_identity_replacement(
    cx: &mut TestAppContext,
) {
    use std::time::Duration;

    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        this.metadata.select(1);
        let original = this.track_snapshot().unwrap();
        let start = Instant::now();
        let query = Query::Track {
            track_id: original.id,
        };
        this.metadata_read
            .request(original.id, this.state.query_revisions_for(&query), start);
        let _quiet_reader = this
            .metadata_read
            .prepare(&mut this.metadata, true, start)
            .unwrap();
        assert!(Arc::ptr_eq(&this.track_snapshot().unwrap(), &original));
        assert!(
            this.metadata
                .display_track_at(start + Duration::from_millis(199))
                .is_some()
        );
        assert!(
            this.metadata
                .expire(start + rivu_presentation::refresh::REFRESH_RETENTION)
        );
        assert!(this.metadata.track().is_none());
        assert_eq!(this.track(), Some(original.as_ref()));
        assert!(Arc::ptr_eq(&this.track_snapshot().unwrap(), &original));
        assert!(!this.metadata.editable());
        assert!(
            this.metadata
                .begin_save(&SavedValue::Title("stale write".into()))
                .is_none()
        );
        assert!(
            this.metadata
                .begin_source(source::SourceAction::Sync)
                .is_none()
        );
        this.metadata.invalidate_at(start + Duration::from_secs(1));
        assert!(
            this.metadata.track().is_none(),
            "a retry cannot restart expired detail retention"
        );
        assert!(this.metadata.deadline().is_none());
        let replacement = this.metadata.begin_load(original.id);
        assert!(
            this.metadata
                .accept_load(replacement, Some(track(original.id, "fresh details")))
        );
        assert_eq!(this.metadata.track().unwrap().title, "fresh details");
        assert_eq!(this.track().unwrap().title, "fresh details");
        assert!(this.metadata.editable());
        assert!(!Arc::ptr_eq(&this.track_snapshot().unwrap(), &original));
        this.metadata.select(2);
        this.schedule_metadata_expiry(cx);
        assert!(this.track().is_none());
        assert!(this.track_snapshot().is_none());
    });
}
