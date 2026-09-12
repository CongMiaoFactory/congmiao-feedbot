mod crypto;

use async_trait::async_trait;
pub use crypto::SecretBox;
use feedbot_core::auth::{AuthMethod, Credential};
use feedbot_core::extract::{MediaKind, Scope};
use feedbot_core::job::{ChatSettings, Job, JobState, MediaCacheEntry, Subscription};
use feedbot_core::options::OptionValues;
use feedbot_core::repo::{
    CredentialRepo, JobRepo, MediaCacheRepo, SeenRepo, SettingsRepo, Store, StoreError,
    StoredCredentialMeta, SubscriptionRepo,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
    secrets: std::sync::Arc<SecretBox>,
}

impl SqliteStore {
    pub async fn connect(path: &Path, master_key_hex: &str) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| StoreError::Db(e.to_string()))?;
        }
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .map_err(|e| StoreError::Db(e.to_string()))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(Self {
            pool,
            secrets: std::sync::Arc::new(SecretBox::from_hex_key(master_key_hex)?),
        })
    }

    fn parse_time(s: &str) -> Result<OffsetDateTime, StoreError> {
        OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
            .map_err(|e| StoreError::Db(e.to_string()))
    }

    fn fmt_time(t: OffsetDateTime) -> String {
        t.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| t.to_string())
    }

    fn parse_scope(s: &str) -> Result<Scope, StoreError> {
        match s {
            "user" => Ok(Scope::User),
            "chat" => Ok(Scope::Chat),
            "global" => Ok(Scope::Global),
            other => Err(StoreError::Db(format!("unknown scope {other}"))),
        }
    }

    fn parse_method(s: &str) -> Result<AuthMethod, StoreError> {
        match s {
            "qr" | "qr_code" => Ok(AuthMethod::QrCode),
            "cookie" => Ok(AuthMethod::Cookie),
            "token" => Ok(AuthMethod::Token),
            "userpass" => Ok(AuthMethod::UserPass),
            other => Err(StoreError::Db(format!("unknown method {other}"))),
        }
    }

    fn parse_kind(s: &str) -> Result<MediaKind, StoreError> {
        match s {
            "photo" => Ok(MediaKind::Photo),
            "video" => Ok(MediaKind::Video),
            "animation" => Ok(MediaKind::Animation),
            "audio" => Ok(MediaKind::Audio),
            "document" => Ok(MediaKind::Document),
            other => Err(StoreError::Db(format!("unknown kind {other}"))),
        }
    }

    fn kind_str(k: MediaKind) -> &'static str {
        match k {
            MediaKind::Photo => "photo",
            MediaKind::Video => "video",
            MediaKind::Animation => "animation",
            MediaKind::Audio => "audio",
            MediaKind::Document => "document",
        }
    }

    fn cache_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<MediaCacheEntry, StoreError> {
        Ok(MediaCacheEntry {
            source_key: row
                .try_get("source_key")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            idx: row
                .try_get("idx")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            file_id: row
                .try_get("file_id")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            file_unique_id: row
                .try_get("file_unique_id")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            kind: Self::parse_kind(
                &row.try_get::<String, _>("kind")
                    .map_err(|e| StoreError::Db(e.to_string()))?,
            )?,
            size: row
                .try_get("size")
                .map_err(|e| StoreError::Db(e.to_string()))?,
        })
    }

    fn job_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Job, StoreError> {
        let options_json: String = row
            .try_get("options_json")
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let options: OptionValues =
            serde_json::from_str(&options_json).unwrap_or_else(|_| OptionValues::new());
        let url: String = row
            .try_get("url")
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let state: String = row
            .try_get("state")
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let created: String = row
            .try_get("created_at")
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(Job {
            id: Uuid::parse_str(
                &row.try_get::<String, _>("id")
                    .map_err(|e| StoreError::Db(e.to_string()))?,
            )
            .map_err(|e| StoreError::Db(e.to_string()))?,
            chat_id: row
                .try_get("chat_id")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            user_id: row
                .try_get("user_id")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            message_id: row
                .try_get("message_id")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            url: url
                .parse()
                .map_err(|e: url::ParseError| StoreError::Db(e.to_string()))?,
            options,
            state: JobState::parse(&state).ok_or(StoreError::Db(state))?,
            attempts: row
                .try_get("attempts")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            last_error: row
                .try_get("last_error")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            created_at: Self::parse_time(&created)?,
        })
    }

    fn sub_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Subscription, StoreError> {
        let options_json: String = row
            .try_get("options_json")
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(Subscription {
            id: Uuid::parse_str(
                &row.try_get::<String, _>("id")
                    .map_err(|e| StoreError::Db(e.to_string()))?,
            )
            .map_err(|e| StoreError::Db(e.to_string()))?,
            chat_id: row
                .try_get("chat_id")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            user_id: row
                .try_get("user_id")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            extractor_id: row
                .try_get("extractor_id")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            source_ref: row
                .try_get("source_ref")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            options: serde_json::from_str(&options_json).unwrap_or_else(|_| OptionValues::new()),
            cursor: row
                .try_get("cursor")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            interval_secs: row
                .try_get("interval_secs")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            enabled: row
                .try_get::<i64, _>("enabled")
                .map_err(|e| StoreError::Db(e.to_string()))?
                != 0,
            next_due_at: Self::parse_time(
                &row.try_get::<String, _>("next_due_at")
                    .map_err(|e| StoreError::Db(e.to_string()))?,
            )?,
            fail_count: row
                .try_get("fail_count")
                .map_err(|e| StoreError::Db(e.to_string()))?,
        })
    }
}

