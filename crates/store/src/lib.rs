mod crypto;

use async_trait::async_trait;
pub use crypto::SecretBox;
use feedbot_core::auth::{AuthMethod, Credential, CredentialStatus};
use feedbot_core::extract::{MediaKind, Scope};
use feedbot_core::job::{Job, JobState, MediaCacheEntry, Subscription};
use feedbot_core::options::OptionValues;
use feedbot_core::repo::{
    CachedPage, CredentialRepo, JobRepo, MediaCacheRepo, SeenRepo, SettingsRepo, Store, StoreError,
    StoredCredentialMeta, SubscriptionRepo,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};
use std::fmt::Display;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// Map any driver/parse error into [`StoreError::Db`].
trait DbResult<T> {
    fn db(self) -> Result<T, StoreError>;
}

impl<T, E: Display> DbResult<T> for Result<T, E> {
    fn db(self) -> Result<T, StoreError> {
        self.map_err(|e| StoreError::Db(e.to_string()))
    }
}

/// Typed column read with error mapping.
fn col<'r, T>(row: &'r SqliteRow, name: &str) -> Result<T, StoreError>
where
    T: sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
{
    row.try_get(name).db()
}

#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
    secrets: std::sync::Arc<SecretBox>,
}

impl SqliteStore {
    pub async fn connect(path: &Path, master_key_hex: &str) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.db()?;
        }
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .db()?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .db()?;
        sqlx::migrate!("../../migrations").run(&pool).await.db()?;
        Ok(Self {
            pool,
            secrets: std::sync::Arc::new(SecretBox::from_hex_key(master_key_hex)?),
        })
    }

    // Timestamps are RFC 3339 text. Their fractional-second width varies, so
    // every comparison or ordering goes through `julianday()` rather than
    // lexicographic string order.
    fn parse_time(s: &str) -> Result<OffsetDateTime, StoreError> {
        OffsetDateTime::parse(s, &Rfc3339).db()
    }

    fn fmt_time(t: OffsetDateTime) -> String {
        t.format(&Rfc3339).unwrap_or_else(|_| t.to_string())
    }

    fn now() -> String {
        Self::fmt_time(OffsetDateTime::now_utc())
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

    fn uuid(row: &SqliteRow, name: &str) -> Result<Uuid, StoreError> {
        Uuid::parse_str(&col::<String>(row, name)?).db()
    }

    /// Associated data binding a credential ciphertext to its row identity.
    fn credential_aad(extractor_id: &str, scope: Scope, scope_id: i64) -> Vec<u8> {
        format!(
            "feedbot-credential:{extractor_id}:{}:{scope_id}",
            scope.as_str()
        )
        .into_bytes()
    }

    fn cache_from_row(row: &SqliteRow) -> Result<MediaCacheEntry, StoreError> {
        Ok(MediaCacheEntry {
            source_key: col(row, "source_key")?,
            idx: col(row, "idx")?,
            file_id: col(row, "file_id")?,
            file_unique_id: col(row, "file_unique_id")?,
            kind: Self::parse_kind(&col::<String>(row, "kind")?)?,
            size: col(row, "size")?,
        })
    }

    fn job_from_row(row: &SqliteRow) -> Result<Job, StoreError> {
        let state: String = col(row, "state")?;
        Ok(Job {
            id: Self::uuid(row, "id")?,
            chat_id: col(row, "chat_id")?,
            user_id: col(row, "user_id")?,
            message_id: col(row, "message_id")?,
            url: col::<String>(row, "url")?.parse().db()?,
            raw_args: col(row, "raw_args")?,
            state: JobState::parse(&state).ok_or(StoreError::Db(state))?,
            attempts: col(row, "attempts")?,
            last_error: col(row, "last_error")?,
            created_at: Self::parse_time(&col::<String>(row, "created_at")?)?,
        })
    }

    fn sub_from_row(row: &SqliteRow) -> Result<Subscription, StoreError> {
        Ok(Subscription {
            id: Self::uuid(row, "id")?,
            chat_id: col(row, "chat_id")?,
            user_id: col(row, "user_id")?,
            extractor_id: col(row, "extractor_id")?,
            source_ref: col(row, "source_ref")?,
            raw_args: col(row, "raw_args")?,
            cursor: col(row, "cursor")?,
            interval_secs: col(row, "interval_secs")?,
            enabled: col::<i64>(row, "enabled")? != 0,
            next_due_at: Self::parse_time(&col::<String>(row, "next_due_at")?)?,
            fail_count: col(row, "fail_count")?,
            needs_login: col::<i64>(row, "needs_login")? != 0,
        })
    }

    fn meta_from_row(row: &SqliteRow) -> Result<StoredCredentialMeta, StoreError> {
        let expires: Option<String> = col(row, "expires_at")?;
        Ok(StoredCredentialMeta {
            extractor_id: col(row, "extractor_id")?,
            scope: Self::parse_scope(&col::<String>(row, "scope")?)?,
            scope_id: col(row, "scope_id")?,
            method: Self::parse_method(&col::<String>(row, "method")?)?,
            expires_at: expires.as_deref().map(Self::parse_time).transpose()?,
            status: CredentialStatus::parse(&col::<String>(row, "status")?),
        })
    }

    fn credential_from_row(&self, row: &SqliteRow) -> Result<Credential, StoreError> {
        let meta = Self::meta_from_row(row)?;
        let nonce: Vec<u8> = col(row, "nonce")?;
        let ciphertext: Vec<u8> = col(row, "ciphertext")?;
        let aad = Self::credential_aad(&meta.extractor_id, meta.scope, meta.scope_id);
        let payload = self.secrets.decrypt(&nonce, &ciphertext, &aad)?;
        Ok(Credential {
            extractor_id: meta.extractor_id,
            method: meta.method,
            payload,
            expires_at: meta.expires_at,
        })
    }
}

