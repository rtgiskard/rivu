pub mod catalog;
pub mod frontend;
pub mod ipc;

// Mono PCM with exactly 100 samples per CD frame, preserving CUE boundaries.
#[cfg(test)]
pub(crate) fn write_cd_frame_wav(path: &std::path::Path, frames: u32) -> hound::Result<()> {
    rivu_audio::test_support::write_i16_wav(
        path,
        1,
        7_500,
        std::iter::repeat_n(0, frames as usize * 100),
    )
}

#[cfg(test)]
use rivu_metadata::artwork::test_support::{PNG_IMAGE, budget_unsynchronised_wav, pictures_wav};
#[cfg(test)]
use std::path::Path;

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
    let cache = Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache");
    std::fs::create_dir_all(&cache).unwrap();
    for version in [2, 3, 4] {
        let directory = tempfile::tempdir_in(&cache).unwrap();
        let path = directory.path().join(format!("budget-{version}.wav"));
        let size = if version == 2 { 9 } else { 27 } * 1024 * 1024;
        pictures_wav(
            &path,
            version,
            &[(PNG_IMAGE, size, 3), (PNG_IMAGE, PNG_IMAGE.len(), 3)],
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
    let pictures = vec![(PNG_IMAGE, PNG_IMAGE.len(), 3); 64];
    pictures_wav(&path, 4, &pictures);
    assert_eq!(
        rivu_audio::probe(&path, false).unwrap().title,
        "Budget title"
    );
    assert_budget_audio(&path);
}
