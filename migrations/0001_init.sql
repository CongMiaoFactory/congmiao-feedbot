-- media_cache: file_id reuse; source_key includes option fingerprint
CREATE TABLE media_cache (
    source_key TEXT PRIMARY KEY NOT NULL,
    file_id TEXT NOT NULL,
    file_unique_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);

CREATE TABLE jobs (
    id TEXT PRIMARY KEY NOT NULL,
    chat_id INTEGER NOT NULL,
    user_id INTEGER NOT NULL,
    message_id INTEGER NOT NULL,
    url TEXT NOT NULL,
    options_json TEXT NOT NULL DEFAULT '{}',
    state TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_jobs_state ON jobs(state);
CREATE INDEX idx_jobs_user ON jobs(user_id);

CREATE TABLE subscriptions (
    id TEXT PRIMARY KEY NOT NULL,
    chat_id INTEGER NOT NULL,
    user_id INTEGER NOT NULL,
    extractor_id TEXT NOT NULL,
    source_ref TEXT NOT NULL,
    options_json TEXT NOT NULL DEFAULT '{}',
    cursor TEXT,
    interval_secs INTEGER NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    next_due_at TEXT NOT NULL,
    fail_count INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_subs_due ON subscriptions(enabled, next_due_at);

CREATE TABLE seen_items (
    subscription_id TEXT NOT NULL,
    item_key TEXT NOT NULL,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (subscription_id, item_key)
);

CREATE TABLE chat_settings (
    chat_id INTEGER PRIMARY KEY NOT NULL,
    locale TEXT
);

CREATE TABLE platform_settings (
    scope TEXT NOT NULL,
    scope_id INTEGER NOT NULL,
    extractor_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (scope, scope_id, extractor_id, key)
);

CREATE TABLE credentials (
    id TEXT PRIMARY KEY NOT NULL,
    extractor_id TEXT NOT NULL,
    scope TEXT NOT NULL,
    scope_id INTEGER NOT NULL,
    method TEXT NOT NULL,
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    expires_at TEXT,
    last_checked_at TEXT,
    status TEXT NOT NULL DEFAULT 'unknown',
    UNIQUE (extractor_id, scope, scope_id)
);