#[async_trait]
impl JobRepo for SqliteStore {
    async fn insert_job(&self, job: &Job) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO jobs (id, chat_id, user_id, message_id, url, options_json, state, attempts, last_error, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(job.id.to_string())
        .bind(job.chat_id)
        .bind(job.user_id)
        .bind(job.message_id)
        .bind(job.url.as_str())
        .bind(serde_json::to_string(&job.options).unwrap_or_else(|_| "{}".into()))
        .bind(job.state.as_str())
        .bind(job.attempts)
        .bind(&job.last_error)
        .bind(Self::fmt_time(job.created_at))
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(())
    }

    async fn set_state(
        &self,
        id: Uuid,
        state: JobState,
        last_error: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE jobs SET state = ?, last_error = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(last_error)
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(())
    }

    async fn bump_attempt(&self, id: Uuid) -> Result<(), StoreError> {
        sqlx::query("UPDATE jobs SET attempts = attempts + 1 WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(())
    }

    async fn unfinished(&self) -> Result<Vec<Job>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM jobs WHERE state NOT IN ('done', 'failed') ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Db(e.to_string()))?;
        rows.iter().map(Self::job_from_row).collect()
    }

    async fn get_job(&self, id: Uuid) -> Result<Option<Job>, StoreError> {
        let row = sqlx::query("SELECT * FROM jobs WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        row.map(|r| Self::job_from_row(&r)).transpose()
    }

    async fn cancel_queued_for_user(&self, user_id: i64) -> Result<u64, StoreError> {
        let res = sqlx::query("UPDATE jobs SET state = 'failed', last_error = 'cancelled' WHERE user_id = ? AND state = 'queued'")
            .bind(user_id)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(res.rows_affected())
    }
}

