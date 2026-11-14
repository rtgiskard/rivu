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

fn format_version(connection: &Connection) -> i64 {
    connection
        .query_row(
            "SELECT format_version FROM rivu_meta WHERE singleton=1",
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
fn valid_configuration_is_read_only_with_or_without_version_and_through_symlinks() {
    for version in ["", "schema_version=2\n"] {
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
fn configuration_byte_budget_is_enforced_without_changes() {
    let (_directory, options) = fixture();
    for size in [0, config::MAX_CONFIG_BYTES, config::MAX_CONFIG_BYTES + 1] {
        let text = " ".repeat(size);
        fs::write(&options.config, &text).unwrap();
        let mut report = Report::default();
        let result = run(&options, Mode::Apply, &mut report);
        if size <= config::MAX_CONFIG_BYTES {
            result.unwrap();
        } else {
            assert!(result.is_err());
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
        "UPDATE rivu_meta SET format_version=99 WHERE singleton=1",
        "UPDATE rivu_meta SET format_version=1 WHERE singleton=1",
        "UPDATE rivu_meta SET format_version=3 WHERE singleton=1",
        "UPDATE rivu_meta SET format_version=5 WHERE singleton=1",
        "DELETE FROM rivu_meta",
        "DROP TABLE playlist_entries",
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
        INSERT INTO tracks(id,path,favorite,play_count,last_played) VALUES(1,'/music/original.flac',1,7,12345);
        INSERT INTO playlist_entries(id,playlist_id,track_id,position) VALUES(41,0,1,0),(42,0,1,1);
        UPDATE rivu_meta SET current_queue_entry_id=42;").unwrap();
    let checked = execute(&options, Mode::Check).unwrap();
    assert!(!checked.needs_changes);
    let mut report = Report::default();
    apply_database_change(&options, &mut report, |transaction| {
        transaction.execute("UPDATE tracks SET title='Changed' WHERE id=1", [])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(report.backups.len(), 1);
    assert_eq!(fs::read_to_string(&options.config).unwrap(), original);
    assert_eq!(fs::metadata(&options.config).unwrap().ino(), config_inode);
    assert_eq!(format_version(&connection), database::SCHEMA_VERSION);
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
    assert_eq!(format_version(&backup), database::SCHEMA_VERSION);
    for source in [&connection, &backup] {
        let name: String = source
            .query_row("SELECT name FROM playlists WHERE id>0", [], |row| {
                row.get(0)
            })
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
                    "SELECT current_queue_entry_id,COUNT(*) FROM playlist_entries CROSS JOIN rivu_meta WHERE playlist_id=0",
                    [],
                    |row| { Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)) }
                )
                .unwrap(),
            (42, 2)
        );
        let entries = source
            .prepare("SELECT id,track_id,position FROM playlist_entries WHERE playlist_id=0 ORDER BY position,id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<(i64, i64, i64)>>>()
            .unwrap();
        assert_eq!(entries, [(41, 1, 0), (42, 1, 1)]);
        assert_eq!(
            source
                .query_row(
                    "SELECT favorite,play_count,last_played FROM tracks WHERE id=1",
                    [],
                    |row| Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?
                    )),
                )
                .unwrap(),
            (1, 7, 12345)
        );
    }
    connection
        .execute(
            "UPDATE tracks SET path='/music/renamed.flac' WHERE id=1",
            [],
        )
        .unwrap();
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
    assert_eq!(format_version(&connection), database::SCHEMA_VERSION);
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM playlists WHERE id>0", [], |row| row
                .get::<_, i64>(
                0
            ))
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
            .query_row("SELECT name FROM playlists WHERE id>0", [], |row| row
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

fn version_two_fixture() -> (tempfile::TempDir, Options) {
    let directory = tempfile::tempdir().unwrap();
    let options = Options {
        data_dir: directory.path().to_path_buf(),
        config: directory.path().join("config.toml"),
        explicit_data_dir: true,
    };
    let conn = Connection::open(options.data_dir.join("library.db")).unwrap();
    conn.execute_batch(include_str!("../tests/fixtures/schema2.sql"))
        .unwrap();
    conn.execute_batch(
        "INSERT INTO tracks(id,path,raw_album,raw_artist) VALUES
             (1,'one','Original album','Original artist'),(2,'two','','');
         INSERT INTO tracks(id,path,cue_sheet,cue_number,cue_start_frame,cue_end_frame,
             raw_title,raw_album,raw_artist) VALUES
             (3,'/album/image.flac','/album/disc.cue',1,0,4500,'First','CUE album','CUE artist'),
             (4,'/album/image.flac','/album/disc.cue',2,4500,NULL,'Second','CUE album','CUE artist');
         INSERT INTO playlists(id,name) VALUES(30,'Zebra'),(10,'Middle'),(20,'Alpha');
         INSERT INTO playlist_entries(id,playlist_id,track_id,position) VALUES
             (41,0,1,0),(42,0,2,1),(43,0,1,2),(44,0,3,3),
             (51,10,2,0),(52,10,1,1),(53,10,2,2),(54,10,4,3);
         UPDATE tracks SET title_override='User title',favorite=1,play_count=7,last_played=42,
             fingerprint=zeroblob(32),file_size=123456,modified_ns=987654321,
             artist_override='User artist',album_override='User album',duration=60.5,
             codec='flac',channels=2,sample_rate=44100,probe_version=3,bitrate_bps=1000000,
             track_number=2,disc_number=1,bitrate_exact=1,bits_per_sample=16,
             release_date='2001-02-03',relocation_independent=1,decoder_hint=2 WHERE id=1;
         UPDATE tracks SET raw_title='Hidden title',title_override='',raw_artist='Hidden artist',
             artist_override='',raw_album='Hidden album',album_override='' WHERE id=2;
         UPDATE rivu_meta SET current_queue_entry_id=42;",
    )
    .unwrap();
    target_schema::validate_tag_source(&conn).unwrap();
    (directory, options)
}

fn occurrence_snapshot(conn: &Connection) -> Vec<(i64, i64, i64, i64)> {
    conn.prepare("SELECT id,playlist_id,track_id,position FROM playlist_entries ORDER BY id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn track_snapshot(conn: &Connection) -> Vec<Vec<rusqlite::types::Value>> {
    let mut statement = conn.prepare("SELECT * FROM tracks ORDER BY id").unwrap();
    let columns = statement.column_count();
    statement
        .query_map([], |row| {
            (0..columns).map(|column| row.get(column)).collect()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn effective_track_snapshot(conn: &Connection) -> Vec<Vec<rusqlite::types::Value>> {
    let mut statement = conn
        .prepare(
            "SELECT id,path,fingerprint,file_size,modified_ns,
        COALESCE(title_override,raw_title),COALESCE(artist_override,raw_artist),
        COALESCE(album_override,raw_album),duration,codec,channels,sample_rate,missing,
        play_count,last_played,cue_sheet,cue_number,cue_start_frame,cue_end_frame,
        probe_version,bitrate_bps,track_number,disc_number,bitrate_exact,bits_per_sample,
        release_date,favorite,relocation_independent,decoder_hint FROM tracks ORDER BY id",
        )
        .unwrap();
    let columns = statement.column_count();
    statement
        .query_map([], |row| {
            (0..columns).map(|column| row.get(column)).collect()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn album_snapshot(conn: &Connection) -> Vec<(String, String, i64, Option<String>)> {
    conn.prepare("SELECT directory,title,track_count,artist FROM albums ORDER BY directory,title")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn catalog_migration_preserves_previous_order_contents_and_queue_after_reopen() {
    let (_directory, options) = version_two_fixture();
    fs::write(
        &options.config,
        "# preserve formatting\n[playback]\nvolume_percent=37\n",
    )
    .unwrap();
    let path = options.data_dir.join("library.db");
    let conn = Connection::open(&path).unwrap();
    let source_version = format_version(&conn);
    assert_eq!(source_version, 2);
    let tracks = track_snapshot(&conn);
    let effective_tracks = effective_track_snapshot(&conn);
    let albums = album_snapshot(&conn);
    let occurrences = occurrence_snapshot(&conn);
    let schema: i64 = conn
        .pragma_query_value(None, "schema_version", |row| row.get(0))
        .unwrap();
    // Check opens its own connection; observe commits rather than local changes.
    let data_version: i64 = conn
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .unwrap();
    let checked = execute(&options, Mode::Check).unwrap();
    assert!(checked.needs_changes);
    assert!(checked.backups.is_empty());
    assert_eq!(format_version(&conn), source_version);
    assert_eq!(
        conn.pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        data_version
    );
    assert_eq!(
        conn.pragma_query_value(None, "schema_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        schema
    );
    assert!(database::validate_structure(&conn).is_err());

    let applied = execute(&options, Mode::Apply).unwrap();
    assert!(!applied.needs_changes);
    assert_eq!(applied.backups.len(), 1);
    assert_eq!(applied.backups[0].status, BackupStatus::Complete);
    let backup = Connection::open(&applied.backups[0].path).unwrap();
    assert_eq!(format_version(&backup), source_version);
    target_schema::validate_tag_source(&backup).unwrap();
    assert_eq!(occurrence_snapshot(&backup), occurrences);
    assert_eq!(track_snapshot(&backup), tracks);
    assert_eq!(album_snapshot(&backup), albums);
    drop(conn);

    let conn = Connection::open(&path).unwrap();
    database::validate_structure(&conn).unwrap();
    let order = conn
        .prepare("SELECT id,name,position FROM playlists WHERE id>0 ORDER BY position,id")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        order,
        [(10, "Middle"), (20, "Alpha"), (30, "Zebra")]
            .into_iter()
            .enumerate()
            .map(|(position, (id, name))| (id, name.to_owned(), position as i64))
            .collect::<Vec<_>>()
    );
    assert_eq!(occurrence_snapshot(&conn), occurrences);
    assert_eq!(track_snapshot(&conn), effective_tracks);
    assert_eq!(album_snapshot(&conn), albums);
    assert_eq!(
        conn.query_row("SELECT current_queue_entry_id FROM rivu_meta", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        42
    );
    let statistics = conn
        .query_row(
            "SELECT title,favorite,play_count,last_played FROM tracks WHERE id=1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(statistics, (String::from("User title"), true, 7, 42));
    assert_eq!(
        conn.query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='playlists'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        30
    );
    assert_eq!(
        fs::read_to_string(&options.config).unwrap(),
        "# preserve formatting\n[playback]\nvolume_percent=37\n"
    );
    let repeated = execute(&options, Mode::Apply).unwrap();
    assert!(!repeated.needs_changes);
    assert!(repeated.backups.is_empty());
}

#[test]
fn upgraded_history_supports_cue_identity_and_collection_writes() {
    let (_directory, options) = version_two_fixture();
    execute(&options, Mode::Apply).unwrap();
    let conn = Connection::open(options.data_dir.join("library.db")).unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    let cue_tracks = conn
        .prepare(
            "SELECT id,cue_number,cue_start_frame,cue_end_frame
             FROM tracks INDEXED BY tracks_cue
             WHERE cue_sheet IS NOT NULL AND cue_sheet=?1 ORDER BY cue_number",
        )
        .unwrap()
        .query_map(["/album/disc.cue"], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(cue_tracks, [(3, 1, 0, Some(4500)), (4, 2, 4500, None)]);

    // An ordinary track and separate CUE identities can share the audio path.
    conn.execute_batch(
        "INSERT INTO tracks(path) VALUES('/album/image.flac');
         INSERT INTO tracks(path,cue_sheet,cue_number)
             VALUES('/album/image.flac','/album/alternate.cue',1);",
    )
    .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM tracks INDEXED BY tracks_path WHERE path=?1
             AND CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END=0 AND missing=0",
            ["/album/image.flac"],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        1
    );
    for sql in [
        "INSERT INTO tracks(path,missing) VALUES('/album/image.flac',1)",
        "INSERT INTO tracks(path,cue_sheet,cue_number) VALUES('/other.flac','/album/disc.cue',1)",
    ] {
        assert_eq!(
            conn.execute(sql, []).unwrap_err().sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
    }

    conn.execute(
        "INSERT INTO playlists(name,position)
         SELECT 'New playlist',COALESCE(MAX(position)+1,0) FROM playlists WHERE id>0",
        [],
    )
    .unwrap();
    let playlist = conn.last_insert_rowid();
    assert_eq!(playlist, 31);
    assert_eq!(
        conn.query_row(
            "SELECT position FROM playlists WHERE id=?1",
            [playlist],
            |row| { row.get::<_, i64>(0) }
        )
        .unwrap(),
        3
    );
    conn.execute(
        "INSERT INTO playlist_entries(playlist_id,track_id,position)
         SELECT ?1,value,CAST(key AS INTEGER) FROM json_each('[3,3]') ORDER BY CAST(key AS INTEGER)",
        [playlist],
    )
    .unwrap();
    let appended = occurrence_snapshot(&conn)
        .into_iter()
        .filter(|entry| entry.1 == playlist)
        .collect::<Vec<_>>();
    assert_eq!(appended.len(), 2);
    assert!(appended.iter().all(|entry| entry.0 > 54 && entry.2 == 3));
    assert_eq!((appended[0].3, appended[1].3), (0, 1));

    conn.execute_batch(
        "UPDATE rivu_meta SET current_queue_entry_id=43 WHERE singleton=1;
         DELETE FROM playlist_entries WHERE id=41;",
    )
    .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT current_queue_entry_id,
                 (SELECT track_id FROM playlist_entries WHERE id=current_queue_entry_id)
             FROM rivu_meta WHERE singleton=1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap(),
        (43, 1)
    );
    conn.execute_batch("DELETE FROM tracks WHERE id=1;")
        .unwrap();
    assert_eq!(
        conn.query_row("SELECT current_queue_entry_id FROM rivu_meta", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .unwrap(),
        None
    );
    assert!(!occurrence_snapshot(&conn).iter().any(|entry| entry.2 == 1));
    conn.execute("DELETE FROM playlists WHERE id=?1", [playlist])
        .unwrap();
    assert!(
        !occurrence_snapshot(&conn)
            .iter()
            .any(|entry| entry.1 == playlist)
    );
}

#[test]
fn upgraded_history_keeps_album_summary_triggers_writable() {
    let (_directory, options) = version_two_fixture();
    execute(&options, Mode::Apply).unwrap();
    let conn = Connection::open(options.data_dir.join("library.db")).unwrap();
    let original = album_snapshot(&conn);
    conn.execute_batch(
        "INSERT INTO tracks(id,path,album,artist)
             VALUES(5,'/fresh/one.flac','Fresh album','First artist'),
                   (6,'/fresh/two.flac','Fresh album','Second artist');",
    )
    .unwrap();
    let summary = |title: &str| {
        conn.query_row(
            "SELECT track_count,artist FROM albums WHERE directory='/fresh' AND title=?1",
            [title],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap()
    };
    assert_eq!(summary("Fresh album"), (2, "Various artists".to_owned()));
    conn.execute("UPDATE tracks SET artist='First artist' WHERE id=6", [])
        .unwrap();
    assert_eq!(summary("Fresh album"), (2, "First artist".to_owned()));
    conn.execute("UPDATE tracks SET album='Moved album' WHERE id=6", [])
        .unwrap();
    assert_eq!(summary("Fresh album"), (1, "First artist".to_owned()));
    assert_eq!(summary("Moved album"), (1, "First artist".to_owned()));
    conn.execute_batch("DELETE FROM tracks WHERE id IN (5,6);")
        .unwrap();
    assert_eq!(album_snapshot(&conn), original);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM album_artists WHERE directory='/fresh'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        0
    );
}

#[test]
fn schema_two_preflight_rejects_changed_constraints_and_defaults() {
    for (original, changed) in [
        (
            "file_size INTEGER NOT NULL DEFAULT 0",
            "file_size INTEGER NOT NULL DEFAULT 1",
        ),
        (
            "favorite INTEGER NOT NULL DEFAULT 0 CHECK(favorite IN (0,1))",
            "favorite INTEGER NOT NULL DEFAULT 0",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let options = Options {
            data_dir: directory.path().to_path_buf(),
            config: directory.path().join("config.toml"),
            explicit_data_dir: true,
        };
        let path = options.data_dir.join("library.db");
        let conn = Connection::open(&path).unwrap();
        let schema = include_str!("../tests/fixtures/schema2.sql").replace(original, changed);
        conn.execute_batch(&schema).unwrap();
        database::validate_common_structure(&conn).unwrap();
        let before = fs::read(&path).unwrap();
        for mode in [Mode::Check, Mode::Apply] {
            let mut report = Report::default();
            let error = run(&options, mode, &mut report).expect_err(changed);
            assert!(format!("{error:#}").contains("Schema definition differs"));
            assert!(report.backups.is_empty());
            assert_eq!(format_version(&conn), 2);
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(fs::read_dir(&options.data_dir).unwrap().count(), 1);
        }
    }
}

#[test]
fn unrecognized_schema_changes_fail_preflight_without_losing_user_objects() {
    for (change, retained_value) in [
        (
            "CREATE TABLE private_notes(value TEXT); INSERT INTO private_notes VALUES('keep me');",
            "SELECT value FROM private_notes",
        ),
        (
            "CREATE INDEX custom_title_lookup ON tracks(raw_title);",
            "SELECT name FROM sqlite_schema WHERE name='custom_title_lookup'",
        ),
        (
            "CREATE VIEW custom_titles AS SELECT raw_title FROM tracks;",
            "SELECT sql FROM sqlite_schema WHERE name='custom_titles'",
        ),
        (
            "CREATE TRIGGER custom_playlist_update AFTER UPDATE ON playlists BEGIN UPDATE tracks SET raw_title='changed' WHERE id=1; END;",
            "SELECT sql FROM sqlite_schema WHERE name='custom_playlist_update'",
        ),
        (
            "ALTER TABLE tracks ADD COLUMN retired TEXT DEFAULT '  keep me  ';",
            "SELECT retired FROM tracks WHERE id=1",
        ),
        (
            "DROP TRIGGER albums_update; CREATE TRIGGER albums_update AFTER UPDATE ON tracks BEGIN SELECT 1; END;",
            "SELECT name FROM sqlite_schema WHERE name='albums_update'",
        ),
        (
            "DROP INDEX albums_artist; CREATE INDEX albums_artist ON albums(COALESCE(artist,'unknown artist'),title,directory);",
            "SELECT sql FROM sqlite_schema WHERE name='albums_artist'",
        ),
        (
            "DROP TRIGGER albums_artist_update;",
            "SELECT sql FROM sqlite_schema WHERE name='albums_update'",
        ),
        (
            "CREATE TABLE sqlitex_notes(value TEXT); INSERT INTO sqlitex_notes VALUES('keep me');",
            "SELECT value FROM sqlitex_notes",
        ),
    ] {
        let (_directory, options) = version_two_fixture();
        let path = options.data_dir.join("library.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(change).unwrap();
        let retained: String = conn
            .query_row(retained_value, [], |row| row.get(0))
            .unwrap();
        let occurrences = occurrence_snapshot(&conn);
        let tracks = track_snapshot(&conn);
        let albums = album_snapshot(&conn);
        let before = fs::read(&path).unwrap();
        let entries = fs::read_dir(&options.data_dir).unwrap().count();
        let mut failures = Vec::new();
        for mode in [Mode::Check, Mode::Apply] {
            let mut report = Report::default();
            let error = run(&options, mode, &mut report).expect_err(change);
            failures.push(format!("{error:#}"));
            assert!(report.backups.is_empty(), "{change}");
            assert_eq!(report.database_status, "unchanged");
            assert_eq!(format_version(&conn), 2);
            assert_eq!(fs::read(&path).unwrap(), before, "{change}");
            assert_eq!(fs::read_dir(&options.data_dir).unwrap().count(), entries);
            assert_eq!(
                conn.query_row(retained_value, [], |row| row.get::<_, String>(0))
                    .unwrap(),
                retained
            );
            assert_eq!(occurrence_snapshot(&conn), occurrences);
            assert_eq!(track_snapshot(&conn), tracks);
            assert_eq!(album_snapshot(&conn), albums);
        }
        assert_eq!(failures[0], failures[1], "{change}");
    }
}

#[test]
fn failed_catalog_migration_rolls_back_schema_version_and_data() {
    let (_directory, options) = version_two_fixture();
    let conn = Connection::open(options.data_dir.join("library.db")).unwrap();
    let occurrences = occurrence_snapshot(&conn);
    let tracks = track_snapshot(&conn);
    let albums = album_snapshot(&conn);
    let mut report = Report::default();
    let error = apply_database_change(&options, &mut report, |transaction| {
        catalog_order::apply(transaction)?;
        track_tags::apply(transaction)?;
        transaction.execute_batch(
            "CREATE TABLE unexpected(value TEXT);
             UPDATE tracks SET title='must roll back' WHERE id=1;",
        )?;
        target_schema::validate(transaction)
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("Schema object set differs from a fresh database"));
    assert_eq!(report.backups.len(), 1);
    assert_eq!(report.backups[0].status, BackupStatus::Complete);
    let backup = Connection::open(&report.backups[0].path).unwrap();
    target_schema::validate_tag_source(&backup).unwrap();
    assert_eq!(occurrence_snapshot(&backup), occurrences);
    assert_eq!(track_snapshot(&backup), tracks);
    assert_eq!(album_snapshot(&backup), albums);
    target_schema::validate_tag_source(&conn).unwrap();
    assert_eq!(occurrence_snapshot(&conn), occurrences);
    assert_eq!(track_snapshot(&conn), tracks);
    assert_eq!(album_snapshot(&conn), albums);
    assert_eq!(
        conn.query_row("SELECT current_queue_entry_id FROM rivu_meta", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        42
    );
}

#[test]
fn incompatible_old_formats_are_rejected_before_backup_or_schema_changes() {
    for sql in [
        "DROP INDEX playlist_order",
        "DELETE FROM playlists WHERE id=0",
        "ALTER TABLE playlists ADD COLUMN position INTEGER NOT NULL DEFAULT 0",
    ] {
        let (_directory, options) = version_two_fixture();
        let conn = Connection::open(options.data_dir.join("library.db")).unwrap();
        conn.execute_batch(sql).unwrap();
        let version = format_version(&conn);
        let schema: i64 = conn
            .pragma_query_value(None, "schema_version", |row| row.get(0))
            .unwrap();
        for mode in [Mode::Check, Mode::Apply] {
            let mut report = Report::default();
            assert!(run(&options, mode, &mut report).is_err());
            assert!(report.backups.is_empty());
            assert_eq!(format_version(&conn), version);
            assert_eq!(
                conn.pragma_query_value(None, "schema_version", |row| row.get::<_, i64>(0))
                    .unwrap(),
                schema
            );
        }
    }
}

#[test]
fn schema_four_tag_cutover_preserves_effective_empty_values_sequences_and_audio() {
    let (directory, options) = version_two_fixture();
    let path = options.data_dir.join("library.db");
    let conn = Connection::open(&path).unwrap();
    let audio = directory.path().join("preserved.flac");
    fs::write(&audio, b"audio bytes must remain untouched").unwrap();
    catalog_order::apply(&conn).unwrap();
    conn.execute(
        "UPDATE tracks SET path=?1 WHERE id=1",
        [audio.to_str().unwrap()],
    )
    .unwrap();
    conn.execute_batch(
        "UPDATE playlists SET position=CASE id WHEN 30 THEN 0 WHEN 10 THEN 1 ELSE 2 END WHERE id>0;
        INSERT INTO tracks(id,path) VALUES(10000,'/deleted-sequence.wav');
        DELETE FROM tracks WHERE id=10000;",
    )
    .unwrap();
    let tracks = effective_track_snapshot(&conn);
    let occurrences = occurrence_snapshot(&conn);
    let albums = album_snapshot(&conn);
    let applied = execute(&options, Mode::Apply).unwrap();
    assert_eq!(applied.backups.len(), 1);
    assert_eq!(
        format_version(&Connection::open(&applied.backups[0].path).unwrap()),
        4
    );
    assert_eq!(format_version(&conn), 6);
    assert_eq!(track_snapshot(&conn), tracks);
    assert_eq!(occurrence_snapshot(&conn), occurrences);
    assert_eq!(album_snapshot(&conn), albums);
    assert_eq!(
        fs::read(&audio).unwrap(),
        b"audio bytes must remain untouched"
    );
    let empty: (String, String, String) = conn
        .query_row(
            "SELECT title,artist,album FROM tracks WHERE id=2",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(empty, (String::new(), String::new(), String::new()));
    let order = conn
        .prepare("SELECT id FROM playlists WHERE id>0 ORDER BY position,id")
        .unwrap()
        .query_map([], |row| row.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(order, [30, 10, 20]);
    let obsolete: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('tracks') WHERE name IN (
            'raw_title','raw_artist','raw_album','title_override','artist_override','album_override')",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(obsolete, 0);
    target_schema::validate(&conn).unwrap();
    conn.execute("INSERT INTO tracks(path) VALUES('/next-track.wav')", [])
        .unwrap();
    assert_eq!(conn.last_insert_rowid(), 10001);
    assert!(execute(&options, Mode::Apply).unwrap().backups.is_empty());
}

#[test]
fn target_schema_keeps_quoted_identifier_spelling_significant() {
    let conn = Connection::open_in_memory().unwrap();
    database::create_current(&conn).unwrap();
    conn.execute_batch("ALTER TABLE tracks RENAME COLUMN title TO \"Title\";")
        .unwrap();
    database::validate_structure(&conn).unwrap();
    let error = target_schema::validate(&conn).unwrap_err();
    assert!(format!("{error:#}").contains("Schema definition differs"));
}
