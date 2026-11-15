use rivu_protocol::model::Track;
use std::path::PathBuf;

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
        replaygain_track_gain: None,
        favorite: false,
        decoder_hint: Default::default(),
        missing: false,
        play_count: 0,
        last_played: None,
    }
}