#[async_trait]
impl JobRepo for SqliteStore {
    async fn insert_job(&self, job: &Job) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO jobs (id, chat_id, user_id, message_id, url, raw_args, state, attempts, last_error, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(job.id.to_string())
        .bind(job.chat_id)
        .bind(job.user_id)
        .bind(job.message_id)
        .bind(job.url.as_str())
        .bind(&job.raw_args)
        .bind(job.state.as_str())
        .bind(job.attempts)
        .bind(&job.last_error)
        .bind(Self::fmt_time(job.created_at))
        .execute(&self.pool)
        .await
        .db()?;
        Ok(())
    }

    async fn set_state(
        &self,
        id: Uuid,
        state: JobState,
        last_error: Option<&str>,
    ) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "UPDATE jobs SET state = ?, last_error = ?
             WHERE id = ? AND state NOT IN ('done', 'failed', 'cancelled')",
        )
        .bind(state.as_str())
        .bind(last_error)
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .db()?;
        Ok(res.rows_affected() > 0)
    }

    async fn bump_attempt(&self, id: Uuid) -> Result<(), StoreError> {
        sqlx::query("UPDATE jobs SET attempts = attempts + 1 WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .db()?;
        Ok(())
    }

    async fn unfinished(&self) -> Result<Vec<Job>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM jobs WHERE state NOT IN ('done', 'failed', 'cancelled') ORDER BY julianday(created_at)",
        )
        .fetch_all(&self.pool)
        .await
        .db()?;
        rows.iter().map(Self::job_from_row).collect()
    }

    async fn cancel_for_user(&self, user_id: i64) -> Result<u64, StoreError> {
        let res = sqlx::query(
            "UPDATE jobs SET state = 'cancelled'
             WHERE user_id = ? AND state NOT IN ('done', 'failed', 'cancelled')",
        )
        .bind(user_id)
        .execute(&self.pool)
        .await
        .db()?;
        Ok(res.rows_affected())
    }

    async fn prune_finished(&self, before: OffsetDateTime) -> Result<u64, StoreError> {
        let res = sqlx::query(
            "DELETE FROM jobs
             WHERE state IN ('done', 'failed', 'cancelled') AND julianday(created_at) < julianday(?)",
        )
        .bind(Self::fmt_time(before))
        .execute(&self.pool)
        .await
        .db()?;
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
        .db()?;
        rows.iter().map(Self::cache_from_row).collect()
    }

    async fn put_many(&self, entries: &[MediaCacheEntry]) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.db()?;
        let now = Self::now();
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
            .db()?;
        }
        tx.commit().await.db()?;
        Ok(())
    }

    async fn get_page(&self, source_key: &str) -> Result<Option<CachedPage>, StoreError> {
        let row = sqlx::query("SELECT url, items FROM page_cache WHERE source_key = ?")
            .bind(source_key)
            .fetch_optional(&self.pool)
            .await
            .db()?;
        row.map(|row| {
            Ok(CachedPage {
                url: col(&row, "url")?,
                items: col(&row, "items")?,
            })
        })
        .transpose()
    }

    async fn put_page(&self, source_key: &str, page: &CachedPage) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO page_cache (source_key, url, items, created_at) VALUES (?, ?, ?, ?)
             ON CONFLICT(source_key) DO UPDATE SET
                url = excluded.url, items = excluded.items, created_at = excluded.created_at",
        )
        .bind(source_key)
        .bind(&page.url)
        .bind(page.items)
        .bind(Self::now())
        .execute(&self.pool)
        .await
        .db()?;
        Ok(())
    }
}

