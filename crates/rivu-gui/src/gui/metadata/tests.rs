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

fn ack(ok: bool) -> Ack {
    Ack {
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
    }
}

#[gpui::test]
fn selection_and_blur_cancel_without_submitting(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    for field in [Field::Title, Field::Artist, Field::Album, Field::PlayCount] {
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
            this.finish_save(
                save,
                Some((serial, "submitted".into())),
                None,
                ack(false),
                cx,
            );
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
            this.finish_save(
                save,
                Some((serial, "submitted".into())),
                None,
                ack(false),
                cx,
            );
            assert_eq!(this.edit.as_ref().unwrap().field, Field::Artist);
            assert!(this.edit.as_ref().unwrap().error.is_none());
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
        this.finish_save(save, None, None, ack(true), cx);
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
        this.finish_save(save, None, None, ack(false), cx);
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
