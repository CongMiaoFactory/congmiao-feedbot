CREATE TABLE IF NOT EXISTS telegram_media_cache (
    cache_key TEXT NOT NULL,
    media_kind TEXT NOT NULL,
    file_id TEXT NOT NULL,
    file_unique_id TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (cache_key, media_kind)
);

