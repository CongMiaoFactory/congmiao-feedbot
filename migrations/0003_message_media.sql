CREATE TABLE IF NOT EXISTS telegram_message_media (
    chat_id INTEGER NOT NULL,
    message_id INTEGER NOT NULL,
    canonical_url TEXT NOT NULL,
    media_cache_key TEXT NOT NULL,
    quality INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (chat_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_telegram_message_media_updated_at
    ON telegram_message_media(updated_at);
