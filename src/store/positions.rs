use anyhow::Result;
use rusqlite::{Transaction, params};

#[derive(Clone, Copy)]
pub(super) enum PositionScope {
    Queue,
    Playlist(i64),
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
