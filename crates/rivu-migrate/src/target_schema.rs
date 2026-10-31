//! Conservative target verification against the authoritative fresh schema.
use anyhow::{Result, bail, ensure};
use rivu_format::database;
use rusqlite::Connection;

// Keep literals and quoted identifiers intact; do not erase token boundaries.
// This deliberately rejects unsupported rewrites instead of guessing equivalence.
fn tokens(mut sql: &str) -> impl Iterator<Item = &str> {
    std::iter::from_fn(move || {
        sql = sql.trim_start_matches(|c: char| c.is_ascii_whitespace());
        let bytes = sql.as_bytes();
        let &first = bytes.first()?;
        let mut end = 1;
        if matches!(first, b'\'' | b'"' | b'`' | b'[') {
            let close = if first == b'[' { b']' } else { first };
            while end < bytes.len() {
                if bytes[end] == close {
                    end += 1;
                    if close != b']' && bytes.get(end) == Some(&close) {
                        end += 1;
                        continue;
                    }
                    break;
                }
                end += 1;
            }
        } else if first.is_ascii_alphanumeric() || first == b'_' || first >= 128 {
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] >= 128)
            {
                end += 1;
            }
        }
        let (token, rest) = sql.split_at(end);
        sql = rest;
        Some(token)
    })
}

pub(super) fn validate(conn: &Connection) -> Result<()> {
    database::validate_structure(conn)?;
    let fresh = Connection::open_in_memory()?;
    database::create_current(&fresh)?;
    const OBJECTS: &str = "SELECT type,name,tbl_name,sql FROM sqlite_schema
        WHERE name NOT GLOB 'sqlite_*' ORDER BY type,name";
    let mut expected_statement = fresh.prepare(OBJECTS)?;
    let mut actual_statement = conn.prepare(OBJECTS)?;
    let mut expected_rows = expected_statement.query([])?;
    let mut actual_rows = actual_statement.query([])?;
    loop {
        let (expected, actual) = match (expected_rows.next()?, actual_rows.next()?) {
            (None, None) => return Ok(()),
            (Some(expected), Some(actual)) => (expected, actual),
            (Some(expected), None) => bail!(
                "Missing fresh-schema object {:?}",
                expected.get_ref(1)?.as_str()?
            ),
            (None, Some(actual)) => bail!(
                "Unexpected schema object {:?}",
                actual.get_ref(1)?.as_str()?
            ),
        };
        for column in 0..3 {
            ensure!(
                expected.get_ref(column)? == actual.get_ref(column)?,
                "Schema object set differs from a fresh database: expected {:?}, found {:?}",
                expected.get_ref(1)?.as_str()?,
                actual.get_ref(1)?.as_str()?
            );
        }
        let mut expected_tokens = tokens(expected.get_ref(3)?.as_str()?);
        let mut actual_tokens = tokens(actual.get_ref(3)?.as_str()?);
        loop {
            let pair = (expected_tokens.next(), actual_tokens.next());
            if pair == (None, None) {
                break;
            }
            let equal = match pair {
                (Some(left), Some(right))
                    if matches!(left.as_bytes()[0], b'\'' | b'"' | b'`' | b'[') =>
                {
                    left == right
                }
                (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
                _ => false,
            };
            ensure!(
                equal,
                "Schema definition differs from a fresh database for {:?}: expected token {:?}, found {:?}",
                expected.get_ref(1)?.as_str()?,
                pair.0,
                pair.1
            );
        }
    }
}
