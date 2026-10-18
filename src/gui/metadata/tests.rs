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
