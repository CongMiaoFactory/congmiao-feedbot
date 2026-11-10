//! Repository traits. Storage crate implements these; core stays backend-agnostic.

use crate::auth::{AuthMethod, Credential, CredentialStatus};
use crate::extract::Scope;
use crate::job::{Job, JobState, MediaCacheEntry, Subscription};
use crate::options::OptionValues;
use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("数据库错误: {0}")]
    Db(String),
    #[error("加密错误: {0}")]
    Crypto(String),
    #[error("未找到")]
    NotFound,
}

#[async_trait]
pub trait JobRepo: Send + Sync {
    async fn insert_job(&self, job: &Job) -> Result<(), StoreError>;
    /// Move a job to `state`. Returns `false` when the job is already terminal
    /// (done/failed/cancelled), so callers can stop working on it.
    async fn set_state(
        &self,
        id: Uuid,
        state: JobState,
        last_error: Option<&str>,
    ) -> Result<bool, StoreError>;
    async fn bump_attempt(&self, id: Uuid) -> Result<(), StoreError>;
    async fn unfinished(&self) -> Result<Vec<Job>, StoreError>;
    /// Move every non-terminal job of `user_id` to `cancelled`.
    async fn cancel_for_user(&self, user_id: i64) -> Result<u64, StoreError>;
    /// Delete terminal jobs created before `before`.
    async fn prune_finished(&self, before: OffsetDateTime) -> Result<u64, StoreError>;
}

#[async_trait]
pub trait MediaCacheRepo: Send + Sync {
    async fn get_cache_all(&self, source_key: &str) -> Result<Vec<MediaCacheEntry>, StoreError>;
    async fn put_many(&self, entries: &[MediaCacheEntry]) -> Result<(), StoreError>;
    /// Page published for `source_key` instead of albums (Telegraph).
    async fn get_page(&self, source_key: &str) -> Result<Option<CachedPage>, StoreError>;
    async fn put_page(&self, source_key: &str, page: &CachedPage) -> Result<(), StoreError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedPage {
    pub url: String,
    /// Item count when published; a different count means the post changed.
    pub items: i64,
}

#[async_trait]
pub trait SubscriptionRepo: Send + Sync {
    async fn insert_subscription(&self, sub: &Subscription) -> Result<(), StoreError>;
    async fn due(&self, now: OffsetDateTime) -> Result<Vec<Subscription>, StoreError>;
    async fn mark_ran(
        &self,
        id: Uuid,
        cursor: Option<&str>,
        next_due_at: OffsetDateTime,
        fail_count: i32,
        enabled: bool,
    ) -> Result<(), StoreError>;
    /// Pause a subscription until a login is saved; `false` if it already was.
    async fn pause_for_login(&self, id: Uuid) -> Result<bool, StoreError>;
    /// Resume subscriptions paused for `extractor_id` and make them due now.
    /// `user_id: None` (a global login) resumes them for every user.
    async fn resume_after_login(
        &self,
        extractor_id: &str,
        user_id: Option<i64>,
        now: OffsetDateTime,
    ) -> Result<u64, StoreError>;
    async fn list_for_chat(&self, chat_id: i64) -> Result<Vec<Subscription>, StoreError>;
    async fn delete_subscription(&self, id: Uuid, chat_id: i64) -> Result<bool, StoreError>;
}

#[async_trait]
pub trait SeenRepo: Send + Sync {
    async fn insert_if_new(
        &self,
        subscription_id: Uuid,
        item_key: &str,
    ) -> Result<bool, StoreError>;
    /// Undo `insert_if_new`, e.g. when the item could not be enqueued.
    async fn forget_seen(&self, subscription_id: Uuid, item_key: &str) -> Result<(), StoreError>;
}

#[async_trait]
pub trait SettingsRepo: Send + Sync {
    async fn get_platform(
        &self,
        scope: Scope,
        scope_id: i64,
        extractor_id: &str,
    ) -> Result<OptionValues, StoreError>;
    async fn set_platform(
        &self,
        scope: Scope,
        scope_id: i64,
        extractor_id: &str,
        values: &OptionValues,
    ) -> Result<(), StoreError>;
}

#[derive(Debug, Clone)]
pub struct StoredCredentialMeta {
    pub extractor_id: String,
    pub scope: Scope,
    pub scope_id: i64,
    pub method: AuthMethod,
    pub expires_at: Option<OffsetDateTime>,
    pub status: CredentialStatus,
}

#[async_trait]
pub trait CredentialRepo: Send + Sync {
    async fn save(
        &self,
        scope: Scope,
        scope_id: i64,
        credential: &Credential,
    ) -> Result<(), StoreError>;
    /// The user's own credential, falling back to the global one.
    async fn resolve(
        &self,
        extractor_id: &str,
        user_id: i64,
    ) -> Result<Option<Credential>, StoreError>;
    async fn delete_credential(
        &self,
        extractor_id: &str,
        scope: Scope,
        scope_id: i64,
    ) -> Result<bool, StoreError>;
    async fn list_for_user(&self, user_id: i64) -> Result<Vec<StoredCredentialMeta>, StoreError>;
    /// Credentials that do not need a new login and were last checked before `before`.
    async fn due_for_check(
        &self,
        before: OffsetDateTime,
    ) -> Result<Vec<(StoredCredentialMeta, Credential)>, StoreError>;
    async fn update_status(
        &self,
        extractor_id: &str,
        scope: Scope,
        scope_id: i64,
        status: CredentialStatus,
        credential: Option<&Credential>,
    ) -> Result<(), StoreError>;
}

#[async_trait]
pub trait Store:
    JobRepo + MediaCacheRepo + SubscriptionRepo + SeenRepo + SettingsRepo + CredentialRepo
{
}

/// Helper used by settings merge: user overrides chat overrides nothing (then spec).
pub async fn load_saved_options<S: SettingsRepo + ?Sized>(
    store: &S,
    extractor_id: &str,
    user_id: i64,
    chat_id: i64,
) -> Result<OptionValues, StoreError> {
    let mut merged = OptionValues::new();
    let chat = store
        .get_platform(Scope::Chat, chat_id, extractor_id)
        .await?;
    for (k, v) in chat.iter() {
        merged.insert(k, v.clone());
    }
    let user = store
        .get_platform(Scope::User, user_id, extractor_id)
        .await?;
    for (k, v) in user.iter() {
        merged.insert(k, v.clone());
    }
    Ok(merged)
}
