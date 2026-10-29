use rivu_format::decoder_hint::DecoderHint;
use rivu_protocol::model::{CueSegment, Track};
use std::path::PathBuf;

pub(super) fn fingerprint_from_row(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<Option<String>> {
    let value = row.get_ref(offset)?;
    if value == rusqlite::types::ValueRef::Null {
        return Ok(None);
    }
    let bytes: &[u8; 32] = value.as_blob()?.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            offset,
            rusqlite::types::Type::Blob,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid BLAKE3 fingerprint length",
            )
            .into(),
        )
    })?;
    Ok(Some(blake3::Hash::from_bytes(*bytes).to_hex().to_string()))
}

pub(super) fn cue_from_row(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<Option<CueSegment>> {
    let sheet: Option<String> = row.get(offset)?;
    sheet
        .map(|sheet| {
            Ok(CueSegment {
                sheet: sheet.into(),
                number: row.get(offset + 1)?,
                start_frame: {
                    let value: i64 = row.get(offset + 2)?;
                    u64::try_from(value)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(offset + 2, value))?
                },
                end_frame: row
                    .get::<_, Option<i64>>(offset + 3)?
                    .map(|value| {
                        u64::try_from(value).map_err(|_| {
                            rusqlite::Error::IntegralValueOutOfRange(offset + 3, value)
                        })
                    })
                    .transpose()?,
            })
        })
        .transpose()
}

pub(super) const TRACK_COLUMNS: &str = "id,path,COALESCE(title_override,raw_title),COALESCE(artist_override,raw_artist),COALESCE(album_override,raw_album),duration,codec,channels,sample_rate,missing,play_count,last_played,cue_sheet,cue_number,cue_start_frame,cue_end_frame,bitrate_bps,track_number,disc_number,bits_per_sample,release_date,favorite,fingerprint,decoder_hint,bitrate_exact";
/// Number of columns decoded by `track_from_row`; appended row payloads start here.
pub(super) const TRACK_COLUMN_COUNT: usize = 25;

pub(super) fn track_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Track> {
    Ok(Track {
        id: r.get(0)?,
        path: PathBuf::from(r.get::<_, String>(1)?),
        fingerprint: fingerprint_from_row(r, 22)?,
        title: r.get(2)?,
        artist: r.get(3)?,
        album: r.get(4)?,
        duration: r.get(5)?,
        codec: r.get(6)?,
        channels: r.get::<_, i64>(7)? as u16,
        sample_rate: r.get::<_, i64>(8)? as u32,
        bitrate_bps: r.get::<_, Option<i64>>(16)?.map(|value| value as u64),
        bitrate_exact: r.get(24)?,
        track_number: r.get(17)?,
        disc_number: r.get(18)?,
        bits_per_sample: r.get(19)?,
        release_date: r.get(20)?,
        favorite: r.get(21)?,
        decoder_hint: match r.get::<_, i64>(23)? {
            0 => DecoderHint::Auto,
            1 => DecoderHint::Native,
            2 => DecoderHint::Ffmpeg,
            _ => {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    23,
                    rusqlite::types::Type::Integer,
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Invalid track decoder hint",
                    )
                    .into(),
                ));
            }
        },
        missing: r.get::<_, i64>(9)? != 0,
        play_count: r.get::<_, i64>(10)? as u64,
        last_played: r.get(11)?,
        cue: cue_from_row(r, 12)?,
    })
}

pub(super) const LIBRARY_ROW_COLUMNS: &str = "id,COALESCE(title_override,raw_title),COALESCE(artist_override,raw_artist),COALESCE(album_override,raw_album),duration,favorite,missing,play_count,codec,bitrate_bps,bitrate_exact,path,channels,sample_rate,bits_per_sample,cue_number";
pub(super) const LIBRARY_ROW_COLUMNS_QUALIFIED: &str = "t.id,COALESCE(t.title_override,t.raw_title),COALESCE(t.artist_override,t.raw_artist),COALESCE(t.album_override,t.raw_album),t.duration,t.favorite,t.missing,t.play_count,t.codec,t.bitrate_bps,t.bitrate_exact,t.path,t.channels,t.sample_rate,t.bits_per_sample,t.cue_number";

pub(super) fn library_row_at(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<rivu_protocol::model::LibraryRow> {
    Ok(rivu_protocol::model::LibraryRow {
        id: row.get(offset)?,
        path: PathBuf::from(row.get::<_, String>(offset + 11)?),
        title: row.get(offset + 1)?,
        artist: row.get(offset + 2)?,
        album: row.get(offset + 3)?,
        duration: row.get(offset + 4)?,
        favorite: row.get(offset + 5)?,
        missing: row.get(offset + 6)?,
        play_count: row.get::<_, i64>(offset + 7)? as u64,
        codec: row.get(offset + 8)?,
        bitrate_bps: row
            .get::<_, Option<i64>>(offset + 9)?
            .map(|value| value as u64),
        bitrate_exact: row.get(offset + 10)?,
        channels: row.get::<_, i64>(offset + 12)? as u16,
        sample_rate: row.get::<_, i64>(offset + 13)? as u32,
        bits_per_sample: row.get(offset + 14)?,
        cue_number: row.get(offset + 15)?,
    })
}

pub(super) fn library_row_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<rivu_protocol::model::LibraryRow> {
    library_row_at(row, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_projection_column_count_matches_appended_payload_offset() {
        let store = super::super::Store::open(std::path::Path::new(":memory:")).unwrap();
        let statement = store
            .conn
            .prepare(&format!("SELECT {TRACK_COLUMNS} FROM tracks"))
            .unwrap();
        assert_eq!(statement.column_count(), TRACK_COLUMN_COUNT);
    }
}
