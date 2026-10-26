use anyhow::Result;
use rusqlite::{Transaction, params};

/// Stage incoming occurrences; directory unions alone are set selections.
pub(super) fn insert_tracks(
    tx: &Transaction<'_>,
    collection_id: i64,
    index: usize,
    ids: &str,
    directories: Option<&str>,
) -> Result<usize> {
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
    insert_staged_tracks(tx, collection_id, index)
}

/// Consume SQL-staged occurrences without changing their order or multiplicity.
pub(super) fn insert_staged_tracks(
    tx: &Transaction<'_>,
    collection_id: i64,
    index: usize,
) -> Result<usize> {
    let index = i64::try_from(index)?;
    let total: i64 = tx.query_row(
        "SELECT (SELECT COALESCE(MAX(position)+1,0) FROM playlist_entries WHERE playlist_id=?1) FROM playlists WHERE id=?1",
        [collection_id], |row| row.get(0),
    )?;
    anyhow::ensure!(index <= total, "Insertion index out of range");
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM insertion_tracks", [], |row| {
        row.get(0)
    })?;
    anyhow::ensure!(
        total.checked_add(count).is_some(),
        "Insertion exceeds database positions"
    );
    if count != 0 {
        tx.execute("UPDATE playlist_entries SET position=position+?2 WHERE playlist_id=?3 AND position>=?1", params![index, count, collection_id])?;
        tx.execute("INSERT INTO playlist_entries(playlist_id,track_id,position) SELECT ?2,track_id,?1+row_number() OVER (ORDER BY ordinal)-1 FROM insertion_tracks ORDER BY ordinal", params![index, collection_id])?;
    }
    tx.execute_batch("DROP TABLE insertion_tracks;")?;
    Ok(usize::try_from(count)?)
}

/// Close deletion gaps using sorted, unique positions captured before deletion.
/// Ascending, disjoint original intervals prevent shifting a survivor twice.
pub(super) fn close_gaps(
    tx: &Transaction<'_>,
    positions: &[i64],
    collection_id: i64,
) -> Result<()> {
    let Some(&last) = positions.last() else {
        return Ok(());
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
            None => update.insert(tx.prepare("UPDATE playlist_entries SET position=position-?3 WHERE playlist_id=?4 AND position>?1 AND position<=?2")?),
        };
        update.execute(params![
            lower,
            upper,
            i64::try_from(index + 1)?,
            collection_id
        ])?;
    }
    tx.execute(
        "UPDATE playlist_entries SET position=position-?2 WHERE playlist_id=?3 AND position>?1",
        params![last, i64::try_from(positions.len())?, collection_id],
    )?;
    Ok(())
}

/// Move a validated, position-sorted selection without mapping survivors.
/// Shift original intervals left in ascending order and right in descending order.
pub(super) fn move_selection(
    tx: &Transaction<'_>,
    collection_id: i64,
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
    let mut update = tx.prepare("UPDATE playlist_entries SET position=position+?1 WHERE position BETWEEN ?2 AND ?3 AND playlist_id=?4")?;
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
        let (lower, upper, shift) = if left {
            (lower, upper.min(insertion + picked - 1), -picked)
        } else {
            (lower.max(insertion + picked), upper, count - picked)
        };
        if lower <= upper && shift != 0 {
            update.execute(params![shift, lower, upper, collection_id])?;
        }
        Ok(())
    };
    for index in 0..=selected.len() {
        shift_interval(index, true)?;
    }
    for index in (0..=selected.len()).rev() {
        shift_interval(index, false)?;
    }
    tx.execute(
        "UPDATE playlist_entries SET position=json_extract(destination.value,'$[1]')
         FROM json_each(?1) AS destination
         WHERE playlist_entries.id=json_extract(destination.value,'$[0]')
         AND playlist_entries.playlist_id=?2
         AND playlist_entries.position<>json_extract(destination.value,'$[1]')",
        params![destinations, collection_id],
    )?;
    Ok(true)
}
