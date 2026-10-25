-- Audio files index
CREATE TABLE IF NOT EXISTS sounds (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    path             TEXT    UNIQUE NOT NULL,
    filename         TEXT    NOT NULL,
    extension        TEXT    NOT NULL,
    size             INTEGER,
    duration         REAL,
    bitrate          INTEGER,
    sample_rate      INTEGER,
    bit_depth        INTEGER,
    collection       TEXT,
    folder_path      TEXT,
    source           TEXT CHECK(source IN ('sfx', 'music')),
    favorite         INTEGER NOT NULL DEFAULT 0,
    indexed_at       TEXT    NOT NULL,
    file_modified_at TEXT,
    rating           INTEGER NOT NULL DEFAULT 0,
    label            INTEGER NOT NULL DEFAULT 0
);

-- FTS5 full-text search virtual table
CREATE VIRTUAL TABLE IF NOT EXISTS sounds_fts USING fts5(
    filename,
    folder_path,
    collection,
    tags,
    content='sounds',
    content_rowid='id'
);

-- FTS5 sync triggers
CREATE TRIGGER IF NOT EXISTS sounds_ai AFTER INSERT ON sounds BEGIN
    INSERT INTO sounds_fts(rowid, filename, folder_path, collection, tags)
    VALUES (new.id, new.filename, new.folder_path, new.collection, '');
END;

CREATE TRIGGER IF NOT EXISTS sounds_ad AFTER DELETE ON sounds BEGIN
    INSERT INTO sounds_fts(sounds_fts, rowid, filename, folder_path, collection, tags)
    SELECT 'delete', old.id, old.filename, old.folder_path, old.collection,
           COALESCE((SELECT GROUP_CONCAT(tag, ' ') FROM tags WHERE sound_id = old.id ORDER BY tag), '')
    FROM (SELECT 1);
END;

-- Keep FTS in sync when the indexer updates an existing sound row (ON CONFLICT DO UPDATE
-- fires neither INSERT nor DELETE triggers, so we need an explicit UPDATE trigger).
CREATE TRIGGER IF NOT EXISTS sounds_au AFTER UPDATE OF filename, folder_path, collection ON sounds BEGIN
    INSERT INTO sounds_fts(sounds_fts, rowid, filename, folder_path, collection, tags)
    VALUES('delete', old.id, old.filename, old.folder_path, old.collection,
           COALESCE((SELECT GROUP_CONCAT(tag, ' ') FROM tags WHERE sound_id = old.id ORDER BY tag), ''));
    INSERT INTO sounds_fts(rowid, filename, folder_path, collection, tags)
    VALUES(new.id, new.filename, new.folder_path, new.collection,
           COALESCE((SELECT GROUP_CONCAT(tag, ' ') FROM tags WHERE sound_id = new.id ORDER BY tag), ''));
END;

-- User-assigned tags
CREATE TABLE IF NOT EXISTS tags (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    sound_id INTEGER NOT NULL,
    tag      TEXT    NOT NULL,
    UNIQUE(sound_id, tag),
    FOREIGN KEY(sound_id) REFERENCES sounds(id) ON DELETE CASCADE
);

-- AI-suggested tags (not searchable until confirmed)
CREATE TABLE IF NOT EXISTS suggested_tags (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    sound_id INTEGER NOT NULL,
    tag      TEXT    NOT NULL,
    score    REAL,
    UNIQUE(sound_id, tag),
    FOREIGN KEY(sound_id) REFERENCES sounds(id) ON DELETE CASCADE
);

-- CLAP vector embeddings (512 x float32 stored as BLOB)
CREATE TABLE IF NOT EXISTS embeddings (
    sound_id INTEGER PRIMARY KEY,
    vector   BLOB    NOT NULL,
    model    TEXT    NOT NULL DEFAULT 'laion-630k-legacy',
    FOREIGN KEY(sound_id) REFERENCES sounds(id) ON DELETE CASCADE
);

-- UMAP 2D coordinates for acoustic radar view
CREATE TABLE IF NOT EXISTS radar_coords (
    sound_id INTEGER PRIMARY KEY,
    x        REAL    NOT NULL,
    y        REAL    NOT NULL,
    FOREIGN KEY(sound_id) REFERENCES sounds(id) ON DELETE CASCADE
);

-- Injection history for co-occurrence recommendations
CREATE TABLE IF NOT EXISTS play_history (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    sound_id        INTEGER NOT NULL,
    timeline_id     TEXT,
    playhead_ms     INTEGER,
    injected_at     TEXT    NOT NULL,
    FOREIGN KEY(sound_id) REFERENCES sounds(id) ON DELETE CASCADE
);

-- Project-scoped tag bundles
CREATE TABLE IF NOT EXISTS tag_bundles (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT    UNIQUE NOT NULL,
    created_at TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS bundle_tags (
    bundle_id INTEGER NOT NULL,
    tag       TEXT    NOT NULL,
    PRIMARY KEY(bundle_id, tag),
    FOREIGN KEY(bundle_id) REFERENCES tag_bundles(id) ON DELETE CASCADE
);

-- Indexed root directories (used for path security validation in audio streaming)
CREATE TABLE IF NOT EXISTS indexed_roots (
    path   TEXT PRIMARY KEY,
    source TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS projects (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT    UNIQUE NOT NULL,
    color      TEXT    NOT NULL DEFAULT '#e47a2a',
    created_at TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS project_sounds (
    project_id INTEGER NOT NULL,
    sound_id   INTEGER NOT NULL,
    added_at   TEXT    NOT NULL,
    PRIMARY KEY(project_id, sound_id),
    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE,
    FOREIGN KEY(sound_id)   REFERENCES sounds(id)   ON DELETE CASCADE
);

-- State of background AI jobs (radar generate / umap) so failures are observable after the fact.
CREATE TABLE IF NOT EXISTS ai_jobs (
    name       TEXT PRIMARY KEY,
    state      TEXT NOT NULL,
    phase      TEXT NOT NULL DEFAULT '',
    done       INTEGER NOT NULL DEFAULT 0,
    total      INTEGER NOT NULL DEFAULT 0,
    failed     INTEGER NOT NULL DEFAULT 0,
    error      TEXT,
    updated_at TEXT
);
