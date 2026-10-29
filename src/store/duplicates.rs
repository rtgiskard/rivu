use super::Store;
use super::rows::{TRACK_COLUMN_COUNT, TRACK_COLUMNS, fingerprint_from_row, track_from_row};
use anyhow::{Context, Result, bail};
use rivu_protocol::model::{DuplicateGroup, DuplicateMember, Page};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;

const DUPLICATE_MEMBER_COLUMNS: &str = "file_size,modified_ns,
    (title_override IS NOT NULL OR artist_override IS NOT NULL
     OR album_override IS NOT NULL OR decoder_hint<>0),
    (SELECT COUNT(*) FROM playlist_entries WHERE playlist_id>0 AND track_id=tracks.id),
    (SELECT COUNT(*) FROM playlist_entries WHERE playlist_id=0 AND track_id=tracks.id),
    relocation_independent";

pub(super) struct DuplicateGroupCount {
    changes: u64,
    total: usize,
}

fn duplicate_member_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DuplicateMember> {
    Ok(DuplicateMember {
        track: track_from_row(row)?,
        file_size: row.get::<_, i64>(TRACK_COLUMN_COUNT)? as u64,
        modified_ns: row.get(TRACK_COLUMN_COUNT + 1)?,
        has_overrides: row.get(TRACK_COLUMN_COUNT + 2)?,
        playlist_references: row.get::<_, i64>(TRACK_COLUMN_COUNT + 3)? as usize,
        queue_references: row.get::<_, i64>(TRACK_COLUMN_COUNT + 4)? as usize,
        independent: row.get(TRACK_COLUMN_COUNT + 5)?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RelocationIdentity {
    pub id: i64,
    pub path: PathBuf,
    pub fingerprint: String,
    pub file_size: u64,
    pub modified_ns: i64,
}

/// The Core must freshly hash the target and check stable file identity, source
/// absence and active playback before calling the database-only commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreparedRelocation {
    pub source: RelocationIdentity,
    pub target: RelocationIdentity,
}

struct RelocationRow {
    identity: RelocationIdentity,
    missing: bool,
    independent: bool,
    has_user_state: bool,
    cue_sheet: Option<String>,
}

fn relocation_row(conn: &Connection, id: i64) -> Result<RelocationRow> {
    let row = conn
        .query_row(
            "SELECT path,fingerprint,file_size,modified_ns,missing,relocation_independent,
                    cue_sheet,(title_override IS NOT NULL OR artist_override IS NOT NULL
                    OR album_override IS NOT NULL OR decoder_hint<>0 OR favorite<>0
                    OR play_count<>0 OR last_played IS NOT NULL
                    OR EXISTS(SELECT 1 FROM playlist_entries WHERE track_id=tracks.id))
             FROM tracks WHERE id=?1",
            [id],
            |row| {
                Ok((
                    PathBuf::from(row.get::<_, String>(0)?),
                    fingerprint_from_row(row, 1)?,
                    row.get::<_, i64>(2)? as u64,
                    row.get::<_, i64>(3)?,
                    row.get::<_, bool>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, bool>(7)?,
                ))
            },
        )
        .optional()?
        .with_context(|| format!("Track {id} no longer exists"))?;
    Ok(RelocationRow {
        identity: RelocationIdentity {
            id,
            path: row.0,
            fingerprint: row.1.context("Track has no cached content fingerprint")?,
            file_size: row.2,
            modified_ns: row.3,
        },
        missing: row.4,
        independent: row.5,
        cue_sheet: row.6,
        has_user_state: row.7,
    })
}

fn validate_pair(source: &RelocationRow, target: &RelocationRow) -> Result<()> {
    if source.identity.id == target.identity.id {
        bail!("Relocation source and target must be different tracks");
    }
    if source.cue_sheet.is_some() || target.cue_sheet.is_some() {
        bail!("Manual relocation supports ordinary files only");
    }
    if !source.missing || target.missing {
        bail!("Relocation requires a missing source and a present target");
    }
    if source.independent || target.independent {
        bail!("Track was explicitly kept independent");
    }
    if source.identity.fingerprint != target.identity.fingerprint {
        bail!("Relocation tracks no longer have the same cached fingerprint");
    }
    if target.has_user_state {
        bail!("Target has user state or playlist/queue references; relocation cannot merge it");
    }
    Ok(())
}

impl Store {
    fn duplicate_group_count(&self) -> Result<usize> {
        // Writable transactions bypass counts; read-only owners fence invalidation.
        let cacheable = self.revision_owned_counts || self.conn.is_autocommit();
        let changes = self.conn.total_changes();
        if cacheable
            && let Some(cached) = self.duplicate_group_count_cache.borrow().as_ref()
            && cached.changes == changes
        {
            return Ok(cached.total);
        }
        let total = self.conn.query_row(
            "SELECT COUNT(*) FROM (
                SELECT fingerprint FROM tracks INDEXED BY tracks_fingerprint
                WHERE cue_sheet IS NULL AND fingerprint IS NOT NULL
                GROUP BY fingerprint HAVING COUNT(*)>1
             )",
            [],
            |row| row.get::<_, i64>(0),
        )? as usize;
        if cacheable {
            *self.duplicate_group_count_cache.borrow_mut() =
                Some(DuplicateGroupCount { changes, total });
        }
        Ok(total)
    }

    pub fn duplicate_groups_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Page<DuplicateGroup>> {
        let total = self.duplicate_group_count()?;
        let mut query = self.conn.prepare(
            "SELECT fingerprint,COUNT(*),SUM(missing<>0),SUM(missing=0)
             FROM tracks INDEXED BY tracks_fingerprint
             WHERE cue_sheet IS NULL AND fingerprint IS NOT NULL
             GROUP BY fingerprint HAVING COUNT(*)>1 ORDER BY fingerprint LIMIT ?1 OFFSET ?2",
        )?;
        let rows = query
            .query_map(
                params![
                    limit.min(rivu_protocol::model::MAX_QUERY_ROWS) as i64,
                    offset.min(i64::MAX as usize) as i64
                ],
                |row| {
                    Ok(DuplicateGroup {
                        fingerprint: fingerprint_from_row(row, 0)?
                            .expect("non-null group fingerprint"),
                        total_members: row.get::<_, i64>(1)? as usize,
                        missing_members: row.get::<_, i64>(2)? as usize,
                        present_members: row.get::<_, i64>(3)? as usize,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page { total, rows })
    }

    pub fn duplicate_members_page(
        &self,
        fingerprint: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Page<DuplicateMember>> {
        let fingerprint = blake3::Hash::from_hex(fingerprint)
            .context("Invalid duplicate group BLAKE3 fingerprint")?;
        let total = self.conn.query_row(
            "SELECT COUNT(*) FROM tracks INDEXED BY tracks_fingerprint
             WHERE cue_sheet IS NULL AND fingerprint=?1",
            [fingerprint.as_bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let mut query = self.conn.prepare(&format!(
            "SELECT {TRACK_COLUMNS},{DUPLICATE_MEMBER_COLUMNS}
             FROM tracks INDEXED BY tracks_fingerprint
             WHERE cue_sheet IS NULL AND fingerprint=?1 ORDER BY missing DESC,id
             LIMIT ?2 OFFSET ?3"
        ))?;
        let rows = query
            .query_map(
                params![
                    fingerprint.as_bytes().as_slice(),
                    limit.min(rivu_protocol::model::MAX_QUERY_ROWS) as i64,
                    offset.min(i64::MAX as usize) as i64
                ],
                duplicate_member_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page { total, rows })
    }
    /// Bounded selected identities are facts, not a scan of uncached group pages.
    pub fn duplicate_member_selection(
        &self,
        fingerprint: &str,
        track_ids: &[i64],
    ) -> Result<Page<DuplicateMember>> {
        anyhow::ensure!(
            track_ids.len() <= rivu_protocol::model::MAX_QUERY_ROWS,
            "Duplicate selection exceeds query limit"
        );
        let fingerprint = blake3::Hash::from_hex(fingerprint)
            .context("Invalid duplicate group BLAKE3 fingerprint")?;
        let mut query = self.conn.prepare(&format!(
            "SELECT {TRACK_COLUMNS},{DUPLICATE_MEMBER_COLUMNS}
             FROM tracks WHERE id IN (SELECT value FROM json_each(?2))
             AND cue_sheet IS NULL AND fingerprint=?1 ORDER BY missing DESC,id"
        ))?;
        let rows = query
            .query_map(
                params![
                    fingerprint.as_bytes().as_slice(),
                    serde_json::to_string(track_ids)?
                ],
                duplicate_member_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page {
            total: rows.len(),
            rows,
        })
    }

    pub fn keep_independent(&self, track_id: i64) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let row = relocation_row(&tx, track_id)?;
        if row.cue_sheet.is_some() {
            bail!("Only ordinary files can be kept independent for relocation");
        }
        let updated = tx.execute(
            "UPDATE tracks SET relocation_independent=1
             WHERE id=?1 AND relocation_independent=0",
            [track_id],
        )?;
        tx.commit()?;
        Ok(updated != 0)
    }

    pub(crate) fn prepare_relocation(
        &self,
        source_id: i64,
        target_id: i64,
    ) -> Result<PreparedRelocation> {
        let tx = self.conn.unchecked_transaction()?;
        let source = relocation_row(&tx, source_id)?;
        let target = relocation_row(&tx, target_id)?;
        validate_pair(&source, &target)?;
        tx.commit()?;
        Ok(PreparedRelocation {
            source: source.identity,
            target: target.identity,
        })
    }

    pub(crate) fn commit_relocation(
        &self,
        prepared: &PreparedRelocation,
    ) -> Result<super::ScanImpacts> {
        let tx = self.conn.unchecked_transaction()?;
        let source = relocation_row(&tx, prepared.source.id)?;
        let target = relocation_row(&tx, prepared.target.id)?;
        validate_pair(&source, &target)?;
        if source.identity != prepared.source || target.identity != prepared.target {
            bail!("Relocation identity changed while the target was being verified");
        }
        // Snapshot the target's scanned raw metadata before releasing its unique
        // path. No target user state is transferred or discarded.
        tx.execute(
            "CREATE TEMP TABLE relocation_target AS
             SELECT path,fingerprint,file_size,modified_ns,raw_title,raw_artist,raw_album,
                    duration,codec,channels,sample_rate,probe_version,bitrate_bps,bitrate_exact,
                    track_number,disc_number,bits_per_sample,release_date
             FROM tracks WHERE id=?1",
            [target.identity.id],
        )?;
        let queue = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM playlist_entries WHERE playlist_id=0 AND track_id=?1)",
            [source.identity.id],
            |row| row.get(0),
        )?;
        let albums = tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM tracks AS t WHERE id IN (?1,?2) AND {}) OR EXISTS(SELECT 1 FROM tracks AS s CROSS JOIN temp.relocation_target AS t WHERE s.id=?1 AND {})", super::albums::eligible_member("t"), rivu_format::database::eligible_title("COALESCE(s.album_override,t.raw_album)")),
            params![source.identity.id, target.identity.id], |row| row.get(0),
        )?;
        tx.execute("DELETE FROM tracks WHERE id=?1", [target.identity.id])?;
        let updated = tx.execute(
            "UPDATE tracks SET path=t.path,fingerprint=t.fingerprint,
                file_size=t.file_size,modified_ns=t.modified_ns,
                raw_title=t.raw_title,raw_artist=t.raw_artist,raw_album=t.raw_album,
                duration=t.duration,codec=t.codec,channels=t.channels,sample_rate=t.sample_rate,
                probe_version=t.probe_version,bitrate_bps=t.bitrate_bps,bitrate_exact=t.bitrate_exact,
                track_number=t.track_number,disc_number=t.disc_number,
                bits_per_sample=t.bits_per_sample,release_date=t.release_date,missing=0
             FROM temp.relocation_target AS t WHERE tracks.id=?1",
            [source.identity.id],
        )?;
        super::changed(updated, "track", source.identity.id)?;
        tx.execute_batch("DROP TABLE temp.relocation_target")?;
        tx.commit()?;
        let mut cache = self.track_cache.borrow_mut();
        cache.entries.remove(&source.identity.id);
        cache.entries.remove(&target.identity.id);
        Ok(super::ScanImpacts {
            changed: true,
            library: true,
            structure: true,
            duplicate_groups: true,
            queue,
            albums,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivu_format::decoder_hint::DecoderHint;
    use std::path::Path;

    fn fixture() -> Result<Store> {
        let store = Store::open(Path::new(":memory:"))?;
        let hash = blake3::hash(b"same audio");
        store.conn.execute(
            "INSERT INTO tracks(id,path,fingerprint,file_size,modified_ns,missing,
                raw_title,raw_artist,raw_album,duration,codec,channels,sample_rate,
                bitrate_bps,bitrate_exact,track_number,disc_number,bits_per_sample,release_date,probe_version)
             VALUES(1,'/old/source.wav',?1,10,20,1,'Old','Artist','Album',1,'pcm',2,48000,
                    1536000,0,1,1,16,'2024',1),
                   (2,'/new/target.wav',?1,10,30,0,'Target','New artist','New album',2,'flac',2,96000,
                    300000,1,2,2,24,'2025',1)",
            [hash.as_bytes().as_slice()],
        )?;
        Ok(store)
    }

    #[test]
    fn group_totals_follow_local_writes_and_rollback() -> Result<()> {
        let store = Store::open(std::path::Path::new(":memory:"))?;
        let first = blake3::hash(b"first");
        let second = blake3::hash(b"second");
        store.conn.execute(
            "INSERT INTO tracks(id,path,fingerprint) VALUES
             (1,'/first/a',?1),(2,'/first/b',?1),
             (3,'/second/a',?2),(4,'/second/b',?2)",
            params![first.as_bytes().as_slice(), second.as_bytes().as_slice()],
        )?;
        let total = || -> Result<usize> { Ok(store.duplicate_groups_page(0, 0)?.total) };
        assert_eq!(total()?, 2);
        store
            .conn
            .execute("UPDATE tracks SET fingerprint=NULL WHERE id=2", [])?;
        assert_eq!(total()?, 1);
        store.conn.execute(
            "UPDATE tracks SET fingerprint=?1 WHERE id=2",
            [first.as_bytes().as_slice()],
        )?;
        assert_eq!(total()?, 2);
        let transaction = store.conn.unchecked_transaction()?;
        transaction.execute("UPDATE tracks SET fingerprint=NULL", [])?;
        assert_eq!(total()?, 0);
        transaction.rollback()?;
        assert_eq!(total()?, 2);
        store.conn.execute("DELETE FROM tracks WHERE id=2", [])?;
        assert_eq!(total()?, 1);
        Ok(())
    }

    #[test]
    fn ordinary_groups_and_members_are_bounded_read_only_and_exclude_cue_segments() -> Result<()> {
        let store = fixture()?;
        let hash = blake3::hash(b"same audio");
        store.conn.execute(
            "INSERT INTO tracks(path,fingerprint,cue_sheet,cue_number,cue_start_frame)
             VALUES('/new/target.wav',?1,'/album.cue',1,0),
                   ('/new/target.wav',?1,'/album.cue',2,75)",
            [hash.as_bytes().as_slice()],
        )?;
        let cue_only_hash = blake3::hash(b"cue album source");
        store.conn.execute(
            "INSERT INTO tracks(path,fingerprint,cue_sheet,cue_number,cue_start_frame)
             VALUES('/cue/source.wav',?1,'/cue/album.cue',1,0),
                   ('/cue/source.wav',?1,'/cue/album.cue',2,75)",
            [cue_only_hash.as_bytes().as_slice()],
        )?;
        store.edit_track(1, Some("Custom"), None, None, Some(DecoderHint::Native))?;
        store.set_favorite(&[1], true)?;
        store.increment_play_count(1)?;
        store.mark_played(1, 123)?;
        store.enqueue_tracks(&[1, 1])?;
        store.conn.execute_batch(
            "INSERT INTO playlists(id,name) VALUES(1,'Owned');
             INSERT INTO playlist_entries(playlist_id,track_id,position) VALUES(1,1,0);",
        )?;
        store.keep_independent(2)?;
        let before = store.queue_state()?;
        let groups = store.duplicate_groups_page(0, usize::MAX)?;
        assert_eq!(groups.total, 1);
        assert_eq!(groups.rows[0].total_members, 2);
        assert_eq!(groups.rows[0].missing_members, 1);
        assert_eq!(groups.rows[0].present_members, 1);
        let members = store.duplicate_members_page(&groups.rows[0].fingerprint, 0, 10)?;
        assert_eq!(members.total, 2);
        assert_eq!(members.rows[0].track.id, 1);
        assert_eq!(members.rows[0].track.title, "Custom");
        assert!(members.rows[0].has_overrides);
        assert!(members.rows[0].track.favorite);
        assert_eq!(members.rows[0].track.play_count, 1);
        assert_eq!(members.rows[0].track.last_played, Some(123));
        assert_eq!(members.rows[0].playlist_references, 1);
        assert_eq!(members.rows[0].queue_references, 2);
        assert!(members.rows[1].independent);
        assert!(store.duplicate_groups_page(1, 10)?.rows.is_empty());
        assert!(store.duplicate_groups_page(0, 0)?.rows.is_empty());
        assert_eq!(store.queue_state()?, before);
        assert_eq!(store.track_count()?, 6);

        for index in 0..rivu_protocol::model::MAX_QUERY_ROWS + 3 {
            let hash = blake3::hash(index.to_string().as_bytes());
            store.conn.execute(
                "INSERT INTO tracks(path,fingerprint) VALUES(?1,?3),(?2,?3)",
                params![
                    format!("/copies/{index}/a"),
                    format!("/copies/{index}/b"),
                    hash.as_bytes().as_slice()
                ],
            )?;
        }
        assert_eq!(
            store.duplicate_groups_page(0, usize::MAX)?.rows.len(),
            rivu_protocol::model::MAX_QUERY_ROWS
        );
        store.conn.execute(
            "INSERT INTO tracks(path,fingerprint) SELECT '/more/'||value,?1 FROM json_each(?2)",
            params![
                hash.as_bytes().as_slice(),
                serde_json::to_string(&(0..300).collect::<Vec<_>>())?
            ],
        )?;
        let members = store.duplicate_members_page(hash.to_hex().as_ref(), 0, usize::MAX)?;
        assert_eq!(members.total, 302);
        assert_eq!(members.rows.len(), rivu_protocol::model::MAX_QUERY_ROWS);
        assert_eq!(
            store
                .duplicate_members_page(hash.to_hex().as_ref(), 256, 256)?
                .rows
                .len(),
            46
        );
        assert!(store.duplicate_members_page("not a hash", 0, 10).is_err());
        Ok(())
    }

    #[test]
    fn relocation_retains_source_identity_current_user_state_and_all_references() -> Result<()> {
        let store = fixture()?;
        store.enqueue_tracks(&[1, 1])?;
        let ids = store.queue_ids()?;
        store.set_queue_current(Some(ids[1]))?;
        store.conn.execute_batch(
            "INSERT INTO playlists(id,name) VALUES(1,'Owned');
             INSERT INTO playlist_entries(playlist_id,track_id,position) VALUES(1,1,0);",
        )?;
        let prepared = store.prepare_relocation(1, 2)?;
        // Source edits while hashing are kept; only identity changes invalidate.
        store.edit_track(
            1,
            Some("Custom"),
            Some(""),
            Some("Owned"),
            Some(DecoderHint::Ffmpeg),
        )?;
        store.set_favorite(&[1], true)?;
        store.increment_play_count(1)?;
        store.mark_played(1, 123)?;
        let queue = store.queue_state()?;
        let before = store.track(1)?.unwrap();
        let target = store.track(2)?.unwrap();
        store.commit_relocation(&prepared)?;
        let retained = store.track(1)?.unwrap();
        assert_eq!(retained.id, before.id);
        assert_eq!(retained.path, target.path);
        assert_eq!(retained.title, before.title);
        assert_eq!(retained.artist, before.artist);
        assert_eq!(retained.album, before.album);
        assert_eq!(retained.favorite, before.favorite);
        assert_eq!(retained.play_count, before.play_count);
        assert_eq!(retained.last_played, before.last_played);
        assert_eq!(retained.decoder_hint, before.decoder_hint);
        assert!(!retained.missing);
        assert_eq!(retained.duration, target.duration);
        assert_eq!(retained.codec, target.codec);
        assert_eq!(retained.bitrate_bps, target.bitrate_bps);
        assert!(retained.bitrate_exact);
        assert_eq!(retained.sample_rate, target.sample_rate);
        assert_eq!(retained.bits_per_sample, target.bits_per_sample);
        assert_eq!(retained.release_date, target.release_date);
        assert!(store.track(2)?.is_none());
        assert_eq!(store.queue_ids()?, ids);
        assert_eq!(store.queue_state()?.current_id, queue.current_id);
        assert_eq!(
            store
                .conn
                .query_row("SELECT track_id FROM playlist_entries", [], |row| row
                    .get::<_, i64>(0))?,
            1
        );
        let known = store.stored_scan_files(&[PathBuf::from("/new")])?;
        assert_eq!(known[0].media.as_ref().unwrap().title, "Target");
        assert_eq!(known[0].size, prepared.target.file_size);
        assert_eq!(known[0].modified_ns, prepared.target.modified_ns);
        assert!(store.duplicate_groups_page(0, 10)?.rows.is_empty());
        Ok(())
    }

    #[test]
    fn relocation_rejects_every_target_state_even_when_added_after_preparation() -> Result<()> {
        for state in [
            "UPDATE tracks SET title_override='' WHERE id=2",
            "UPDATE tracks SET artist_override='' WHERE id=2",
            "UPDATE tracks SET album_override='' WHERE id=2",
            "UPDATE tracks SET decoder_hint=1 WHERE id=2",
            "UPDATE tracks SET favorite=1 WHERE id=2",
            "UPDATE tracks SET play_count=1 WHERE id=2",
            "UPDATE tracks SET last_played=0 WHERE id=2",
            "INSERT INTO playlist_entries(playlist_id,track_id,position) VALUES(0,2,0)",
            "INSERT INTO playlists(id,name) VALUES(1,'Target'); INSERT INTO playlist_entries(playlist_id,track_id,position) VALUES(1,2,0)",
        ] {
            let store = fixture()?;
            let prepared = store.prepare_relocation(1, 2)?;
            store.conn.execute_batch(state)?;
            let source = store.track(1)?.unwrap();
            let target = store.track(2)?.unwrap();
            assert!(store.prepare_relocation(1, 2).is_err(), "{state}");
            assert!(store.commit_relocation(&prepared).is_err(), "{state}");
            assert_eq!(store.track(1)?.unwrap(), source);
            assert_eq!(store.track(2)?.unwrap(), target);
        }
        Ok(())
    }

    #[test]
    fn relocation_revalidates_both_identities_eligibility_and_hash_at_commit() -> Result<()> {
        for mutation in [
            "UPDATE tracks SET path='/changed-source' WHERE id=1",
            "UPDATE tracks SET path='/changed-target' WHERE id=2",
            "UPDATE tracks SET file_size=file_size+1 WHERE id=1",
            "UPDATE tracks SET file_size=file_size+1 WHERE id=2",
            "UPDATE tracks SET modified_ns=modified_ns+1 WHERE id=1",
            "UPDATE tracks SET modified_ns=modified_ns+1 WHERE id=2",
            "UPDATE tracks SET fingerprint=zeroblob(32) WHERE id=1",
            "UPDATE tracks SET fingerprint=zeroblob(32) WHERE id=2",
            "UPDATE tracks SET missing=0 WHERE id=1",
            "UPDATE tracks SET missing=1 WHERE id=2",
            "UPDATE tracks SET relocation_independent=1 WHERE id=1",
            "UPDATE tracks SET relocation_independent=1 WHERE id=2",
            "UPDATE tracks SET cue_sheet='/album.cue',cue_number=1,cue_start_frame=0 WHERE id=1",
            "UPDATE tracks SET cue_sheet='/album.cue',cue_number=1,cue_start_frame=0 WHERE id=2",
        ] {
            let store = fixture()?;
            let prepared = store.prepare_relocation(1, 2)?;
            store.conn.execute_batch(mutation)?;
            let count = store.track_count()?;
            assert!(store.commit_relocation(&prepared).is_err(), "{mutation}");
            assert_eq!(store.track_count()?, count);
            assert!(store.track(1)?.is_some());
            assert!(store.track(2)?.is_some());
        }
        let store = fixture()?;
        assert!(store.prepare_relocation(1, 1).is_err());
        assert!(store.prepare_relocation(2, 1).is_err());
        Ok(())
    }

    #[test]
    fn relocation_failure_rolls_back_deleted_target_metadata_and_temporary_snapshot() -> Result<()>
    {
        let store = fixture()?;
        store.enqueue_tracks(&[1])?;
        let before = store.queue_state()?;
        let source = store.track(1)?.unwrap();
        let target = store.track(2)?.unwrap();
        let prepared = store.prepare_relocation(1, 2)?;
        store.conn.execute_batch(
            "CREATE TEMP TRIGGER reject_relocation BEFORE UPDATE OF path ON tracks
             WHEN old.id=1 BEGIN SELECT RAISE(ABORT,'injected failure'); END;",
        )?;
        assert!(store.commit_relocation(&prepared).is_err());
        assert_eq!(store.track(1)?.unwrap(), source);
        assert_eq!(store.track(2)?.unwrap(), target);
        assert_eq!(store.queue_state()?, before);
        store.conn.execute_batch("DROP TRIGGER reject_relocation")?;
        store.commit_relocation(&prepared)?;
        assert!(store.track(2)?.is_none());
        Ok(())
    }

    #[test]
    fn selected_member_facts_are_bounded_deduplicated_and_current() -> Result<()> {
        let store = fixture()?;
        let fingerprint = blake3::hash(b"same audio").to_hex().to_string();
        let selected = store.duplicate_member_selection(&fingerprint, &[2, 1, 2, 999])?;
        assert_eq!(
            selected
                .rows
                .iter()
                .map(|member| member.track.id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(selected.total, 2);
        store.enqueue_tracks(&[2])?;
        store.create_playlist_with_tracks("target refs", &[2])?;
        let selected = store.duplicate_member_selection(&fingerprint, &[2])?;
        assert_eq!(selected.rows[0].queue_references, 1);
        assert_eq!(selected.rows[0].playlist_references, 1);
        assert!(store.prepare_relocation(1, 2).is_err());
        assert!(
            store
                .duplicate_member_selection(
                    &fingerprint,
                    &vec![1; rivu_protocol::model::MAX_QUERY_ROWS + 1]
                )
                .is_err()
        );
        assert!(
            store
                .duplicate_member_selection(blake3::hash(b"different").to_hex().as_ref(), &[1, 2])?
                .rows
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn independent_choice_is_idempotent_and_persists_across_database_reopen() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("library.db");
        let store = Store::open(&path)?;
        let hash = blake3::hash(b"same audio");
        store.conn.execute(
            "INSERT INTO tracks(path,fingerprint,missing) VALUES('/source',?1,1),('/target',?1,0)",
            [hash.as_bytes().as_slice()],
        )?;
        assert!(store.keep_independent(2)?);
        assert!(!store.keep_independent(2)?);
        drop(store);
        let store = Store::open(&path)?;
        let members = store.duplicate_members_page(hash.to_hex().as_ref(), 0, 10)?;
        assert!(members.rows[1].independent);
        assert!(store.prepare_relocation(1, 2).is_err());
        Ok(())
    }
}
