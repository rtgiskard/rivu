use rivu_format::database;
use rusqlite::Connection;
use std::{fs, path::Path, process::Command};

const HOSTILE: &str = "path\u{1b}[2J\n\r\t\u{009b}end";
const ESCAPED: &str = "path\\x1b[2J\\n\\r\\t\\u{009b}end";

fn invoke(data: &Path, config: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rivu-migrate"))
        .arg("--data-dir")
        .arg(data)
        .arg("--config")
        .arg(config)
        .arg("apply")
        .output()
        .unwrap()
}

fn assert_terminal_safe(bytes: &[u8]) -> String {
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        text.chars()
            .all(|character| !character.is_control() || character == '\n')
    );
    text
}

#[test]
fn success_resources_and_backup_paths_are_escaped_without_renaming_files() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join(HOSTILE);
    fs::create_dir(&data).unwrap();
    let config = data.join(format!("{HOSTILE}.toml"));
    let original = "# keep me\n[playback]\nvolume_percent=37\n";
    fs::write(&config, original).unwrap();
    let database_path = data.join("library.db");
    let connection = Connection::open(&database_path).unwrap();
    database::create_current(&connection).unwrap();
    connection
        .execute_batch("UPDATE schema_meta SET value=1 WHERE key='album_trigger_revision'")
        .unwrap();
    drop(connection);

    let output = invoke(&data, &config);
    assert!(output.status.success(), "{:?}", output);
    let stdout = assert_terminal_safe(&output.stdout);
    assert!(
        stdout.contains(&format!("{ESCAPED}.toml: current")),
        "{stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("Backup: ") && line.contains(ESCAPED))
    );
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
    assert!(database_path.is_file());
    assert!(fs::read_dir(&data).unwrap().any(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|extension| extension == "bak")
    }));
}

#[test]
fn errors_escape_hostile_paths_and_inner_anyhow_causes() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join(HOSTILE);
    fs::create_dir(&data).unwrap();
    let config = data.join("missing.toml");
    let connection = Connection::open(data.join("library.db")).unwrap();
    database::create_current(&connection).unwrap();
    connection
        .execute_batch(
            "UPDATE schema_meta SET value=1 WHERE key='album_trigger_revision';
         CREATE TRIGGER reject_maintenance BEFORE INSERT ON schema_meta
         WHEN NEW.key='album_trigger_revision'
         BEGIN SELECT RAISE(ABORT,'inner\u{1b}[2J\n\t\u{009b}cause'); END;",
        )
        .unwrap();
    drop(connection);

    let output = invoke(&data, &config);
    assert_eq!(output.status.code(), Some(2));
    let stdout = assert_terminal_safe(&output.stdout);
    let stderr = assert_terminal_safe(&output.stderr);
    assert!(stdout.contains(ESCAPED), "{stdout}");
    assert!(
        stderr.contains("Applying transactional derived trigger maintenance"),
        "{stderr}"
    );
    assert!(
        stderr.contains("inner\\x1b[2J\\n\\t\\u{009b}cause"),
        "{stderr}"
    );
    assert!(!config.exists());
}

#[test]
fn preflight_error_paths_are_escaped_at_final_cli_output() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    fs::create_dir(&data).unwrap();
    let config = directory.path().join(format!("{HOSTILE}.toml"));
    fs::write(&config, "[broken").unwrap();
    let output = invoke(&data, &config);
    assert_eq!(output.status.code(), Some(2));
    let stderr = assert_terminal_safe(&output.stderr);
    assert!(stderr.contains(&format!("{ESCAPED}.toml")), "{stderr}");
    assert!(
        stderr.contains("Validating current configuration"),
        "{stderr}"
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), "[broken");
}
