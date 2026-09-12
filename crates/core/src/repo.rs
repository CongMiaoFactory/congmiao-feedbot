//! Repository traits. Storage crate implements these; core stays backend-agnostic.

use crate::auth::{AuthMethod, Credential};
use crate::extract::Scope;
use crate::job::{ChatSettings, Job, JobState, MediaCacheEntry, Subscription};
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
    async fn set_state(
        &self,
        id: Uuid,
        state: JobState,
        last_error: Option<&str>,
    ) -> Result<(), StoreError>;
    async fn bump_attempt(&self, id: Uuid) -> Result<(), StoreError>;
    async fn unfinished(&self) -> Result<Vec<Job>, StoreError>;
    async fn get_job(&self, id: Uuid) -> Result<Option<Job>, StoreError>;
    async fn cancel_queued_for_user(&self, user_id: i64) -> Result<u64, StoreError>;
}

#[async_trait]
pub trait MediaCacheRepo: Send + Sync {
    async fn get_cache_all(&self, source_key: &str) -> Result<Vec<MediaCacheEntry>, StoreError>;
    async fn put_many(&self, entries: &[MediaCacheEntry]) -> Result<(), StoreError>;
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
}

#[async_trait]
pub trait SettingsRepo: Send + Sync {
    async fn chat(&self, chat_id: i64) -> Result<ChatSettings, StoreError>;
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
    pub status: String,
}

#[async_trait]
pub trait CredentialRepo: Send + Sync {
    async fn save(
        &self,
        scope: Scope,
        scope_id: i64,
        credential: &Credential,
    ) -> Result<(), StoreError>;
    /// Lookup in user → chat → global order.
    async fn resolve(
        &self,
        extractor_id: &str,
        user_id: i64,
        chat_id: i64,
    ) -> Result<Option<Credential>, StoreError>;
    async fn delete_credential(
        &self,
        extractor_id: &str,
        scope: Scope,
        scope_id: i64,
    ) -> Result<bool, StoreError>;
    async fn list_for_user(&self, user_id: i64) -> Result<Vec<StoredCredentialMeta>, StoreError>;
    async fn all(&self) -> Result<Vec<(StoredCredentialMeta, Credential)>, StoreError>;
    async fn update_status(
        &self,
        extractor_id: &str,
        scope: Scope,
        scope_id: i64,
        status: &str,
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