#[async_trait]
impl MediaCacheRepo for SqliteStore {
    async fn get_cache_all(&self, source_key: &str) -> Result<Vec<MediaCacheEntry>, StoreError> {
        let rows = sqlx::query(
            "SELECT source_key, idx, file_id, file_unique_id, kind, size
             FROM media_cache WHERE source_key = ? ORDER BY idx",
        )
        .bind(source_key)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Db(e.to_string()))?;
        rows.iter().map(Self::cache_from_row).collect()
    }

    async fn put_many(&self, entries: &[MediaCacheEntry]) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let now = Self::fmt_time(OffsetDateTime::now_utc());
        for entry in entries {
            sqlx::query(
                "INSERT INTO media_cache (source_key, idx, file_id, file_unique_id, kind, size, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(source_key, idx) DO UPDATE SET
                    file_id = excluded.file_id,
                    file_unique_id = excluded.file_unique_id,
                    kind = excluded.kind,
                    size = excluded.size",
            )
            .bind(&entry.source_key)
            .bind(entry.idx)
            .bind(&entry.file_id)
            .bind(&entry.file_unique_id)
            .bind(Self::kind_str(entry.kind))
            .bind(entry.size)
            .bind(&now)
            .execute(&mut *tx)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl SubscriptionRepo for SqliteStore {
    async fn insert_subscription(&self, sub: &Subscription) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO subscriptions (id, chat_id, user_id, extractor_id, source_ref, options_json, cursor, interval_secs, enabled, next_due_at, fail_count)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(sub.id.to_string())
        .bind(sub.chat_id)
        .bind(sub.user_id)
        .bind(&sub.extractor_id)
        .bind(&sub.source_ref)
        .bind(serde_json::to_string(&sub.options).unwrap_or_else(|_| "{}".into()))
        .bind(&sub.cursor)
        .bind(sub.interval_secs)
        .bind(if sub.enabled { 1 } else { 0 })
        .bind(Self::fmt_time(sub.next_due_at))
        .bind(sub.fail_count)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(())
    }

    async fn due(&self, now: OffsetDateTime) -> Result<Vec<Subscription>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM subscriptions WHERE enabled = 1 AND next_due_at <= ? ORDER BY next_due_at",
        )
        .bind(Self::fmt_time(now))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Db(e.to_string()))?;
        rows.iter().map(Self::sub_from_row).collect()
    }

    async fn mark_ran(
        &self,
        id: Uuid,
        cursor: Option<&str>,
        next_due_at: OffsetDateTime,
        fail_count: i32,
        enabled: bool,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE subscriptions SET cursor = ?, next_due_at = ?, fail_count = ?, enabled = ? WHERE id = ?",
        )
        .bind(cursor)
        .bind(Self::fmt_time(next_due_at))
        .bind(fail_count)
        .bind(if enabled { 1 } else { 0 })
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(())
    }

    async fn list_for_chat(&self, chat_id: i64) -> Result<Vec<Subscription>, StoreError> {
        let rows = sqlx::query("SELECT * FROM subscriptions WHERE chat_id = ?")
            .bind(chat_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        rows.iter().map(Self::sub_from_row).collect()
    }

    async fn delete_subscription(&self, id: Uuid, chat_id: i64) -> Result<bool, StoreError> {
        let res = sqlx::query("DELETE FROM subscriptions WHERE id = ? AND chat_id = ?")
            .bind(id.to_string())
            .bind(chat_id)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(res.rows_affected() > 0)
    }
}

#[async_trait]
impl SeenRepo for SqliteStore {
    async fn insert_if_new(
        &self,
        subscription_id: Uuid,
        item_key: &str,
    ) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "INSERT OR IGNORE INTO seen_items (subscription_id, item_key, seen_at) VALUES (?, ?, ?)",
        )
        .bind(subscription_id.to_string())
        .bind(item_key)
        .bind(Self::fmt_time(OffsetDateTime::now_utc()))
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(res.rows_affected() > 0)
    }
}

