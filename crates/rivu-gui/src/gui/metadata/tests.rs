use super::*;
use crate::test_support::ConfigFixture;
use crate::test_support::CoreRuntime;
use gpui::{EntityInputHandler, TestAppContext};
use rivu_format::config::Config;
use rivu_presentation::i18n::Localizer;
use rivu_protocol::response::Ack;

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
        favorite: false,
        decoder_hint: DecoderHint::Auto,
        missing: false,
        play_count: 0,
        last_played: None,
    }
}

#[test]
fn source_support_accepts_decoder_names_and_profiles() {
    for (extension, codec, supported) in [
        ("wav", "ADPCM_MS", true),
        ("wav", "ADPCM_IMA_WAV", true),
        ("aiff", "ADPCM_IMA_QT", true),
        ("aac", "aac (LC)", true),
        ("m4a", "aac (HE-AAC)", true),
        ("flac", "dts (DTS-HD MA)", false),
        ("m4a", "opus", false),
    ] {
        let mut target = track(1, "track");
        target.path = format!("/track.{extension}").into();
        target.codec = codec.into();
        assert_eq!(source::source_support(Some(&target)).is_ok(), supported);
    }
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
    cx.update(|cx| {
        crate::gui::appearance::theme::set_theme(rivu_presentation::theme::Theme::default(), cx);
        crate::gui::appearance::fonts::publish(&gpui::font("sans-serif"), 12, cx);
    });
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    Config {
        volume: 0.0,
        ffmpeg_enabled: false,
        mpris_enabled: false,
        ..Config::default()
    }
    .save_fixture(&path)
    .unwrap();
    let runtime = CoreRuntime::start(directory.path(), &path).unwrap();
    let i18n = Arc::new(Localizer::new("en-US"));
    let context = FeatureContext {
        snapshots: Rc::new(crate::gui::feature::SnapshotSource::new(
            runtime.handle.clone(),
        )),
        labels: Rc::new(
            i18n.fixed_texts()
                .map(|(id, text)| (id.to_owned(), SharedString::from(text.to_owned())))
                .collect(),
        ),
        i18n: i18n.clone(),
        scroll: Default::default(),
    };
    let default_artwork = cx.new(|cx| DefaultArtwork::new(&i18n, cx));
    let metadata = cx.new(|cx| {
        Metadata::new(
            context,
            gpui::font("sans-serif"),
            Arc::new(ArtworkManager::new(directory.path())),
            ArtworkImageRegistry::default(),
            default_artwork,
            cx,
        )
    });
    metadata.update(cx, |this, cx| {
        playback(this, track(1, "A"), cx);
    });
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
fn hidden_playback_advance_with_bitrate_change_prepares_current_identity(cx: &mut TestAppContext) {
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
        assert_eq!(this.metadata.track().unwrap().title, "B");
        assert!(this.edit.is_none());
        // Exercise reveal's demand preparation without a real Core read or timer.
        let task = this
            .metadata_read
            .prepare(&mut this.metadata, true, Instant::now())
            .unwrap();
        assert_eq!(task.read.query(), Query::Track { track_id: 2 });
        assert!(this.metadata_read.submit(
            task.read,
            &this.metadata,
            ReadEnvironment {
                visible: true,
                revisions: this.state.query_revisions_for(&task.read.query()),
            },
        ));
    });
}

