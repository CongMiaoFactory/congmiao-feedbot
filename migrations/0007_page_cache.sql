-- Pages (Telegraph) published instead of albums, reused for the same source
-- and options while the item count is unchanged.
CREATE TABLE page_cache (
    source_key TEXT PRIMARY KEY,
    url TEXT NOT NULL,
    items INTEGER NOT NULL,
    created_at TEXT NOT NULL
);