#[async_trait]
impl SettingsRepo for SqliteStore {
    async fn chat(&self, chat_id: i64) -> Result<ChatSettings, StoreError> {
        let row = sqlx::query("SELECT chat_id, locale FROM chat_settings WHERE chat_id = ?")
            .bind(chat_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(match row {
            Some(row) => ChatSettings {
                chat_id,
                locale: row
                    .try_get("locale")
                    .map_err(|e| StoreError::Db(e.to_string()))?,
            },
            None => ChatSettings {
                chat_id,
                locale: None,
            },
        })
    }

    async fn get_platform(
        &self,
        scope: Scope,
        scope_id: i64,
        extractor_id: &str,
    ) -> Result<OptionValues, StoreError> {
        let rows = sqlx::query(
            "SELECT key, value FROM platform_settings WHERE scope = ? AND scope_id = ? AND extractor_id = ?",
        )
        .bind(scope.as_str())
        .bind(scope_id)
        .bind(extractor_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Db(e.to_string()))?;
        let mut values = OptionValues::new();
        for row in rows {
            let key: String = row
                .try_get("key")
                .map_err(|e| StoreError::Db(e.to_string()))?;
            let value: String = row
                .try_get("value")
                .map_err(|e| StoreError::Db(e.to_string()))?;
            let parsed: feedbot_core::OptionValue =
                serde_json::from_str(&value).map_err(|e| StoreError::Db(e.to_string()))?;
            values.insert(key, parsed);
        }
        Ok(values)
    }

    async fn set_platform(
        &self,
        scope: Scope,
        scope_id: i64,
        extractor_id: &str,
        values: &OptionValues,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "DELETE FROM platform_settings WHERE scope = ? AND scope_id = ? AND extractor_id = ?",
        )
        .bind(scope.as_str())
        .bind(scope_id)
        .bind(extractor_id)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Db(e.to_string()))?;
        for (key, value) in values.iter() {
            sqlx::query(
                "INSERT INTO platform_settings (scope, scope_id, extractor_id, key, value) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(scope.as_str())
            .bind(scope_id)
            .bind(extractor_id)
            .bind(key)
            .bind(serde_json::to_string(value).unwrap_or_default())
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        }
        Ok(())
    }
}

#[async_trait]
impl CredentialRepo for SqliteStore {
    async fn save(
        &self,
        scope: Scope,
        scope_id: i64,
        credential: &Credential,
    ) -> Result<(), StoreError> {
        let (nonce, ciphertext) = self.secrets.encrypt(&credential.payload)?;
        let expires = credential.expires_at.map(Self::fmt_time);
        sqlx::query(
            "INSERT INTO credentials (id, extractor_id, scope, scope_id, method, nonce, ciphertext, expires_at, last_checked_at, status)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'valid')
             ON CONFLICT(extractor_id, scope, scope_id) DO UPDATE SET
                method = excluded.method,
                nonce = excluded.nonce,
                ciphertext = excluded.ciphertext,
                expires_at = excluded.expires_at,
                status = 'valid'",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&credential.extractor_id)
        .bind(scope.as_str())
        .bind(scope_id)
        .bind(credential.method.to_string())
        .bind(nonce)
        .bind(ciphertext)
        .bind(expires)
        .bind(Self::fmt_time(OffsetDateTime::now_utc()))
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(())
    }

    async fn resolve(
        &self,
        extractor_id: &str,
        user_id: i64,
        chat_id: i64,
    ) -> Result<Option<Credential>, StoreError> {
        for (scope, scope_id) in [
            (Scope::User, user_id),
            (Scope::Chat, chat_id),
            (Scope::Global, 0),
        ] {
            if let Some(c) = self.load_one(extractor_id, scope, scope_id).await? {
                return Ok(Some(c));
            }
        }
        Ok(None)
    }

    async fn delete_credential(
        &self,
        extractor_id: &str,
        scope: Scope,
        scope_id: i64,
    ) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "DELETE FROM credentials WHERE extractor_id = ? AND scope = ? AND scope_id = ?",
        )
        .bind(extractor_id)
        .bind(scope.as_str())
        .bind(scope_id)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(res.rows_affected() > 0)
    }

    async fn list_for_user(&self, user_id: i64) -> Result<Vec<StoredCredentialMeta>, StoreError> {
        let rows = sqlx::query(
            "SELECT extractor_id, scope, scope_id, method, expires_at, status FROM credentials
             WHERE (scope = 'user' AND scope_id = ?) OR scope = 'global'",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Db(e.to_string()))?;
        rows.iter().map(|row| self.meta_from_row(row)).collect()
    }

    async fn all(&self) -> Result<Vec<(StoredCredentialMeta, Credential)>, StoreError> {
        let rows = sqlx::query("SELECT * FROM credentials")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            let meta = self.meta_from_row(&row)?;
            if let Some(cred) = self.credential_from_row(&row)? {
                out.push((meta, cred));
            }
        }
        Ok(out)
    }

    async fn update_status(
        &self,
        extractor_id: &str,
        scope: Scope,
        scope_id: i64,
        status: &str,
        credential: Option<&Credential>,
    ) -> Result<(), StoreError> {
        if let Some(credential) = credential {
            let (nonce, ciphertext) = self.secrets.encrypt(&credential.payload)?;
            sqlx::query(
                "UPDATE credentials SET status = ?, nonce = ?, ciphertext = ?, last_checked_at = ?
                 WHERE extractor_id = ? AND scope = ? AND scope_id = ?",
            )
            .bind(status)
            .bind(nonce)
            .bind(ciphertext)
            .bind(Self::fmt_time(OffsetDateTime::now_utc()))
            .bind(extractor_id)
            .bind(scope.as_str())
            .bind(scope_id)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        } else {
            sqlx::query(
                "UPDATE credentials SET status = ?, last_checked_at = ?
                 WHERE extractor_id = ? AND scope = ? AND scope_id = ?",
            )
            .bind(status)
            .bind(Self::fmt_time(OffsetDateTime::now_utc()))
            .bind(extractor_id)
            .bind(scope.as_str())
            .bind(scope_id)
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        }
        Ok(())
    }
}

