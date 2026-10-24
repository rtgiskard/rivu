use anyhow::Result;
use rusqlite::{Transaction, params};

#[derive(Clone, Copy)]
pub(super) enum PositionScope {
    Queue,
    Playlist(i64),
}

/// Stage only incoming tracks, then open a gap and insert within one transaction.
/// Source selection keeps the existing union's stable ID order and deduplication.
pub(super) fn insert_tracks(
    tx: &Transaction<'_>,
    scope: PositionScope,
    index: usize,
    ids: &str,
    directories: Option<&str>,
) -> Result<usize> {
    let index = i64::try_from(index)?;
    let total: i64 = match scope {
        PositionScope::Queue => tx.query_row(
            "SELECT COALESCE(MAX(position)+1,0) FROM queue_entries", [], |row| row.get(0),
        )?,
        PositionScope::Playlist(id) => tx.query_row(
            "SELECT (SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?1) FROM playlists WHERE id=?1",
            [id], |row| row.get(0),
        )?,
    };
    anyhow::ensure!(index <= total, "Insertion index out of range");
    tx.execute_batch("CREATE TEMP TABLE insertion_tracks(ordinal INTEGER PRIMARY KEY,track_id INTEGER NOT NULL);")?;
    if let Some(directories) = directories {
        tx.execute(&format!(
            "INSERT INTO insertion_tracks SELECT row_number() OVER (ORDER BY tracks.id)-1,tracks.id FROM tracks WHERE {} ORDER BY tracks.id",
            super::tracks::SOURCE_FILTER,
        ), params![directories, ids])?;
    } else {
        tx.execute(
            "INSERT INTO insertion_tracks SELECT CAST(key AS INTEGER),value FROM json_each(?1) ORDER BY CAST(key AS INTEGER)",
            [ids],
        )?;
    }
    if let PositionScope::Playlist(id) = scope {
        tx.execute(
            "DELETE FROM insertion_tracks WHERE track_id IN (SELECT track_id FROM playlist_entries WHERE playlist_id=?1) OR ordinal NOT IN (SELECT MIN(ordinal) FROM insertion_tracks GROUP BY track_id)",
            [id],
        )?;
    }
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM insertion_tracks", [], |row| {
        row.get(0)
    })?;
    anyhow::ensure!(
        total.checked_add(count).is_some(),
        "Insertion exceeds database positions"
    );
    if count != 0 {
        match scope {
            PositionScope::Queue => {
                tx.execute(
                    "UPDATE queue_entries SET position=position+?2 WHERE position>=?1",
                    params![index, count],
                )?;
                tx.execute("INSERT INTO queue_entries(track_id,position) SELECT track_id,?1+ordinal FROM insertion_tracks ORDER BY ordinal", [index])?;
            }
            PositionScope::Playlist(id) => {
                tx.execute("UPDATE playlist_entries SET position=position+?2 WHERE playlist_id=?3 AND position>=?1", params![index, count, id])?;
                tx.execute("INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT ?2,track_id,?1+row_number() OVER (ORDER BY ordinal)-1 FROM insertion_tracks ORDER BY ordinal", params![index, id])?;
            }
        }
    }
    tx.execute_batch("DROP TABLE insertion_tracks;")?;
    Ok(usize::try_from(count)?)
}