#[gpui::test]
fn playback_and_explicit_selection_own_target_not_read_completion(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        let old_follow = this.metadata.begin_load(1);
        playback(this, track(2, "B"), cx);
        assert_eq!(this.metadata.identity(), Some(2));
        assert!(
            !this
                .metadata
                .accept_load(old_follow, Some(track(1, "late A")))
        );
        assert_eq!(this.metadata.track().unwrap().title, "B");

        artwork_selection(this, track(3, "selected C"), cx);
        let old_selection = this.metadata.begin_load(3);
        playback(this, track(4, "D"), cx);
        assert_eq!(this.metadata.selected(), Some(3));
        assert_eq!(this.metadata.track().unwrap().title, "selected C");

        this.clear_selection(cx);
        assert_eq!(this.metadata.identity(), Some(4));
        assert_eq!(this.metadata.track().unwrap().title, "D");
        assert!(
            !this
                .metadata
                .accept_load(old_selection, Some(track(3, "late C")))
        );
        assert_eq!(this.metadata.identity(), Some(4));

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
    crate::frontend::CommandDelivery {
        ack: Ack {
            status: if ok {
                rivu_protocol::response::AckStatus::Succeeded
            } else {
                rivu_protocol::response::AckStatus::Failed
            },
            error: (!ok).then(|| "save failed".into()),
            revision: 0,
            scan_id: None,
            relocation_id: None,
            processing_id: None,
            database_optimization: None,
            source_tags: None,
            source_file_saved: false,
        },
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
    }
}

fn source_ack(ok: bool, file_saved: bool) -> crate::frontend::CommandDelivery {
    let mut delivery = ack(ok);
    delivery.ack.source_tags = Some(file_tags());
    delivery.ack.source_file_saved = file_saved;
    delivery
}

#[test]
fn bulk_source_commands_include_six_staged_values_and_database_expectation() {
    use source::{SourceAction, database_tags};
    let mut session = MetadataSession::default();
    let mut original = track(1, "database title");
    original.play_count = 42;
    original.favorite = true;
    original.decoder_hint = DecoderHint::Native;
    session.sync_playback(Some(&original));
    session.source.stage(file_tags());
    assert_eq!(session.track(), Some(&original));
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
    assert_eq!(session.track(), Some(&original));
}

#[test]
fn source_capability_uses_container_and_codec_hints_without_file_access() {
    let mut target = track(1, "supported");
    target.path = "/does/not/exist/test.flac".into();
    target.codec = "FLAC".into();
    assert_eq!(source::source_support(Some(&target)), Ok(()));
    target.codec = "dts".into();
    assert_eq!(
        source::source_support(Some(&target)),
        Err(source::SupportReason::Unsupported)
    );
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
fn file_load_stages_six_values_even_when_observation_is_unavailable(cx: &mut TestAppContext) {
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
        let read = this.metadata.begin_load(1);
        assert!(this.metadata.accept_load(read, Some(refreshed)));
        let mut expected = file_tags();
        expected.title = "edited title".into();
        assert_eq!(this.metadata.source.values(this.metadata.track().unwrap()), expected);
        let (count, _) = this.metadata.begin_save(&SavedValue::PlayCount(4)).unwrap();
        this.finish_save(count, None, ack(true), cx);
        assert_eq!(this.metadata.source.values(this.metadata.track().unwrap()), expected);
    });
}

