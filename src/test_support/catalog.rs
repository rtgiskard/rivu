//! Genuine SQLite catalog setup for frontend behavior tests, without exposing Store.
use crate::{
    library::{ScanRecord, ScanResult},
    store::Store,
};
use anyhow::Result;
use rivu_audio::MediaInfo;
use rivu_protocol::model::{DirectoryRow, Page};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct CatalogFixture {
    store: Store,
}

impl CatalogFixture {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            store: Store::open(path)?,
        })
    }
    pub fn seed_tracks(&self, root: PathBuf, tracks: Vec<(PathBuf, String)>) -> Result<()> {
        self.store.apply_scan(&ScanResult {
            roots: vec![root],
            records: tracks
                .into_iter()
                .map(|(path, title)| {
                    Arc::new(ScanRecord {
                        path,
                        size: 1,
                        modified_ns: 0,
                        fingerprint: None,
                        media: MediaInfo {
                            title,
                            artist: String::new(),
                            album: String::new(),
                            duration: Some(1.0),
                            bitrate_bps: None,
                            bitrate_exact: false,
                            track_number: None,
                            disc_number: None,
                            bits_per_sample: None,
                            release_date: None,
                            codec: "PCM_S16LE".into(),
                            channels: 1,
                            sample_rate: 8000,
                        },
                    })
                })
                .collect(),
            ..Default::default()
        })?;
        Ok(())
    }
    pub fn track_id(&self, path: &Path) -> Result<Option<i64>> {
        self.store
            .track_id_for_identity(crate::store::TrackIdentity::File(path))
    }
    pub fn set_favorite(&self, ids: &[i64], favorite: bool) -> Result<()> {
        self.store.set_favorite(ids, favorite)?;
        Ok(())
    }
    pub fn mark_absent(&self, paths: Vec<PathBuf>) -> Result<()> {
        self.store.apply_scan(&ScanResult {
            roots: paths,
            ..Default::default()
        })?;
        Ok(())
    }
    pub fn directory_page(
        &self,
        path: &Path,
        query: Option<&str>,
        favorite: Option<bool>,
        missing: Option<bool>,
        offset: usize,
        limit: usize,
    ) -> Result<Page<DirectoryRow>> {
        self.store
            .directory_page(path, query, favorite, missing, offset, limit)
    }
}

pub fn seed_playlist_targets(data_dir: &Path, count: i64) -> Result<()> {
    let database = data_dir.join("library.db");
    let store = Store::open(&database)?;
    rusqlite::Connection::open(&database)?.execute(
        "INSERT INTO tracks(id,path,raw_title) VALUES(1,?1,'Source')",
        [data_dir
            .join("source.wav")
            .to_str()
            .expect("fixture path is UTF-8")],
    )?;
    for id in 1..=count {
        anyhow::ensure!(
            store.create_playlist(&format!("Target {id:03}"))? == id,
            "unexpected fixture playlist identity"
        );
    }
    Ok(())
}
