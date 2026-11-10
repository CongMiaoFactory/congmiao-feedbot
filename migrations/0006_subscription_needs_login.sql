-- A poll that needs a login pauses the subscription instead of failing it;
-- saving a login for that platform resumes it.
ALTER TABLE subscriptions ADD COLUMN needs_login INTEGER NOT NULL DEFAULT 0;
