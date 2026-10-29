use super::*;
use crate::config::{Config, ConfigPersistence};
use crate::gui::layout::interaction::{DragCommit, LayoutInteraction, SeekCommit};
use gpui::{AppContext, Bounds, Context, point, px, size};
use rivu_audio::WaveformFrame;
use rivu_format::decoder_hint::DecoderHint;
use rivu_protocol::model::Track;
use std::{path::PathBuf, sync::atomic::Ordering};

fn frame(peaks: Vec<Option<f32>>) -> WaveformFrame {
    let counts: Vec<_> = peaks
        .iter()
        .map(|value| u64::from(value.is_some()))
        .collect();
    let mut frame = WaveformFrame::default();
    frame.revision = 1;
    frame.path = Some(PathBuf::from("track.flac"));
    frame.duration = Some(10.0);
    frame.span_seconds = 10.0;
    frame.bin_frames = 80_000;
    frame.bin_count = peaks.len() as u64;
    frame.identity = Some(rivu_audio::WaveformIdentity::FullScan {
        duration: Some(10.0),
        sample_rate: 8_000,
    });
    frame.max_peak = peaks.iter().flatten().copied().fold(1.0, f32::max);
    frame.bin_revisions = counts.clone();
    frame.rms_counts = counts.clone();
    frame.left_rms_counts = counts.clone();
    frame.right_rms_counts = counts;
    frame.rms = peaks.clone();
    frame.left_peaks = peaks.clone();
    frame.right_peaks = peaks.clone();
    frame.left_rms = peaks.clone();
    frame.right_rms = peaks.clone();
    frame.peaks = peaks;
    frame
}

fn track(path: &str, decoder_hint: DecoderHint) -> Track {
    Track {
        id: 1,
        path: PathBuf::from(path),
        fingerprint: None,
        cue: None,
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        duration: Some(10.0),
        codec: String::new(),
        channels: 2,
        sample_rate: 44100,
        bitrate_bps: None,
        bitrate_exact: false,
        track_number: None,
        disc_number: None,
        bits_per_sample: None,
        release_date: None,
        favorite: false,
        decoder_hint,
        missing: false,
        play_count: 0,
        last_played: None,
    }
}

fn captured_seek() -> (Waveform, LayoutInteraction) {
    let mut waveform = Waveform::test_preview(WaveformFrame::default());
    waveform.test_capture_seek(7);
    let mut owner = LayoutInteraction::default();
    owner.waveform_preview(10., Some(11), 7);
    (waveform, owner)
}

#[test]
fn collapsed_waveform_geometry_cannot_produce_a_seek() {
    let (mut waveform, mut owner) = captured_seek();
    let position = point(px(60.0), px(5.0));
    let bounds = Bounds::new(point(px(10.0), px(0.0)), size(px(100.0), px(20.0)));
    waveform.test_panel_bounds(1, bounds, 20.0);
    assert_eq!(waveform.test_seek_seconds(position, None), Some(10.0));
    for size in [size(px(0.0), px(20.0)), size(px(100.0), px(0.0))] {
        waveform.test_panel_bounds(1, Bounds::new(bounds.origin, size), 20.0);
        assert_eq!(waveform.test_seek_seconds(position, None), None);
    }
    let cancelled = waveform.test_remove_panel(1).unwrap();
    assert!(!waveform.accepts_seek_event(7));
    assert!(owner.cancel_waveform(cancelled));
    assert_eq!(owner.finish_waveform(20.0, 7), None);
    waveform.test_panel_bounds(1, bounds, 20.0);
    assert_eq!(waveform.test_seek_seconds(position, None), None);
    waveform.test_seek_duration(20.0);
    assert_eq!(waveform.test_seek_seconds(position, Some(1)), Some(10.0));
}

#[test]
fn hidden_and_unavailable_surfaces_cancel_both_owners_without_committing() {
    for hidden in [true, false] {
        let (mut waveform, mut owner) = captured_seek();
        waveform.test_panel_bounds(2, Bounds::default(), 40.0);
        assert_eq!(waveform.test_remove_panel(2), None);
        assert_eq!(waveform.test_retain_panels(|id| id == 1), None);
        assert!(waveform.accepts_seek_event(7));
        let cancelled = if hidden {
            waveform.test_retain_panels(|_| false)
        } else {
            // This is the same transition used by source/status/timeline early exits.
            waveform.test_remove_panel(1)
        }
        .unwrap();
        assert_eq!(cancelled, 7);
        assert!(!waveform.accepts_seek_event(7));
        assert!(!waveform.test_dragging());
        assert!(owner.cancel_waveform(cancelled));
        assert_eq!(owner.seek_preview(), None);
        assert_eq!(owner.finish_drag(), DragCommit::None);
        assert_eq!(owner.finish_waveform(12., 7), None);
        assert_eq!(waveform.test_remove_panel(1), None);
        assert_eq!(waveform.cancel_unreleased_seek(), None);
        assert!(owner.waveform_preview(20., Some(12), 8));
        assert!(!owner.cancel_waveform(cancelled));
        assert_eq!(
            owner.finish_waveform(21., 8),
            Some(SeekCommit {
                queue_id: 12,
                seconds: 21.
            })
        );
    }
}

