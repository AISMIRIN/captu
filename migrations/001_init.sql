-- Reference schema (informational only).
-- The authoritative runtime schema is defined and applied by src/db.rs::create_schema().
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS programs (
    id               INTEGER PRIMARY KEY,
    title            TEXT NOT NULL UNIQUE,
    normalized_title TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ts_files (
    id             INTEGER PRIMARY KEY,
    path           TEXT UNIQUE NOT NULL,
    filename       TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'pending'
                   CHECK(status IN ('pending', 'ingesting', 'done', 'error')),
    error_msg      TEXT,
    ingested_at    DATETIME,
    program_id     INTEGER REFERENCES programs(id),
    episode_number INTEGER,
    episode_title  TEXT,
    air_date       DATE
);

CREATE TABLE IF NOT EXISTS captions (
    id         INTEGER PRIMARY KEY,
    ts_file_id INTEGER NOT NULL REFERENCES ts_files(id),
    pts_start  INTEGER NOT NULL,
    pts_end    INTEGER NOT NULL,
    text       TEXT NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS captions_fts USING fts5(
    text,
    content=captions,
    content_rowid=id,
    tokenize='trigram'
);

CREATE TRIGGER IF NOT EXISTS captions_ai AFTER INSERT ON captions BEGIN
    INSERT INTO captions_fts(rowid, text) VALUES (new.id, new.text);
END;

-- Delete trigger: keep FTS in sync when captions are removed (e.g. reingest reset).
CREATE TRIGGER IF NOT EXISTS captions_ad AFTER DELETE ON captions BEGIN
    INSERT INTO captions_fts(captions_fts, rowid, text)
    VALUES ('delete', old.id, old.text);
END;

CREATE TABLE IF NOT EXISTS tags (
    id         INTEGER PRIMARY KEY,
    caption_id INTEGER NOT NULL REFERENCES captions(id),
    tag        TEXT NOT NULL,
    UNIQUE(caption_id, tag)
);
