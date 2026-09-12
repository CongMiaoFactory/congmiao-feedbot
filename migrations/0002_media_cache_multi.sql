CREATE TABLE media_cache_new (
    source_key TEXT NOT NULL,
    idx INTEGER NOT NULL,
    file_id TEXT NOT NULL,
    file_unique_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    PRIMARY KEY (source_key, idx)
);
INSERT INTO media_cache_new
    SELECT source_key, 0, file_id, file_unique_id, kind, size, created_at
    FROM media_cache;
DROP TABLE media_cache;
ALTER TABLE media_cache_new RENAME TO media_cache;
