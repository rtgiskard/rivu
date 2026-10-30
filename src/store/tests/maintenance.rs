use super::*;

#[test]
fn maintenance_retries_after_reader_releases_and_keeps_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let store = Store::open(&path).unwrap();
    store
        .apply_scan(&scan(vec![rec("/music/a.wav", "hash", "A", 10.)]))
        .unwrap();
    let id = all_tracks(&store)[0].id;
    let reader = Connection::open(&path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT * FROM tracks;")
        .unwrap();
    store.mark_played(id, 42).unwrap();
    store.conn.busy_timeout(std::time::Duration::ZERO).unwrap();
    assert!(store.optimize().is_err());
    reader.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(all_tracks(&reopened)[0].last_played, Some(42));
    reopened.optimize().unwrap();
    assert_eq!(all_tracks(&reopened)[0].last_played, Some(42));
}
