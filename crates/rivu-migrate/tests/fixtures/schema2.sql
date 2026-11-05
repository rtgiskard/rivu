-- Frozen schema 2: occurrence identities and album summaries, before catalog positions.
-- This historical input must not be regenerated from the current-schema constructor.
CREATE TABLE tracks(
    id INTEGER PRIMARY KEY AUTOINCREMENT,path TEXT NOT NULL,
    fingerprint BLOB CHECK(fingerprint IS NULL OR (typeof(fingerprint)='blob' AND length(fingerprint)=32)),
    file_size INTEGER NOT NULL DEFAULT 0,modified_ns INTEGER NOT NULL DEFAULT 0,
    raw_title TEXT NOT NULL DEFAULT '',raw_artist TEXT NOT NULL DEFAULT '',
    raw_album TEXT NOT NULL DEFAULT '',title_override TEXT,artist_override TEXT,
    album_override TEXT,duration REAL,codec TEXT NOT NULL DEFAULT '',
    channels INTEGER NOT NULL DEFAULT 0,sample_rate INTEGER NOT NULL DEFAULT 0,
    missing INTEGER NOT NULL DEFAULT 0,play_count INTEGER NOT NULL DEFAULT 0,
    last_played INTEGER,cue_sheet TEXT,cue_number INTEGER,
    cue_start_frame INTEGER,cue_end_frame INTEGER,
    probe_version INTEGER NOT NULL DEFAULT 0,
    bitrate_bps INTEGER,track_number INTEGER,disc_number INTEGER,
    bitrate_exact INTEGER NOT NULL DEFAULT 0 CHECK(bitrate_exact IN (0,1)),
    bits_per_sample INTEGER,release_date TEXT,
    favorite INTEGER NOT NULL DEFAULT 0 CHECK(favorite IN (0,1)),
    relocation_independent INTEGER NOT NULL DEFAULT 0 CHECK(relocation_independent IN (0,1)),
    decoder_hint INTEGER NOT NULL DEFAULT 0 CHECK(decoder_hint IN (0,1,2))
);
CREATE TABLE playlists(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL);
INSERT INTO playlists(id,name) VALUES(0,'');
CREATE TABLE playlist_entries(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,position INTEGER NOT NULL
);
CREATE INDEX playlist_order ON playlist_entries(playlist_id,position);
CREATE INDEX playlist_track_lookup ON playlist_entries(track_id,playlist_id);
CREATE TABLE rivu_meta(
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    format_version INTEGER NOT NULL,
    current_queue_entry_id INTEGER REFERENCES playlist_entries(id) ON DELETE SET NULL
);
INSERT INTO rivu_meta(singleton,format_version) VALUES(1,2);
CREATE INDEX tracks_fingerprint ON tracks(fingerprint);
CREATE UNIQUE INDEX tracks_path ON tracks(path, CASE WHEN cue_sheet IS NULL THEN 0 ELSE NULL END);
CREATE UNIQUE INDEX tracks_cue ON tracks(cue_sheet,cue_number) WHERE cue_sheet IS NOT NULL;
CREATE INDEX tracks_recent ON tracks(last_played DESC,id DESC) WHERE last_played IS NOT NULL;
CREATE INDEX tracks_album ON tracks(COALESCE(album_override,raw_album), CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(disc_number,4294967295) END, CASE WHEN COALESCE(album_override,raw_album)<>'' THEN COALESCE(track_number,4294967295) END, COALESCE(title_override,raw_title));
CREATE INDEX tracks_most_played ON tracks(play_count DESC);
CREATE TABLE albums(
    directory TEXT NOT NULL,title TEXT NOT NULL,
    track_count INTEGER NOT NULL,artist TEXT,
    PRIMARY KEY(directory,title)
) WITHOUT ROWID;
CREATE INDEX albums_title ON albums(title,directory);
CREATE INDEX albums_artist ON albums(COALESCE(artist,'Unknown artist'),title,directory);
CREATE TABLE album_artists(
    directory TEXT NOT NULL,title TEXT NOT NULL,artist TEXT NOT NULL,
    track_count INTEGER NOT NULL,
    PRIMARY KEY(directory,title,artist)
) WITHOUT ROWID;
CREATE INDEX tracks_album_members ON tracks(CASE WHEN rtrim(path,replace(path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(path,replace(path,'/','')),'/') END,COALESCE(album_override,raw_album),disc_number IS NULL,disc_number,track_number IS NULL,track_number,COALESCE(title_override,raw_title),id);

CREATE TRIGGER albums_insert AFTER INSERT ON tracks BEGIN INSERT INTO albums(directory,title,track_count) SELECT CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END,COALESCE(new.album_override,new.raw_album),1 WHERE trim(COALESCE(new.album_override,new.raw_album),char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>''
    ON CONFLICT(directory,title) DO UPDATE SET track_count=track_count+1;
    INSERT INTO album_artists(directory,title,artist,track_count)
    SELECT CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END,COALESCE(new.album_override,new.raw_album),COALESCE(new.artist_override,new.raw_artist),1 WHERE trim(COALESCE(new.album_override,new.raw_album),char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>'' AND COALESCE(new.artist_override,new.raw_artist)<>''
    ON CONFLICT(directory,title,artist) DO UPDATE SET track_count=track_count+1;
    UPDATE albums SET artist=CASE WHEN EXISTS(SELECT 1 FROM album_artists
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) LIMIT 1 OFFSET 1)
        THEN 'Various artists'
        ELSE (SELECT artist FROM album_artists
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) LIMIT 1) END
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) AND artist IS NOT (CASE WHEN EXISTS(SELECT 1 FROM album_artists
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) LIMIT 1 OFFSET 1)
        THEN 'Various artists'
        ELSE (SELECT artist FROM album_artists
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) LIMIT 1) END); END;
CREATE TRIGGER albums_delete AFTER DELETE ON tracks BEGIN UPDATE albums SET track_count=track_count-1 WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album);
    UPDATE album_artists SET track_count=track_count-1
    WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) AND artist=COALESCE(old.artist_override,old.raw_artist);
    DELETE FROM album_artists WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) AND artist=COALESCE(old.artist_override,old.raw_artist)
    AND track_count=0;
    DELETE FROM albums WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) AND track_count=0;
    UPDATE albums SET artist=CASE WHEN EXISTS(SELECT 1 FROM album_artists
        WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) LIMIT 1 OFFSET 1)
        THEN 'Various artists'
        ELSE (SELECT artist FROM album_artists
        WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) LIMIT 1) END
        WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) AND artist IS NOT (CASE WHEN EXISTS(SELECT 1 FROM album_artists
        WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) LIMIT 1 OFFSET 1)
        THEN 'Various artists'
        ELSE (SELECT artist FROM album_artists
        WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) LIMIT 1) END); END;
