pub mod catalog;
pub mod frontend;
pub mod ipc;

#[cfg(test)]
pub(crate) fn write_i16_wav(
    path: &std::path::Path,
    channels: u16,
    sample_rate: u32,
    samples: impl IntoIterator<Item = i16>,
) -> hound::Result<()> {
    let mut writer = hound::WavWriter::create(
        path,
        hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )?;
    for sample in samples {
        writer.write_sample(sample)?;
    }
    writer.finalize()
}

#[cfg(test)]
pub(crate) fn write_float_wav(
    path: &std::path::Path,
    channels: u16,
    sample_rate: u32,
    samples: impl IntoIterator<Item = f32>,
) -> hound::Result<()> {
    let mut writer = hound::WavWriter::create(
        path,
        hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )?;
    for sample in samples {
        writer.write_sample(sample)?;
    }
    writer.finalize()
}

// Mono PCM with exactly 100 samples per CD frame, preserving CUE boundaries.
#[cfg(test)]
pub(crate) fn write_cd_frame_wav(path: &std::path::Path, frames: u32) -> hound::Result<()> {
    write_i16_wav(
        path,
        1,
        7_500,
        std::iter::repeat_n(0, frames as usize * 100),
    )
}

/// Re-run a potentially blocking regression in a killable process, not a thread
/// whose destructor could join the same stuck worker during unwinding.
pub fn run_in_subprocess(test: &str, timeout: std::time::Duration) -> bool {
    const CHILD_TEST: &str = "RIVU_BOUNDED_TEST";
    if std::env::var(CHILD_TEST).as_deref() == Ok(test) {
        return false;
    }
    struct Runner {
        child: std::process::Child,
        reaped: bool,
    }
    impl Drop for Runner {
        fn drop(&mut self) {
            if !self.reaped {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
    let mut runner = Runner {
        child: std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD_TEST, test)
            .spawn()
            .unwrap(),
        reaped: false,
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = runner.child.try_wait().unwrap() {
            runner.reaped = true;
            assert!(status.success(), "{test} failed: {status}");
            return true;
        }
        if std::time::Instant::now() >= deadline {
            panic!("{test} exceeded {timeout:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(test)]
use rivu_metadata::artwork::test_support::{PNG_IMAGE, budget_unsynchronised_wav, pictures_wav};
#[cfg(test)]
use rivu_protocol::model::Track;
#[cfg(test)]
use std::path::{Path, PathBuf};

#[cfg(test)]
pub(crate) fn artwork_track(path: PathBuf) -> Track {
    let fingerprint = path
        .is_file()
        .then(|| std::fs::read(&path).unwrap())
        .map(|bytes| blake3::hash(&bytes).to_hex().to_string());
    Track {
        id: 1,
        path,
        fingerprint,
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