/// Close deletion gaps using sorted, unique positions captured before deletion.
/// Ascending, disjoint original intervals prevent shifting a survivor twice.
pub(super) fn close_gaps(
    tx: &Transaction<'_>,
    positions: &[i64],
    scope: PositionScope,
) -> Result<()> {
    let Some(&last) = positions.last() else {
        return Ok(());
    };
    let sql = match scope {
        PositionScope::Queue => {
            "UPDATE queue_entries SET position=position-?3 WHERE position>?1 AND position<=?2"
        }
        PositionScope::Playlist(_) => {
            "UPDATE playlist_entries SET position=position-?3 WHERE playlist_id=?4 AND position>?1 AND position<=?2"
        }
    };
    let mut update: Option<rusqlite::Statement<'_>> = None;
    for (index, interval) in positions.windows(2).enumerate() {
        let lower = interval[0];
        let upper = interval[1] - 1;
        if upper <= lower {
            continue;
        }
        let update = match update.as_mut() {
            Some(update) => update,
            None => update.insert(tx.prepare(sql)?),
        };
        let shift = i64::try_from(index + 1)?;
        match scope {
            PositionScope::Queue => update.execute(params![lower, upper, shift])?,
            PositionScope::Playlist(id) => update.execute(params![lower, upper, shift, id])?,
        };
    }
    let shift = i64::try_from(positions.len())?;
    // An empty tail is cheaper to update than probing its maximum first.
    match scope {
        PositionScope::Queue => tx.execute(
            "UPDATE queue_entries SET position=position-?2 WHERE position>?1",
            params![last, shift],
        )?,
        PositionScope::Playlist(id) => tx.execute(
            "UPDATE playlist_entries SET position=position-?2 WHERE playlist_id=?3 AND position>?1",
            params![last, shift, id],
        )?,
    };
    Ok(())
}

/// Move a validated, position-sorted selection without mapping survivors.
/// Shift original intervals left in ascending order and right in descending order.
pub(super) fn move_selection(
    tx: &Transaction<'_>,
    scope: PositionScope,
    selected: &[(i64, i64)],
    insertion: i64,
) -> Result<bool> {
    if selected.is_empty()
        || selected
            .iter()
            .enumerate()
            .all(|(ordinal, &(_, position))| position == insertion + ordinal as i64)
    {
        return Ok(false);
    }
    let count = i64::try_from(selected.len())?;
    let first = selected[0].1.min(insertion);
    let last = selected[selected.len() - 1].1.max(insertion + count - 1);
    let mut destinations = Vec::new();
    serde::ser::Serializer::collect_seq(
        &mut serde_json::Serializer::new(&mut destinations),
        selected
            .iter()
            .enumerate()
            .map(|(ordinal, &(id, _))| (id, insertion + ordinal as i64)),
    )?;
    let destinations = String::from_utf8(destinations)?;
    let sql = match scope {
        PositionScope::Queue => {
            "UPDATE queue_entries SET position=position+?1 WHERE position BETWEEN ?2 AND ?3"
        }
        PositionScope::Playlist(_) => {
            "UPDATE playlist_entries SET position=position+?1 WHERE position BETWEEN ?2 AND ?3 AND playlist_id=?4"
        }
    };
    let mut update = tx.prepare(sql)?;
    let mut shift_interval = |index: usize, left: bool| -> Result<()> {
        let lower = if index == 0 {
            first
        } else {
            selected[index - 1].1 + 1
        };
        let upper = if index == selected.len() {
            last
        } else {
            selected[index].1 - 1
        };
        let picked = i64::try_from(index)?;
        // Split at the insertion point in positions compressed by removal.
        let (lower, upper, shift) = if left {
            (lower, upper.min(insertion + picked - 1), -picked)
        } else {
            (lower.max(insertion + picked), upper, count - picked)
        };
        if lower <= upper && shift != 0 {
            match scope {
                PositionScope::Queue => update.execute(params![shift, lower, upper])?,
                PositionScope::Playlist(id) => update.execute(params![shift, lower, upper, id])?,
            };
        }
        Ok(())
    };
    for index in 0..=selected.len() {
        shift_interval(index, true)?;
    }
    for index in (0..=selected.len()).rev() {
        shift_interval(index, false)?;
    }
    let sql = match scope {
        PositionScope::Queue => {
            "UPDATE queue_entries SET position=json_extract(destination.value,'$[1]')
             FROM json_each(?1) AS destination
             WHERE queue_entries.id=json_extract(destination.value,'$[0]')
             AND queue_entries.position<>json_extract(destination.value,'$[1]')"
        }
        PositionScope::Playlist(_) => {
            "UPDATE playlist_entries SET position=json_extract(destination.value,'$[1]')
             FROM json_each(?1) AS destination
             WHERE playlist_entries.id=json_extract(destination.value,'$[0]')
             AND playlist_entries.position<>json_extract(destination.value,'$[1]')"
        }
    };
    tx.execute(sql, [destinations])?;
    Ok(true)
}