#[async_trait]
impl SubscriptionRepo for SqliteStore {
    async fn insert_subscription(&self, sub: &Subscription) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO subscriptions (id, chat_id, user_id, extractor_id, source_ref, raw_args, cursor, interval_secs, enabled, next_due_at, fail_count, needs_login)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(sub.id.to_string())
        .bind(sub.chat_id)
        .bind(sub.user_id)
        .bind(&sub.extractor_id)
        .bind(&sub.source_ref)
        .bind(&sub.raw_args)
        .bind(&sub.cursor)
        .bind(sub.interval_secs)
        .bind(i64::from(sub.enabled))
        .bind(Self::fmt_time(sub.next_due_at))
        .bind(sub.fail_count)
        .bind(i64::from(sub.needs_login))
        .execute(&self.pool)
        .await
        .db()?;
        Ok(())
    }

    async fn due(&self, now: OffsetDateTime) -> Result<Vec<Subscription>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM subscriptions
             WHERE enabled = 1 AND needs_login = 0 AND julianday(next_due_at) <= julianday(?)
             ORDER BY julianday(next_due_at)",
        )
        .bind(Self::fmt_time(now))
        .fetch_all(&self.pool)
        .await
        .db()?;
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
        .bind(i64::from(enabled))
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .db()?;
        Ok(())
    }

    async fn pause_for_login(&self, id: Uuid) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "UPDATE subscriptions SET needs_login = 1 WHERE id = ? AND needs_login = 0",
        )
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .db()?;
        Ok(res.rows_affected() > 0)
    }

    async fn resume_after_login(
        &self,
        extractor_id: &str,
        user_id: Option<i64>,
        now: OffsetDateTime,
    ) -> Result<u64, StoreError> {
        let res = sqlx::query(
            "UPDATE subscriptions SET needs_login = 0, next_due_at = ?
             WHERE needs_login = 1 AND extractor_id = ? AND (? IS NULL OR user_id = ?)",
        )
        .bind(Self::fmt_time(now))
        .bind(extractor_id)
        .bind(user_id)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .db()?;
        Ok(res.rows_affected())
    }

    async fn list_for_chat(&self, chat_id: i64) -> Result<Vec<Subscription>, StoreError> {
        let rows = sqlx::query("SELECT * FROM subscriptions WHERE chat_id = ?")
            .bind(chat_id)
            .fetch_all(&self.pool)
            .await
            .db()?;
        rows.iter().map(Self::sub_from_row).collect()
    }

    async fn delete_subscription(&self, id: Uuid, chat_id: i64) -> Result<bool, StoreError> {
        let id = id.to_string();
        let mut tx = self.pool.begin().await.db()?;
        let res = sqlx::query("DELETE FROM subscriptions WHERE id = ? AND chat_id = ?")
            .bind(&id)
            .bind(chat_id)
            .execute(&mut *tx)
            .await
            .db()?;
        if res.rows_affected() > 0 {
            sqlx::query("DELETE FROM seen_items WHERE subscription_id = ?")
                .bind(&id)
                .execute(&mut *tx)
                .await
                .db()?;
        }
        tx.commit().await.db()?;
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
        .bind(Self::now())
        .execute(&self.pool)
        .await
        .db()?;
        Ok(res.rows_affected() > 0)
    }

    async fn forget_seen(&self, subscription_id: Uuid, item_key: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM seen_items WHERE subscription_id = ? AND item_key = ?")
            .bind(subscription_id.to_string())
            .bind(item_key)
            .execute(&self.pool)
            .await
            .db()?;
        Ok(())
    }
}

