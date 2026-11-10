-- Inline option text must survive restarts so recovered jobs keep `+q:...` etc.
ALTER TABLE jobs ADD COLUMN raw_args TEXT NOT NULL DEFAULT '';
-- Subscriptions keep the `/sub` option text the same way.
ALTER TABLE subscriptions ADD COLUMN raw_args TEXT NOT NULL DEFAULT '';

CREATE INDEX idx_credentials_check ON credentials(status, last_checked_at);

-- Older builds stored the source URL as cursor without filling seen_items;
-- reset so the next poll only records existing items instead of pushing them.
UPDATE subscriptions SET cursor = NULL;