impl SqliteStore {
    async fn load_one(
        &self,
        extractor_id: &str,
        scope: Scope,
        scope_id: i64,
    ) -> Result<Option<Credential>, StoreError> {
        let row = sqlx::query(
            "SELECT * FROM credentials WHERE extractor_id = ? AND scope = ? AND scope_id = ?",
        )
        .bind(extractor_id)
        .bind(scope.as_str())
        .bind(scope_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| StoreError::Db(e.to_string()))?;
        match row {
            Some(row) => self.credential_from_row(&row),
            None => Ok(None),
        }
    }

    fn meta_from_row(
        &self,
        row: &sqlx::sqlite::SqliteRow,
    ) -> Result<StoredCredentialMeta, StoreError> {
        let expires: Option<String> = row
            .try_get("expires_at")
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(StoredCredentialMeta {
            extractor_id: row
                .try_get("extractor_id")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            scope: Self::parse_scope(
                &row.try_get::<String, _>("scope")
                    .map_err(|e| StoreError::Db(e.to_string()))?,
            )?,
            scope_id: row
                .try_get("scope_id")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            method: Self::parse_method(
                &row.try_get::<String, _>("method")
                    .map_err(|e| StoreError::Db(e.to_string()))?,
            )?,
            expires_at: expires.as_deref().map(Self::parse_time).transpose()?,
            status: row
                .try_get("status")
                .map_err(|e| StoreError::Db(e.to_string()))?,
        })
    }

    fn credential_from_row(
        &self,
        row: &sqlx::sqlite::SqliteRow,
    ) -> Result<Option<Credential>, StoreError> {
        let nonce: Vec<u8> = row
            .try_get("nonce")
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let ciphertext: Vec<u8> = row
            .try_get("ciphertext")
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let payload = self.secrets.decrypt(&nonce, &ciphertext)?;
        let expires: Option<String> = row
            .try_get("expires_at")
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(Some(Credential {
            extractor_id: row
                .try_get("extractor_id")
                .map_err(|e| StoreError::Db(e.to_string()))?,
            method: Self::parse_method(
                &row.try_get::<String, _>("method")
                    .map_err(|e| StoreError::Db(e.to_string()))?,
            )?,
            payload,
            expires_at: expires.as_deref().map(Self::parse_time).transpose()?,
        }))
    }
}

impl Store for SqliteStore {}