#[async_trait]
impl SettingsRepo for SqliteStore {
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
        .db()?;
        let mut values = OptionValues::new();
        for row in rows {
            let key: String = col(&row, "key")?;
            let value: String = col(&row, "value")?;
            values.insert(key, serde_json::from_str(&value).db()?);
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
        let mut tx = self.pool.begin().await.db()?;
        sqlx::query(
            "DELETE FROM platform_settings WHERE scope = ? AND scope_id = ? AND extractor_id = ?",
        )
        .bind(scope.as_str())
        .bind(scope_id)
        .bind(extractor_id)
        .execute(&mut *tx)
        .await
        .db()?;
        for (key, value) in values.iter() {
            sqlx::query(
                "INSERT INTO platform_settings (scope, scope_id, extractor_id, key, value) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(scope.as_str())
            .bind(scope_id)
            .bind(extractor_id)
            .bind(key)
            .bind(serde_json::to_string(value).db()?)
            .execute(&mut *tx)
            .await
            .db()?;
        }
        tx.commit().await.db()?;
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
        let aad = Self::credential_aad(&credential.extractor_id, scope, scope_id);
        let (nonce, ciphertext) = self.secrets.encrypt(&credential.payload, &aad)?;
        sqlx::query(
            "INSERT INTO credentials (id, extractor_id, scope, scope_id, method, nonce, ciphertext, expires_at, last_checked_at, status)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'valid')
             ON CONFLICT(extractor_id, scope, scope_id) DO UPDATE SET
                method = excluded.method,
                nonce = excluded.nonce,
                ciphertext = excluded.ciphertext,
                expires_at = excluded.expires_at,
                last_checked_at = excluded.last_checked_at,
                status = 'valid'",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&credential.extractor_id)
        .bind(scope.as_str())
        .bind(scope_id)
        .bind(credential.method.to_string())
        .bind(nonce)
        .bind(ciphertext)
        .bind(credential.expires_at.map(Self::fmt_time))
        .bind(Self::now())
        .execute(&self.pool)
        .await
        .db()?;
        Ok(())
    }

    async fn resolve(
        &self,
        extractor_id: &str,
        user_id: i64,
    ) -> Result<Option<Credential>, StoreError> {
        let row = sqlx::query(
            "SELECT * FROM credentials
             WHERE extractor_id = ?
               AND ((scope = 'user' AND scope_id = ?) OR scope = 'global')
             ORDER BY CASE scope WHEN 'user' THEN 0 ELSE 1 END
             LIMIT 1",
        )
        .bind(extractor_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .db()?;
        row.map(|row| self.credential_from_row(&row)).transpose()
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
        .db()?;
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
        .db()?;
        rows.iter().map(Self::meta_from_row).collect()
    }

    async fn due_for_check(
        &self,
        before: OffsetDateTime,
    ) -> Result<Vec<(StoredCredentialMeta, Credential)>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM credentials
             WHERE status NOT IN ('invalid', 'expired')
               AND (last_checked_at IS NULL OR julianday(last_checked_at) <= julianday(?))",
        )
        .bind(Self::fmt_time(before))
        .fetch_all(&self.pool)
        .await
        .db()?;
        let mut out = Vec::new();
        for row in rows {
            let meta = Self::meta_from_row(&row)?;
            // One undecryptable row (e.g. master key rotated) must not block the others.
            match self.credential_from_row(&row) {
                Ok(cred) => out.push((meta, cred)),
                Err(e) => tracing::warn!(
                    extractor = %meta.extractor_id,
                    scope = meta.scope.as_str(),
                    error = %e,
                    "skipping undecryptable credential"
                ),
            }
        }
        Ok(out)
    }

    async fn update_status(
        &self,
        extractor_id: &str,
        scope: Scope,
        scope_id: i64,
        status: CredentialStatus,
        credential: Option<&Credential>,
    ) -> Result<(), StoreError> {
        if let Some(credential) = credential {
            let aad = Self::credential_aad(extractor_id, scope, scope_id);
            let (nonce, ciphertext) = self.secrets.encrypt(&credential.payload, &aad)?;
            sqlx::query(
                "UPDATE credentials SET status = ?, nonce = ?, ciphertext = ?, last_checked_at = ?
                 WHERE extractor_id = ? AND scope = ? AND scope_id = ?",
            )
            .bind(status.as_str())
            .bind(nonce)
            .bind(ciphertext)
            .bind(Self::now())
            .bind(extractor_id)
            .bind(scope.as_str())
            .bind(scope_id)
            .execute(&self.pool)
            .await
            .db()?;
        } else {
            sqlx::query(
                "UPDATE credentials SET status = ?, last_checked_at = ?
                 WHERE extractor_id = ? AND scope = ? AND scope_id = ?",
            )
            .bind(status.as_str())
            .bind(Self::now())
            .bind(extractor_id)
            .bind(scope.as_str())
            .bind(scope_id)
            .execute(&self.pool)
            .await
            .db()?;
        }
        Ok(())
    }
}

impl Store for SqliteStore {}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    async fn store() -> (tempfile::TempDir, SqliteStore) {
        let dir = tempfile::tempdir().unwrap();
        let key = "11".repeat(32);
        let store = SqliteStore::connect(&dir.path().join("t.db"), &key)
            .await
            .unwrap();
        (dir, store)
    }

