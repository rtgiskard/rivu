use super::*;
use std::os::unix::{
    fs::{MetadataExt, symlink},
    net::UnixListener,
};

fn fixture() -> (tempfile::TempDir, Options) {
    let directory = tempfile::tempdir().unwrap();
    let options = Options {
        data_dir: directory.path().to_path_buf(),
        config: directory.path().join("config.toml"),
        explicit_data_dir: true,
    };
    let connection = Connection::open(options.data_dir.join("library.db")).unwrap();
    database::create_current(&connection).unwrap();
    drop(connection);
    (directory, options)
}

fn execute(options: &Options, mode: Mode) -> Result<Report> {
    let mut report = Report::default();
    run(options, mode, &mut report)?;
    Ok(report)
}

fn revision(connection: &Connection) -> i64 {
    connection
        .query_row(
            "SELECT value FROM schema_meta WHERE key='album_trigger_revision'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn storage_estimate_rejects_unrepresentable_sizes() {
    assert!(database_storage_bytes(u64::MAX, 4096).is_err());
    assert!(database_storage_bytes(1, u64::MAX).is_err());
    assert!(database_storage_bytes(u64::MAX / 3, 1).is_err());
}

#[test]
fn storage_admission_accepts_exact_capacity_and_rejects_shortfall() {
    assert!(ensure_storage_capacity(Path::new("library.db"), 60, 60).is_ok());
    assert!(ensure_storage_capacity(Path::new("library.db"), 60, 59).is_err());
}

#[test]
fn preflight_errors_name_the_designated_target() {
    let (_directory, options) = fixture();
    fs::write(&options.config, "[broken").unwrap();
    let error = execute(&options, Mode::Check).err().unwrap();
    assert!(
        error
            .to_string()
            .contains(&options.config.display().to_string())
    );
    fs::remove_file(&options.config).unwrap();
    fs::write(options.data_dir.join("library.db"), "not SQLite").unwrap();
    let error = execute(&options, Mode::Check).err().unwrap();
    assert!(
        error
            .to_string()
            .contains(&options.data_dir.join("library.db").display().to_string())
    );
}

#[test]
fn explicit_paths_follow_host_precedence() {
    let options = Options::resolve(Some("chosen-data".into()), None);
    assert_eq!(options.config, PathBuf::from("chosen-data/config.toml"));
    let options = Options::resolve(Some("chosen-data".into()), Some("separate.toml".into()));
    assert_eq!(options.config, PathBuf::from("separate.toml"));
    assert_eq!(options.data_dir, PathBuf::from("chosen-data"));
}

#[test]
fn valid_configuration_is_read_only_with_or_without_version_and_through_symlinks() {
    for version in ["", "schema_version=1\n"] {
        for indirect in [false, true] {
            let (_directory, options) = fixture();
            let target = if indirect {
                let target = options.data_dir.join("actual.toml");
                symlink("actual.toml", &options.config).unwrap();
                target
            } else {
                options.config.clone()
            };
            let original =
                format!("# user formatting\n{version}[playback]\nvolume_percent=37 # retain\n");
            fs::write(&target, &original).unwrap();
            let inode = fs::metadata(&target).unwrap().ino();
            let initial_entries = fs::read_dir(&options.data_dir).unwrap().count();
            for mode in [Mode::Check, Mode::Apply] {
                let report = execute(&options, mode).unwrap();
                assert!(!report.needs_changes);
                assert!(report.backups.is_empty());
                assert_eq!(fs::read_to_string(&target).unwrap(), original);
                assert_eq!(fs::metadata(&target).unwrap().ino(), inode);
                assert_eq!(
                    fs::read_dir(&options.data_dir).unwrap().count(),
                    initial_entries
                );
                assert!(
                    report
                        .resources
                        .iter()
                        .any(|line| line.contains(&target.display().to_string()))
                );
            }
            if indirect {
                assert!(
                    fs::symlink_metadata(&options.config)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
            }
        }
    }
}

#[test]
fn configuration_byte_budget_matches_direct_format_validation_without_changes() {
    let (_directory, options) = fixture();
    for size in [0, config::MAX_CONFIG_BYTES, config::MAX_CONFIG_BYTES + 1] {
        let text = " ".repeat(size);
        fs::write(&options.config, &text).unwrap();
        let mut report = Report::default();
        let result = run(&options, Mode::Apply, &mut report);
        if size <= config::MAX_CONFIG_BYTES {
            result.unwrap();
        } else {
            let expected = config::from_toml(&text).unwrap_err();
            assert_eq!(
                result.unwrap_err().root_cause().to_string(),
                expected.to_string()
            );
        }
        assert!(report.backups.is_empty());
        assert_eq!(fs::read_to_string(&options.config).unwrap(), text);
    }
}

#[test]
fn missing_configuration_and_default_resources_are_not_created() {
    let (directory, options) = fixture();
    execute(&options, Mode::Apply).unwrap();
    assert!(!options.config.exists());
    let absent = Options {
        data_dir: directory.path().join("absent-data"),
        config: directory.path().join("absent-config/config.toml"),
        explicit_data_dir: false,
    };
    for mode in [Mode::Check, Mode::Apply] {
        execute(&absent, mode).unwrap();
        assert!(!absent.data_dir.exists());
        assert!(!absent.config.parent().unwrap().exists());
    }
}

#[test]
fn config_preflight_rejects_fifos_without_a_writer_including_symlinks() {
    use std::ffi::CString;

    let (directory, options) = fixture();
    let fifo = directory.path().join("fifo.toml");
    let name = CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: name is NUL terminated and mkfifo retains no pointer.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    symlink("fifo.toml", &options.config).unwrap();
    for path in [&fifo, &options.config] {
        let error = validate_config(path).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            ErrorKind::InvalidInput
        );
    }
    fs::remove_file(&fifo).unwrap();
    assert_eq!(validate_config(&options.config).unwrap(), None);
    assert!(!fifo.exists());
}

#[test]
fn explicitly_missing_database_fails_without_creating_it_or_changing_config() {
    let directory = tempfile::tempdir().unwrap();
    let options = Options {
        data_dir: directory.path().to_path_buf(),
        config: directory.path().join("config.toml"),
        explicit_data_dir: true,
    };
    fs::write(&options.config, "[playback]\n").unwrap();
    for mode in [Mode::Check, Mode::Apply] {
        let mut report = Report::default();
        assert!(run(&options, mode, &mut report).is_err());
        assert!(report.backups.is_empty());
        assert!(!options.data_dir.join("library.db").exists());
        assert_eq!(fs::read_to_string(&options.config).unwrap(), "[playback]\n");
    }
}

#[test]
fn unknown_future_and_invalid_configuration_fail_preflight() {
    let (_directory, options) = fixture();
    for text in [
        "schema_version=99\n",
        "schema_version=0\n",
        "schema_version='1'\n",
        "volume=0.5\n",
        "[playback]\nunknown=true\n",
        "[playback]\nvolume_percent=101\n",
        "[broken",
    ] {
        fs::write(&options.config, text).unwrap();
        let mut report = Report::default();
        assert!(run(&options, Mode::Apply, &mut report).is_err(), "{text}");
        assert!(report.backups.is_empty());
        assert_eq!(fs::read_to_string(&options.config).unwrap(), text);
    }
}

#[test]
fn unsupported_database_leaves_valid_unversioned_configuration_untouched() {
    for sql in [
        "UPDATE schema_meta SET value=99 WHERE key='version'",
        "UPDATE schema_meta SET value=99 WHERE key='album_trigger_revision'",
        "DELETE FROM schema_meta WHERE key='album_trigger_revision'",
        "UPDATE schema_meta SET value=0 WHERE key='album_trigger_revision'",
        "UPDATE schema_meta SET value=1 WHERE key='album_trigger_revision'",
        "UPDATE schema_meta SET value=2 WHERE key='album_trigger_revision'",
        "UPDATE schema_meta SET value=3 WHERE key='album_trigger_revision'",
        "DELETE FROM schema_meta WHERE key='version'",
        "DROP TABLE queue_entries",
    ] {
        let (_directory, options) = fixture();
        fs::write(&options.config, "# user formatting\n[playback]\n").unwrap();
        let connection = Connection::open(options.data_dir.join("library.db")).unwrap();
        connection.execute_batch(sql).unwrap();
        let before = fs::read(options.data_dir.join("library.db")).unwrap();
        drop(connection);
        for mode in [Mode::Check, Mode::Apply] {
            let mut report = Report::default();
            assert!(run(&options, mode, &mut report).is_err(), "{sql}");
            assert!(report.backups.is_empty());
        }
        let mut report = Report::default();
        assert!(
            apply_database_change(&options, &mut report, |_| panic!(
                "unsupported database must not execute changes"
            ))
            .is_err()
        );
        assert!(report.backups.is_empty());
        assert_eq!(
            fs::read(options.data_dir.join("library.db")).unwrap(),
            before
        );
        assert_eq!(
            fs::read_to_string(&options.config).unwrap(),
            "# user formatting\n[playback]\n"
        );
    }
}

#[test]
fn database_ownership_excludes_apply_even_without_a_migration() {
    let (_directory, options) = fixture();
    fs::write(&options.config, "[playback]\n").unwrap();
    let alias = options.data_dir.join("alias");
    symlink(&options.data_dir, &alias).unwrap();
    let owner = UnixListener::bind_addr(&data_directory_address(&alias).unwrap()).unwrap();
    assert!(execute(&options, Mode::Apply).is_err());
    let mut report = Report::default();
    assert!(
        apply_database_change(&options, &mut report, |_| panic!(
            "owned database must not execute changes"
        ))
        .is_err()
    );
    assert!(report.backups.is_empty());
    assert!(execute(&options, Mode::Check).is_ok());
    drop(owner);
    let report = execute(&options, Mode::Apply).unwrap();
    assert!(report.backups.is_empty());
    assert!(!execute(&options, Mode::Check).unwrap().needs_changes);
    assert_eq!(fs::read_to_string(&options.config).unwrap(), "[playback]\n");
}

#[test]
fn sqlite_backup_includes_committed_wal_and_transaction_preserves_user_state() {
    let (_directory, options) = fixture();
    let original = "# unversioned\n[playback]\nvolume_percent=37\n";
    fs::write(&options.config, original).unwrap();
    let config_inode = fs::metadata(&options.config).unwrap().ino();
    let connection = Connection::open(options.data_dir.join("library.db")).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    connection
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    connection.execute_batch("INSERT INTO playlists(name) VALUES('User playlist');
        INSERT INTO albums(directory,title,artist,track_count) VALUES('/retained','Retain','Artist',1);
        INSERT INTO tracks(id,path) VALUES(1,'/music/original.flac');
        INSERT INTO queue_entries(id,track_id,position) VALUES(41,1,0),(42,1,1);
        UPDATE queue_meta SET current_id=42,revision=9;").unwrap();
    let checked = execute(&options, Mode::Check).unwrap();
    assert!(!checked.needs_changes);
    let mut report = Report::default();
    apply_database_change(&options, &mut report, |transaction| {
        transaction.execute("UPDATE tracks SET raw_title='Changed' WHERE id=1", [])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(report.backups.len(), 1);
    assert_eq!(fs::read_to_string(&options.config).unwrap(), original);
    assert_eq!(fs::metadata(&options.config).unwrap().ino(), config_inode);
    assert_eq!(revision(&connection), database::ALBUM_TRIGGER_REVISION);
    assert_eq!(report.backups[0].status, BackupStatus::Complete);
    assert_eq!(report.backups[0].path.extension().unwrap(), "bak");
    assert!(fs::read_dir(&options.data_dir).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .as_encoded_bytes()
            .ends_with(b".partial")
    }));
    let backup =
        Connection::open_with_flags(&report.backups[0].path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_eq!(revision(&backup), database::ALBUM_TRIGGER_REVISION);
    for source in [&connection, &backup] {
        let name: String = source
            .query_row("SELECT name FROM playlists", [], |row| row.get(0))
            .unwrap();
        assert_eq!(name, "User playlist");
        let title: String = source
            .query_row(
                "SELECT title FROM albums WHERE directory='/retained'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title, "Retain");
        assert_eq!(
            source
                .query_row(
                    "SELECT current_id,COUNT(*) FROM queue_entries CROSS JOIN queue_meta",
                    [],
                    |row| { Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)) }
                )
                .unwrap(),
            (42, 2)
        );
        let entries = source
            .prepare("SELECT id,track_id,position FROM queue_entries ORDER BY position,id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<(i64, i64, i64)>>>()
            .unwrap();
        assert_eq!(entries, [(41, 1, 0), (42, 1, 1)]);
    }
    connection
        .execute(
            "UPDATE tracks SET path='/music/renamed.flac' WHERE id=1",
            [],
        )
        .unwrap();
    let queue_revision: i64 = connection
        .query_row("SELECT revision FROM queue_meta", [], |row| row.get(0))
        .unwrap();
    assert!(queue_revision > 9);
    assert_eq!(
        backup
            .query_row("SELECT path FROM tracks WHERE id=1", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "/music/original.flac"
    );
}

#[test]
fn transactional_change_failure_rolls_back_and_reentry_keeps_distinct_backups() {
    let (_directory, options) = fixture();
    fs::write(&options.config, "# original\n[playback]\n").unwrap();
    let connection = Connection::open(options.data_dir.join("library.db")).unwrap();
    let mut failed = Report::default();
    assert!(
        apply_database_change(&options, &mut failed, |transaction| {
            transaction.execute("INSERT INTO playlists(name) VALUES('Rolled back')", [])?;
            bail!("migration rejected")
        })
        .is_err()
    );
    assert_eq!(failed.backups.len(), 1);
    assert_eq!(failed.backups[0].status, BackupStatus::Complete);
    assert_eq!(revision(&connection), database::ALBUM_TRIGGER_REVISION);
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM playlists", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        fs::read_to_string(&options.config).unwrap(),
        "# original\n[playback]\n"
    );
    let mut applied = Report::default();
    apply_database_change(&options, &mut applied, |transaction| {
        transaction.execute("INSERT INTO playlists(name) VALUES('Committed')", [])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        connection
            .query_row("SELECT name FROM playlists", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        "Committed"
    );
    assert_eq!(applied.backups.len(), 1);
    for earlier in &failed.backups {
        assert!(earlier.path.exists());
        assert!(
            !applied
                .backups
                .iter()
                .any(|backup| backup.path == earlier.path)
        );
    }
    assert_eq!(
        fs::read_to_string(&options.config).unwrap(),
        "# original\n[playback]\n"
    );
    assert!(execute(&options, Mode::Apply).unwrap().backups.is_empty());
}
