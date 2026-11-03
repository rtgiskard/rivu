pub mod catalog;
pub mod frontend;
pub mod ipc;

#[cfg(test)]
use rivu_metadata::artwork::test_support::{budget_unsynchronised_wav, pictures_wav};
#[cfg(test)]
use rivu_presentation::DEFAULT_IMAGE;
#[cfg(test)]
use rivu_protocol::model::Track;
#[cfg(test)]
use std::path::{Path, PathBuf};

#[cfg(test)]
pub(crate) fn artwork_track(path: PathBuf) -> Track {
    Track {
        id: 1,
        path,
        fingerprint: Some("original".into()),
        cue: None,
        title: String::new(),
        artist: String::new(),
        album: String::new(),
        duration: None,
        codec: "wav".into(),
        channels: 1,
        sample_rate: 8_000,
        bitrate_bps: None,
        bitrate_exact: false,
        track_number: None,
        disc_number: None,
        bits_per_sample: Some(16),
        release_date: None,
        favorite: false,
        decoder_hint: Default::default(),
        missing: false,
        play_count: 0,
        last_played: None,
    }
}

#[cfg(test)]
fn assert_budget_audio(path: &Path) {
    use rivu_audio::{AudioBackend, PcmBudget, SourceRenderer};
    use rivu_format::decoder_hint::DecoderHint;
    use std::sync::atomic::AtomicBool;

    let details = rivu_audio::probe_details(path, false, false).unwrap();
    assert!(matches!(details.decoder_backend, AudioBackend::Native));
    assert_eq!(details.info.sample_rate, 8_000);
    let rendered = SourceRenderer::open(
        path,
        None,
        DecoderHint::Native,
        2,
        false,
        &AtomicBool::new(false),
    )
    .unwrap()
    .render_pcm(PcmBudget::new(128 * 1024), &AtomicBool::new(false))
    .unwrap();
    assert_eq!(rendered.frames(), 1);
    let mut samples = [0.0];
    assert_eq!(rendered.read_frames(0, &mut samples), 1);
    assert_eq!(samples, [0.5]);
}

#[test]
fn native_metadata_budget_preserves_unsynchronised_audio() {
    let directory = tempfile::tempdir().unwrap();
    for (version, tag_wide) in [(2, true), (3, true), (4, false), (4, true)] {
        let path = directory
            .path()
            .join(format!("unsync-{version}-{tag_wide}.wav"));
        budget_unsynchronised_wav(&path, version, tag_wide);
        assert_budget_audio(&path);
    }
}

#[test]
fn native_metadata_budget_skips_large_pictures_without_losing_tags_or_audio() {
    let directory = tempfile::tempdir().unwrap();
    for version in [2, 3, 4] {
        let path = directory.path().join(format!("budget-{version}.wav"));
        let size = if version == 2 { 9 } else { 27 } * 1024 * 1024;
        pictures_wav(
            &path,
            version,
            &[
                (DEFAULT_IMAGE, size, 3),
                (DEFAULT_IMAGE, DEFAULT_IMAGE.len(), 3),
            ],
        );
        assert_eq!(
            rivu_audio::probe(&path, false).unwrap().title,
            "Budget title"
        );
        assert_budget_audio(&path);
    }
}

#[test]
fn native_metadata_budget_keeps_audio_after_many_picture_frames() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("many-pictures.wav");
    let pictures = vec![(DEFAULT_IMAGE, DEFAULT_IMAGE.len(), 3); 64];
    pictures_wav(&path, 4, &pictures);
    assert_eq!(
        rivu_audio::probe(&path, false).unwrap().title,
        "Budget title"
    );
    assert_budget_audio(&path);
}