#[test]
fn normal_release_keeps_queued_seek_but_missing_release_is_cancelled() {
    let (mut waveform, mut owner) = captured_seek();
    // Root release may run before the entity listener and must not commit it.
    assert_eq!(owner.finish_drag(), DragCommit::None);
    waveform.test_release_seek();
    assert_eq!(waveform.cancel_unreleased_seek(), None);
    assert!(waveform.accepts_seek_event(7));
    assert_eq!(
        owner.finish_waveform(12., 7),
        Some(SeekCommit {
            queue_id: 11,
            seconds: 12.
        })
    );
    assert_eq!(owner.finish_waveform(12., 7), None);
    let (mut waveform, mut owner) = captured_seek();
    let cancelled = waveform.cancel_unreleased_seek().unwrap();
    assert!(owner.cancel_waveform(cancelled));
    assert!(!waveform.accepts_seek_event(7));
    assert_eq!(owner.finish_waveform(12., 7), None);
}

#[gpui::test]
fn presentation_hiding_retains_pending_and_completed_full_previews(cx: &mut gpui::TestAppContext) {
    use crate::gui::presentation::{Presentation, PresentationState};

    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    Config {
        volume: 0.0,
        ffmpeg_enabled: false,
        mpris_enabled: false,
        ..Config::default()
    }
    .save(&config_path)
    .unwrap();
    let runtime = crate::core::Runtime::start(directory.path(), &config_path).unwrap();
    let config = runtime.handle.config_snapshot();
    let window = cx.add_empty_window();
    let waveform =
        window.update(|_, cx| cx.new(|_| Waveform::test_preview(frame(vec![Some(0.25), None]))));
    let owner = window.update(|_, cx| {
        cx.new(|cx| {
            Presentation::new(
                runtime.handle.clone(),
                Arc::new(Localizer::new("en-US")),
                waveform.clone(),
                cx.new(|_| crate::gui::animation_frames::AnimationFrames::default()),
                cx,
            )
        })
    });
    let track = track("track.flac", DecoderHint::Auto);
    let empty_layout = crate::gui::layout::Layout {
        root: None,
        next_id: 1,
        locked: false,
    };
    let mut layout = empty_layout.clone();
    let panel_id = layout.add("audio_visualizer", None);
    layout.set_audio_visualizer_options(
        panel_id,
        crate::config::AudioVisualizerOptions {
            style: crate::config::AudioVisualizerStyle::Waveform,
            grid: true,
            label: true,
        },
    );
    let mut panels = crate::gui::workspace::WorkspacePanels::default();
    window.update(|_window, cx| {
        let mut sync = |owner: &mut Presentation,
                        config: Arc<Config>,
                        visible,
                        has_panel,
                        cx: &mut Context<Presentation>| {
            panels.reconcile(if has_panel { &layout } else { &empty_layout });
            owner.sync(
                PresentationState {
                    config,
                    panels: &panels,
                    status: rivu_protocol::model::PlaybackStatus::Paused,
                    visible,
                    shutting_down: false,
                    current_track_id: Some(track.id),
                    waveform_track: Some(&track),
                    position: 0.0,
                    duration: track.duration,
                    source_generation: 0,
                    seek_revision: 0,
                },
                cx,
            );
        };
        owner.update(cx, |owner, cx| {
            sync(owner, Arc::clone(&config), true, true, cx)
        });
        for complete in [false, true] {
            let (generation, cancelled) = waveform.update(cx, |waveform, _| {
                waveform.test_start_full(DecoderHint::Auto)
            });
            if complete {
                waveform.update(cx, |waveform, _| {
                    assert!(waveform.test_finish_full(generation, frame(vec![Some(0.5); 2])));
                });
            }
            for (visible, has_panel, release) in [
                (true, false, false),
                (true, true, false),
                (false, true, false),
                (false, true, true),
                (true, true, false),
            ] {
                owner.update(cx, |owner, cx| {
                    sync(owner, Arc::clone(&config), visible, has_panel, cx);
                    if release {
                        owner.release_display_resources(cx);
                    }
                });
                assert!(!cancelled.load(Ordering::Relaxed));
                let waveform = waveform.read(cx);
                if complete {
                    assert_eq!(waveform.test_full_peaks().unwrap(), vec![Some(0.5); 2]);
                } else {
                    assert!(waveform.test_pending_full());
                }
            }
            if !complete {
                waveform.update(cx, |waveform, _| {
                    assert!(waveform.test_finish_full(generation, frame(vec![Some(0.5); 2])));
                });
            }
        }
        let mut still = config.as_ref().clone();
        still.still_mode = true;
        owner.update(cx, |owner, cx| sync(owner, Arc::new(still), true, true, cx));
        assert!(waveform.read(cx).test_full_peaks().is_none());
        assert!(!waveform.read(cx).test_pending_full());
    });
}

#[test]
fn full_query_identity_distinguishes_content_but_preserves_metadata_edits() {
    for (before, after) in [
        (Some("old"), Some("new")),
        (None, Some("new")),
        (Some("old"), None),
    ] {
        let mut original = track("track.flac", DecoderHint::Auto);
        original.fingerprint = before.map(str::to_owned);
        let mut query = crate::gui::FullWaveformRequest::default();
        let generation = query.begin(&original);
        let mut replaced = original.clone();
        replaced.fingerprint = after.map(str::to_owned);
        assert!(!query.accepts(generation, Some(&replaced)));
        query.retain_target(Some(&replaced), false);
        assert!(!query.accepts(generation, Some(&original)));
    }
    let mut original = track("track.flac", DecoderHint::Auto);
    original.fingerprint = Some("same-content".into());
    let mut query = crate::gui::FullWaveformRequest::default();
    let generation = query.begin(&original);
    let mut edited = original.clone();
    edited.title = "renamed".into();
    edited.favorite = true;
    edited.play_count += 1;
    edited.decoder_hint = DecoderHint::Ffmpeg;
    query.retain_target(Some(&edited), false);
    assert!(query.accepts(generation, Some(&edited)));
    query.saved_hint_changed(&edited);
    assert!(!query.accepts(generation, Some(&original)));
}
