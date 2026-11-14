use std::{
    ffi::OsString,
    fs,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn tags(path: &Path, directory: &Path, edit: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rivu"));
    command
        .arg("--json")
        .arg("--data-dir")
        .arg(directory.join("no-server"))
        .arg("--config")
        .arg(directory.join("invalid.toml"))
        .args(["library", "tags"])
        .arg(path);
    if edit {
        command.args(["--title", "Edited title"]);
    }
    command.output().unwrap()
}

fn success_json(output: Output) -> serde_json::Value {
    assert!(output.status.success(), "stderr: {:?}", output.stderr);
    assert!(output.stderr.is_empty(), "stderr: {:?}", output.stderr);
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_json_path(value: &serde_json::Value, path: &Path) {
    let recovered = if let Some(path) = path.to_str() {
        assert_eq!(value, path);
        PathBuf::from(value.as_str().unwrap())
    } else {
        let bytes: Vec<u8> = serde_json::from_value(value["unix_bytes"].clone()).unwrap();
        assert_eq!(bytes, path.as_os_str().as_bytes());
        PathBuf::from(OsString::from_vec(bytes))
    };
    assert_eq!(recovered, path);
}

#[test]
fn offline_tag_json_preserves_paths_and_replacement_facts() {
    for variant in ["utf8", "non_utf8_filename", "non_utf8_parent"] {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("invalid.toml"), "invalid config [").unwrap();
        let parent = if variant == "non_utf8_parent" {
            let parent = directory
                .path()
                .join(OsString::from_vec(b"parent-\xff".to_vec()));
            fs::create_dir(&parent).unwrap();
            parent
        } else {
            directory.path().to_owned()
        };
        let filename = if variant == "non_utf8_filename" {
            OsString::from_vec(b"track-\xff.wav".to_vec())
        } else {
            OsString::from("音楽.wav")
        };
        let path = parent.join(filename);
        let mut wav = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 8000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        wav.write_sample(0_i16).unwrap();
        wav.finalize().unwrap();
        let original = fs::read(&path).unwrap();
        let files_before_save = fs::read_dir(&parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<std::collections::BTreeSet<_>>();

        let read = success_json(tags(&path, directory.path(), false));
        assert_json_path(&read["path"], &path);
        assert_eq!(fs::read(&path).unwrap(), original);

        let replaced = success_json(tags(&path, directory.path(), true));
        assert_eq!(replaced["outcome"], "replaced");
        assert_json_path(&replaced["path"], &path);
        assert_eq!(
            fs::read_dir(&parent)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<std::collections::BTreeSet<_>>(),
            files_before_save
        );
        assert!(replaced.get("backup_path").is_none());
        let edited = fs::read(&path).unwrap();
        let read = success_json(tags(&path, directory.path(), false));
        assert_json_path(&read["path"], &path);
        assert_eq!(read["tags"]["title"], "Edited title");

        let files_before_unchanged = fs::read_dir(&parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<std::collections::BTreeSet<_>>();
        let unchanged = success_json(tags(&path, directory.path(), true));
        assert_eq!(unchanged["outcome"], "unchanged");
        assert_json_path(&unchanged["path"], &path);
        assert_eq!(fs::read(&path).unwrap(), edited);
        assert_eq!(
            fs::read_dir(&parent)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<std::collections::BTreeSet<_>>(),
            files_before_unchanged
        );

        fs::remove_file(&path).unwrap();
        let failure = tags(&path, directory.path(), true);
        assert_eq!(failure.status.code(), Some(1));
        assert!(failure.stdout.is_empty());
        let value: serde_json::Value = serde_json::from_slice(&failure.stderr).unwrap();
        assert_eq!(value["outcome"], "before_replace");
        assert_json_path(&value["path"], &path);
        assert!(value.get("backup_path").is_none());
        assert!(!directory.path().join("no-server").exists());
    }
}