CREATE TRIGGER albums_update AFTER UPDATE OF path,raw_album,album_override,
    raw_artist,artist_override ON tracks
    WHEN CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END IS NOT CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END OR COALESCE(old.album_override,old.raw_album) IS NOT COALESCE(new.album_override,new.raw_album)
    BEGIN UPDATE albums SET track_count=track_count-1 WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album);
    UPDATE album_artists SET track_count=track_count-1
    WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) AND artist=COALESCE(old.artist_override,old.raw_artist);
    DELETE FROM album_artists WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) AND artist=COALESCE(old.artist_override,old.raw_artist)
    AND track_count=0;
    DELETE FROM albums WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) AND track_count=0;
    UPDATE albums SET artist=CASE WHEN EXISTS(SELECT 1 FROM album_artists
        WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) LIMIT 1 OFFSET 1)
        THEN 'Various artists'
        ELSE (SELECT artist FROM album_artists
        WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) LIMIT 1) END
        WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) AND artist IS NOT (CASE WHEN EXISTS(SELECT 1 FROM album_artists
        WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) LIMIT 1 OFFSET 1)
        THEN 'Various artists'
        ELSE (SELECT artist FROM album_artists
        WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) LIMIT 1) END); INSERT INTO albums(directory,title,track_count) SELECT CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END,COALESCE(new.album_override,new.raw_album),1 WHERE trim(COALESCE(new.album_override,new.raw_album),char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>''
    ON CONFLICT(directory,title) DO UPDATE SET track_count=track_count+1;
    INSERT INTO album_artists(directory,title,artist,track_count)
    SELECT CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END,COALESCE(new.album_override,new.raw_album),COALESCE(new.artist_override,new.raw_artist),1 WHERE trim(COALESCE(new.album_override,new.raw_album),char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>'' AND COALESCE(new.artist_override,new.raw_artist)<>''
    ON CONFLICT(directory,title,artist) DO UPDATE SET track_count=track_count+1;
    UPDATE albums SET artist=CASE WHEN EXISTS(SELECT 1 FROM album_artists
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) LIMIT 1 OFFSET 1)
        THEN 'Various artists'
        ELSE (SELECT artist FROM album_artists
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) LIMIT 1) END
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) AND artist IS NOT (CASE WHEN EXISTS(SELECT 1 FROM album_artists
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) LIMIT 1 OFFSET 1)
        THEN 'Various artists'
        ELSE (SELECT artist FROM album_artists
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) LIMIT 1) END); END;
CREATE TRIGGER albums_artist_update AFTER UPDATE OF path,raw_album,album_override,
    raw_artist,artist_override ON tracks
    WHEN CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END IS CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND COALESCE(old.album_override,old.raw_album) IS COALESCE(new.album_override,new.raw_album)
    AND COALESCE(old.artist_override,old.raw_artist) IS NOT COALESCE(new.artist_override,new.raw_artist) AND trim(COALESCE(new.album_override,new.raw_album),char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>''
    BEGIN UPDATE album_artists SET track_count=track_count-1
    WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) AND artist=COALESCE(old.artist_override,old.raw_artist);
    DELETE FROM album_artists WHERE directory=CASE WHEN rtrim(old.path,replace(old.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(old.path,replace(old.path,'/','')),'/') END AND title=COALESCE(old.album_override,old.raw_album) AND artist=COALESCE(old.artist_override,old.raw_artist)
    AND track_count=0; INSERT INTO album_artists(directory,title,artist,track_count)
    SELECT CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END,COALESCE(new.album_override,new.raw_album),COALESCE(new.artist_override,new.raw_artist),1 WHERE trim(COALESCE(new.album_override,new.raw_album),char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>'' AND COALESCE(new.artist_override,new.raw_artist)<>''
    ON CONFLICT(directory,title,artist) DO UPDATE SET track_count=track_count+1; UPDATE albums SET artist=CASE WHEN EXISTS(SELECT 1 FROM album_artists
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) LIMIT 1 OFFSET 1)
        THEN 'Various artists'
        ELSE (SELECT artist FROM album_artists
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) LIMIT 1) END
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) AND artist IS NOT (CASE WHEN EXISTS(SELECT 1 FROM album_artists
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) LIMIT 1 OFFSET 1)
        THEN 'Various artists'
        ELSE (SELECT artist FROM album_artists
        WHERE directory=CASE WHEN rtrim(new.path,replace(new.path,'/',''))='/' THEN '/' ELSE rtrim(rtrim(new.path,replace(new.path,'/','')),'/') END AND title=COALESCE(new.album_override,new.raw_album) LIMIT 1) END); END;