#[gpui::test]
fn physical_save_failure_retains_values_and_only_database_sync_can_retry(cx: &mut TestAppContext) {
    use source::SourceAction;
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        let original = this.metadata.track().unwrap().clone();
        this.metadata.source.stage(file_tags());
        let (token, _) = this.metadata.begin_source(SourceAction::Save).unwrap();
        let mut delivery = source_ack(false, true);
        delivery.observation = crate::frontend::CommandObservation::Unavailable("observation also failed".into());
        this.finish_source_action(token, delivery, cx);
        assert_eq!(this.metadata.track(), Some(&original));
        assert_eq!(this.metadata.source.values(&original), file_tags());
        assert!(this.metadata.source.sync_only);
        assert!(matches!(&this.metadata.source.status, Some(source::SourceStatus::FileSavedDatabaseFailed(error)) if error == "save failed"));
        assert!(this.metadata.begin_source(SourceAction::Save).is_none());
        let (retry, command) = this.metadata.begin_source(SourceAction::Sync).unwrap();
        assert!(matches!(command, Command::SyncTrackTags { tags, expected, .. } if tags == file_tags() && *expected == source::database_tags(&original)));
        this.finish_source_action(retry, source_ack(true, false), cx);
        assert!(!this.metadata.source.sync_only);
        assert_eq!(source::database_tags(this.metadata.track().unwrap()), file_tags());
        assert_eq!(this.metadata.source.status, Some(source::SourceStatus::Synced));
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
        let mut delivery = source_ack(true, true);
        delivery.observation =
            crate::frontend::CommandObservation::Unavailable("refresh unavailable".into());
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
fn newer_database_read_and_inline_input_survive_old_bulk_completion(cx: &mut TestAppContext) {
    use source::SourceAction;
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    cx.update_window(window, |_, window, cx| {
        metadata.update(cx, |this, cx| {
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
            this.finish_save(save, Some((serial, "submitted".into())), ack(true), cx);
            let edit = this.edit.as_ref().unwrap();
            assert_eq!(edit.input.read(cx).text(), "later");
            assert_eq!(edit.baseline, "submitted");
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
        assert!(this.artwork_view.image(cx).is_some());

        this.set_visible(false, cx);
        assert!(this.artwork_view.image(cx).is_none());
        this.prepare_artwork(cx);
        assert!(this.artwork_view.image(cx).is_none());

        this.set_visible(true, cx);
        this.prepare_artwork(cx);
        assert!(this.artwork_view.image(cx).is_some());
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
        assert!(this.artwork_view.image(cx).is_none());
        assert!(this.artwork_loader.prepare(&selected).is_none());
        this.set_visible(false, cx);
    });
    // A completed miss is not a new acquisition demand on every render/reopen.
    rivu_metadata::artwork::test_support::tagged_wav(&selected.path);
    metadata.update(cx, |this, cx| {
        this.set_visible(true, cx);
        this.prepare_artwork(cx);
        assert!(this.artwork_view.image(cx).is_none());
    });
    cx.run_until_parked();
    metadata.update(cx, |this, cx| {
        assert_eq!(
            this.artwork_manager
                .cached_result(crate::artwork_source(&selected)),
            Some(None)
        );
        assert!(this.artwork_view.image(cx).is_none());
        this.prepare_artwork(cx);
        assert!(this.artwork_loader.prepare(&selected).is_none());
        assert!(this.artwork_view.image(cx).is_none());
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
        assert!(this.artwork_view.image(cx).is_none());
        // Hiding releases the negative identity. Revealing cold demand may probe again.
        this.set_visible(false, cx);
        this.set_visible(true, cx);
        this.prepare_artwork(cx);
    });
    cx.run_until_parked();
    metadata.update(cx, |this, cx| {
        assert!(this.artwork_view.image(cx).is_some());
    });
}

#[gpui::test]
fn cached_relocation_is_delivered_while_old_artwork_is_in_flight(cx: &mut TestAppContext) {
    let (directory, _runtime, metadata) = fixture(cx);
    for (name, colour) in [("before", [255, 0, 0]), ("after", [0, 255, 0])] {
        let path = directory.path().join(name);
        std::fs::create_dir(&path).unwrap();
        image::RgbImage::from_pixel(2, 2, image::Rgb(colour))
            .save(path.join("cover.png"))
            .unwrap();
    }
    let mut first = track(7, "Cover");
    first.path = directory.path().join("before/track.wav");
    let mut latest = first.clone();
    latest.path = directory.path().join("after/track.wav");
    let expected = metadata.update(cx, |this, cx| {
        artwork_selection(this, first.clone(), cx);
        this.visible = true;
        this.prepare_artwork(cx);
        assert!(this.artwork_loader.prepare(&latest).is_none());
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
        assert!(this.artwork_loader.prepare(&latest).is_some());
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
        assert!(this.artwork_view.image(cx).is_none());
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
        assert!(this.artwork_view.image(cx).is_none());
        assert_eq!(this.metadata.identity(), Some(8));
        this.set_visible(true, cx);
        this.prepare_artwork(cx);
    });
    cx.run_until_parked();
    metadata.update(cx, |this, cx| {
        assert!(
            this.artwork_manager
                .cached_result(crate::artwork_source(&replacement))
                .is_some()
        );
        assert!(this.artwork_view.image(cx).is_some());
        assert_eq!(this.metadata.identity(), Some(8));
    });
}