    fn job(user_id: i64) -> Job {
        Job {
            id: Uuid::new_v4(),
            chat_id: 1,
            user_id,
            message_id: 1,
            url: Url::parse("https://www.pixiv.net/artworks/1").unwrap(),
            raw_args: "+sp".into(),
            state: JobState::Queued,
            attempts: 0,
            last_error: None,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    fn cred(payload: &str) -> Credential {
        Credential {
            extractor_id: "pixiv".into(),
            method: AuthMethod::Cookie,
            payload: payload.as_bytes().to_vec(),
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn cancelled_jobs_are_terminal() {
        let (_dir, s) = store().await;
        let mine = job(7);
        let other = job(8);
        s.insert_job(&mine).await.unwrap();
        s.insert_job(&other).await.unwrap();
        assert_eq!(s.cancel_for_user(7).await.unwrap(), 1);
        // A worker trying to advance the cancelled job is told to stop.
        assert!(
            !s.set_state(mine.id, JobState::Extracting, None)
                .await
                .unwrap()
        );
        let left = s.unfinished().await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, other.id);
        assert_eq!(left[0].raw_args, "+sp");
    }

    #[tokio::test]
    async fn resolve_prefers_user_then_global_and_ignores_chat() {
        let (_dir, s) = store().await;
        s.save(Scope::Chat, 1, &cred("chat")).await.unwrap();
        assert!(s.resolve("pixiv", 7).await.unwrap().is_none());
        s.save(Scope::Global, 0, &cred("global")).await.unwrap();
        s.save(Scope::User, 7, &cred("mine")).await.unwrap();
        let mine = s.resolve("pixiv", 7).await.unwrap().unwrap();
        assert_eq!(mine.payload, b"mine");
        let fallback = s.resolve("pixiv", 8).await.unwrap().unwrap();
        assert_eq!(fallback.payload, b"global");
    }

    #[tokio::test]
    async fn credentials_needing_login_are_not_rechecked() {
        let (_dir, s) = store().await;
        s.save(Scope::User, 7, &cred("a")).await.unwrap();
        let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
        let due = s.due_for_check(later).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0.status, CredentialStatus::Valid);
        s.update_status("pixiv", Scope::User, 7, CredentialStatus::Expired, None)
            .await
            .unwrap();
        assert!(s.due_for_check(later).await.unwrap().is_empty());
        let listed = s.list_for_user(7).await.unwrap();
        assert_eq!(listed[0].status, CredentialStatus::Expired);
    }

    #[tokio::test]
    async fn forgotten_items_count_as_new_again() {
        let (_dir, s) = store().await;
        let sub = Uuid::new_v4();
        assert!(s.insert_if_new(sub, "1").await.unwrap());
        assert!(!s.insert_if_new(sub, "1").await.unwrap());
        s.forget_seen(sub, "1").await.unwrap();
        assert!(s.insert_if_new(sub, "1").await.unwrap());
    }

    fn subscription(user_id: i64, extractor_id: &str) -> Subscription {
        Subscription {
            id: Uuid::new_v4(),
            chat_id: 1,
            user_id,
            extractor_id: extractor_id.into(),
            source_ref: "https://x.com/jack".into(),
            raw_args: String::new(),
            cursor: None,
            interval_secs: 3600,
            enabled: true,
            next_due_at: OffsetDateTime::now_utc() - time::Duration::hours(1),
            fail_count: 0,
            needs_login: false,
        }
    }

    #[tokio::test]
    async fn page_cache_round_trip() {
        let (_dir, s) = store().await;
        assert_eq!(s.get_page("k").await.unwrap(), None);
        let first = CachedPage {
            url: "https://telegra.ph/a".into(),
            items: 30,
        };
        s.put_page("k", &first).await.unwrap();
        assert_eq!(s.get_page("k").await.unwrap(), Some(first));
        let second = CachedPage {
            url: "https://telegra.ph/b".into(),
            items: 31,
        };
        s.put_page("k", &second).await.unwrap();
        assert_eq!(s.get_page("k").await.unwrap(), Some(second));
    }

    #[tokio::test]
    async fn login_pause_and_resume() {
        let (_dir, s) = store().await;
        let now = OffsetDateTime::now_utc();
        let mine = subscription(7, "x");
        let other = subscription(8, "x");
        let pixiv = subscription(7, "pixiv");
        for sub in [&mine, &other, &pixiv] {
            s.insert_subscription(sub).await.unwrap();
            assert!(s.pause_for_login(sub.id).await.unwrap());
        }
        assert!(!s.pause_for_login(mine.id).await.unwrap(), "already paused");
        assert!(s.due(now).await.unwrap().is_empty());

        // A personal login resumes only that user's subscriptions of that site.
        assert_eq!(s.resume_after_login("x", Some(7), now).await.unwrap(), 1);
        let due: Vec<Uuid> = s.due(now).await.unwrap().iter().map(|d| d.id).collect();
        assert_eq!(due, [mine.id]);

        // A global login resumes the rest of that site.
        assert_eq!(s.resume_after_login("x", None, now).await.unwrap(), 1);
        assert_eq!(s.due(now).await.unwrap().len(), 2);
        assert!(
            s.list_for_chat(1)
                .await
                .unwrap()
                .iter()
                .any(|d| d.needs_login)
        );
    }
}
