use rivu_format::database;
use rusqlite::Connection;
use std::{fs, path::Path, process::Command};

const HOSTILE: &str = "path\u{1b}[2J\n\r\t\u{009b}end";
const ESCAPED: &str = "path\\x1b[2J\\n\\r\\t\\u{009b}end";

fn command(data: &Path, config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rivu-migrate"));
    command
        .env_remove("NO_COLOR")
        .env_remove("RIVU_COLOR")
        .arg("--data-dir")
        .arg(data)
        .arg("--config")
        .arg(config)
        .arg("apply");
    command
}

fn invoke(data: &Path, config: &Path) -> std::process::Output {
    command(data, config).output().unwrap()
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
fn success_resources_are_escaped_without_rewriting_files_or_creating_backups() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join(HOSTILE);
    fs::create_dir(&data).unwrap();
    let config = data.join(format!("{HOSTILE}.toml"));
    let original = "# keep me\n[playback]\nvolume_percent=37\n";
    fs::write(&config, original).unwrap();
    let database_path = data.join("library.db");
    let connection = Connection::open(&database_path).unwrap();
    database::create_current(&connection).unwrap();
    drop(connection);

    let output = invoke(&data, &config);
    assert!(output.status.success(), "{:?}", output);
    let stdout = assert_terminal_safe(&output.stdout);
    assert!(
        stdout.contains(&format!("{ESCAPED}.toml: current")),
        "{stdout}"
    );
    assert!(!stdout.lines().any(|line| line.starts_with("Backup: ")));
    assert_terminal_safe(&output.stderr);
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
    assert!(database_path.is_file());
    assert!(!fs::read_dir(&data).unwrap().any(|entry| {
        entry
            .unwrap()
            .path()
            .extension()
            .is_some_and(|extension| extension == "bak")
    }));
}

#[test]
fn unsupported_format_errors_escape_hostile_paths_without_changes() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join(HOSTILE);
    fs::create_dir(&data).unwrap();
    let config = data.join("missing.toml");
    let connection = Connection::open(data.join("library.db")).unwrap();
    database::create_current(&connection).unwrap();
    connection
        .execute_batch("UPDATE rivu_meta SET format_version=99 WHERE singleton=1")
        .unwrap();
    drop(connection);

    let output = invoke(&data, &config);
    assert_eq!(output.status.code(), Some(2));
    let stderr = assert_terminal_safe(&output.stderr);
    assert!(stderr.contains(ESCAPED), "{stderr}");
    assert!(output.stdout.is_empty());
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
    assert_eq!(fs::read_to_string(&config).unwrap(), "[broken");
}

#[test]
fn stderr_color_policy_applies_to_levels_and_structured_fields() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join(format!("{HOSTILE}.toml"));
    fs::write(&config, "[broken").unwrap();
    for (color, no_color, expected_ansi) in [
        (None, false, false),
        (Some("never"), false, false),
        (Some("always"), false, true),
        (Some("always"), true, false),
    ] {
        let mut command = command(directory.path(), &config);
        if let Some(color) = color {
            command.env("RIVU_COLOR", color);
        }
        if no_color {
            command.env("NO_COLOR", "1");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(&format!("{ESCAPED}.toml")), "{stderr:?}");
        assert!(!stderr.contains(HOSTILE), "{stderr:?}");
        if expected_ansi {
            let (prefix, fields) = stderr.split_once("Migration failed ").unwrap();
            assert!(prefix.contains("\x1b["), "{stderr:?}");
            assert!(
                fields.lines().next().unwrap().contains("\x1b["),
                "{stderr:?}"
            );
        } else {
            assert_terminal_safe(stderr.as_bytes());
            assert!(
                stderr.contains("error=Preflighting configuration"),
                "{stderr}"
            );
        }
    }
    assert_eq!(fs::read_to_string(&config).unwrap(), "[broken");
}
