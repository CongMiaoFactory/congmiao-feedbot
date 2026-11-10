-- Options are kept as raw text in `raw_args` (0003); the JSON copies and the
-- never-used chat_settings table are dropped.
ALTER TABLE jobs DROP COLUMN options_json;
ALTER TABLE subscriptions DROP COLUMN options_json;
DROP TABLE chat_settings;
