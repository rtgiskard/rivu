use super::*;
use crate::{config::Config, core::Runtime, i18n::Localizer, response::Ack};
use gpui::{EntityInputHandler, TestAppContext};

fn track(id: i64, title: &str) -> Track {
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

fn fixture(cx: &mut TestAppContext) -> (tempfile::TempDir, Runtime, Entity<Metadata>) {
    cx.update(|cx| {
        crate::gui::appearance::theme::set_theme(crate::theme::Theme::default(), cx);
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
    .save(&path)
    .unwrap();
    let runtime = Runtime::start(directory.path(), &path).unwrap();
    let i18n = Arc::new(Localizer::new("en-US"));
    let context = FeatureContext {
        snapshots: Rc::new(crate::gui::snapshot::SnapshotSource::new(
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
    let default_album = cx.new(|cx| Artwork::new(&i18n, cx));
    let metadata = cx.new(|cx| {
        Metadata::new(
            context,
            Arc::new(ArtworkManager::new(directory.path())),
            ArtworkImages::default(),
            default_album,
            cx,
        )
    });
    metadata.update(cx, |this, cx| {
        // Exercise the feature's event boundary with no pending root manual intent.
        // Confirmation arbitration remains the root's responsibility.
        this._subscriptions.push(cx.subscribe(
            &cx.entity(),
            |this, _, event: &MetadataEvent, cx| {
                if matches!(
                    event,
                    MetadataEvent::ResumeManualAction {
                        resume_follow: true
                    }
                ) {
                    this.resume_follow(cx);
                }
            },
        ));
        playback(this, track(1, "A"), cx);
    });
    (directory, runtime, metadata)
}

fn playback(owner: &mut Metadata, track: Track, cx: &mut Context<Metadata>) {
    let mut state = (*owner.state).clone();
    state.current_track = Some(Arc::new(track));
    owner.reconcile(Rc::new(state), cx);
}

fn edit_text(
    metadata: &Entity<Metadata>,
    field: Field,
    value: &str,
    window: gpui::AnyWindowHandle,
    cx: &mut TestAppContext,
) {
    let input = metadata.read_with(cx, |this, _| this.inputs[&field].clone());
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
        ok,
        error: (!ok).then(|| "save failed".into()),
        revision: 0,
        scan_id: None,
        relocation_id: None,
    }
}

#[gpui::test]
fn restoring_each_text_input_resumes_playback_without_another_snapshot(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    let window = cx
        .add_empty_window()
        .update(|window, _| window.window_handle());
    for (field, original) in [
        (Field::Title, "A"),
        (Field::Artist, "artist"),
        (Field::Album, "album"),
    ] {
        metadata.update(cx, |this, cx| playback(this, track(1, "A"), cx));
        edit_text(&metadata, field, "draft", window, cx);
        metadata.update(cx, |this, cx| {
            playback(this, track(2, "B"), cx);
            assert_eq!(this.metadata.identity(), Some(1));
            assert!(this.dirty(cx));
        });
        edit_text(&metadata, field, original, window, cx);
        metadata.update(cx, |this, cx| {
            assert_eq!(this.metadata.identity(), Some(2));
            assert_eq!(this.inputs[&Field::Title].read(cx).text(), "B");
            assert!(!this.protected(cx));
        });
    }
}

#[gpui::test]
fn decoder_restoration_reconciles_details_but_keeps_explicit_selection(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        this.set_decoder_hint(DecoderHint::Native, cx);
        playback(this, track(2, "B"), cx);
        assert_eq!(this.metadata.identity(), Some(1));
        this.set_decoder_hint(DecoderHint::Auto, cx);
    });
    cx.run_until_parked();
    metadata.update(cx, |this, cx| {
        assert_eq!(this.metadata.identity(), Some(2));
        playback(this, track(1, "A"), cx);
        this.metadata.select(1);
        this.set_decoder_hint(DecoderHint::Native, cx);
        let token = this.metadata.begin_load(1);
        let draft = this.draft(cx);
        assert!(
            this.metadata
                .accept_load(token, Some(track(1, "remote")), draft)
                .is_none()
        );
        playback(this, track(2, "B"), cx);
        this.set_decoder_hint(DecoderHint::Auto, cx);
    });
    cx.run_until_parked();
    metadata.update(cx, |this, cx| {
        assert_eq!(this.metadata.identity(), Some(1));
        assert_eq!(this.metadata.selected(), Some(1));
        assert_eq!(this.inputs[&Field::Title].read(cx).text(), "remote");
        assert!(!this.protected(cx));
    });
}

#[gpui::test]
fn hidden_save_completion_releases_only_clean_drafts_and_rejects_old_epochs(
    cx: &mut TestAppContext,
) {
    let (_directory, _runtime, metadata) = fixture(cx);
    for ok in [false, true] {
        let (token, values) = metadata.update(cx, |this, cx| {
            playback(this, track(1, "A"), cx);
            this.visible = true;
            this.set_decoder_hint(DecoderHint::Native, cx);
            let values = MetadataValues {
                title: "A".into(),
                artist: "artist".into(),
                album: "album".into(),
                decoder_hint: DecoderHint::Native,
            };
            let (token, _) = this.metadata.begin_save(values.as_draft()).unwrap();
            playback(this, track(2, "B"), cx);
            // A failed write leaves the old baseline; restoration during the write
            // is clean but must remain pinned until its physical completion.
            if !ok {
                this.set_decoder_hint(DecoderHint::Auto, cx);
            }
            this.set_visible(false, cx);
            this.cancel_read();
            assert!(this.saving());
            assert_eq!(this.metadata.identity(), Some(1));
            (token, values)
        });
        cx.run_until_parked();
        metadata.update(cx, |this, cx| {
            assert_eq!(this.metadata.identity(), Some(1));
            this.finish_save(token, values, None, ack(ok), cx);
        });
        cx.run_until_parked();
        metadata.update(cx, |this, cx| {
            assert_eq!(this.metadata.identity(), Some(2));
            assert!(!this.saving());
            assert_eq!(this.inputs[&Field::Title].read(cx).text(), "B");
        });
    }
    let (token, values) = metadata.update(cx, |this, cx| {
        playback(this, track(1, "A"), cx);
        this.set_decoder_hint(DecoderHint::Native, cx);
        let values = MetadataValues {
            title: "A".into(),
            artist: "artist".into(),
            album: "album".into(),
            decoder_hint: DecoderHint::Native,
        };
        let (token, _) = this.metadata.begin_save(values.as_draft()).unwrap();
        this.set_decoder_hint(DecoderHint::Ffmpeg, cx);
        playback(this, track(2, "B"), cx);
        this.finish_save(token, values.clone(), None, ack(true), cx);
        (token, values)
    });
    cx.run_until_parked();
    metadata.update(cx, |this, cx| {
        assert_eq!(this.metadata.identity(), Some(1));
        assert_eq!(this.metadata_decoder_hint, DecoderHint::Ffmpeg);
        assert!(this.dirty(cx));
        this.discard(cx);
        this.clear_selection(cx);
        this.finish_save(token, values, None, ack(true), cx);
        assert_eq!(this.metadata.identity(), Some(2));
        assert_eq!(this.metadata_decoder_hint, DecoderHint::Auto);
    });
}

#[gpui::test]
fn save_ack_preserves_newer_details_and_refreshes_an_unchanged_projection(cx: &mut TestAppContext) {
    let (_directory, _runtime, metadata) = fixture(cx);
    metadata.update(cx, |this, cx| {
        artwork_selection(this, track(7, "original"), cx);
        let submitted = MetadataValues {
            title: "submitted".into(),
            artist: "submitted artist".into(),
            album: "submitted album".into(),
            decoder_hint: DecoderHint::Ffmpeg,
        };
        this.set_metadata_values(submitted.clone(), cx);
        let (save, _) = this.metadata.begin_save(submitted.as_draft()).unwrap();
        let read = this.metadata.begin_load(7);
        let mut newer = track(7, "newer");
        newer.artist = "newer artist".into();
        newer.album = "newer album".into();
        newer.decoder_hint = DecoderHint::Native;
        assert!(
            this.metadata
                .accept_load(read, Some(newer), submitted.as_draft())
                .is_none()
        );
        this.finish_save(save, submitted, None, ack(true), cx);
        assert_eq!(this.metadata.track().unwrap().title, "newer");
        assert_eq!(this.inputs[&Field::Title].read(cx).text(), "newer");
        assert_eq!(this.inputs[&Field::Artist].read(cx).text(), "newer artist");
        assert_eq!(this.inputs[&Field::Album].read(cx).text(), "newer album");
        assert_eq!(this.metadata_decoder_hint, DecoderHint::Native);
        assert!(!this.dirty(cx));
        // No publication changed: the acknowledgement itself must demand a read.
        let now = Instant::now();
        let task = this
            .metadata_read
            .prepare(&mut this.metadata, true, now)
            .expect("the accepted save must refresh authoritative details");
        assert!(matches!(task.read.query(), Query::Track { track_id: 7 }));
        assert!(
            this.metadata_read
                .prepare(&mut this.metadata, true, now)
                .is_none()
        );
    });
}

fn artwork_selection(owner: &mut Metadata, track: Track, cx: &mut Context<Metadata>) {
    owner.metadata.select(track.id);
    let token = owner.metadata.begin_load(track.id);
    let draft = owner.draft(cx);
    let values = owner
        .metadata
        .accept_load(token, Some(track), draft)
        .unwrap();
    owner.set_metadata_values(values, cx);
}

#[gpui::test]
fn hiding_details_releases_cached_artwork_until_revealed(cx: &mut TestAppContext) {
    let (directory, _runtime, metadata) = fixture(cx);
    let path = directory.path().join("cover.wav");
    crate::artwork::tests::tagged_wav(&path);
    let mut selected = track(7, "Cover");
    selected.path = path;
    metadata.update(cx, |this, cx| {
        assert!(
            !this
                .artwork_manager
                .load(&selected)
                .unwrap()
                .is_placeholder()
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
fn hidden_artwork_completion_retires_without_loading_replacement(cx: &mut TestAppContext) {
    let (directory, _runtime, metadata) = fixture(cx);
    let mut first = track(7, "First");
    first.path = directory.path().join("first.wav");
    let mut replacement = track(8, "Replacement");
    replacement.path = directory.path().join("replacement.wav");
    crate::artwork::tests::tagged_wav(&first.path);
    crate::artwork::tests::tagged_wav(&replacement.path);
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
        assert!(this.artwork_manager.is_loaded(&first));
        assert!(!this.artwork_manager.is_loaded(&replacement));
        assert!(this.artwork_view.image(cx).is_none());
        assert_eq!(this.metadata.identity(), Some(8));
        this.set_visible(true, cx);
        this.prepare_artwork(cx);
    });
    cx.run_until_parked();
    metadata.update(cx, |this, cx| {
        assert!(this.artwork_manager.is_loaded(&replacement));
        assert!(this.artwork_view.image(cx).is_some());
        assert_eq!(this.metadata.identity(), Some(8));
    });
}
