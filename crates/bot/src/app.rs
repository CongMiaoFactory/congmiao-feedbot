use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use teloxide::adaptors::throttle::Limits;
use teloxide::adaptors::{CacheMe, DefaultParseMode, Throttle};
use teloxide::prelude::*;
use teloxide::types::{ChatId, ParseMode, UserId};
use tokio::sync::Semaphore;

use crate::publisher::OverflowSink;
use crate::queue::JobQueue;
use feedbot_core::{ExtractCtx, MediaProcessor, Store, Ytdlp};
use feedbot_extractors::ExtractorRegistry;
use feedbot_media::Downloader;

pub type TgBot = DefaultParseMode<CacheMe<Throttle<Bot>>>;
pub type UserLimiter = RateLimiter<u64, DefaultKeyedStateStore<u64>, DefaultClock>;

pub struct App {
    pub bot: TgBot,
    pub registry: Arc<ExtractorRegistry>,
    pub store: Arc<dyn Store>,
    pub jobs: JobQueue,
    /// Who may do what, and how often (handlers and commands).
    pub access: Access,
    /// Everything the worker needs to run a job.
    pub pipeline: Pipeline,
    /// Poll interval given to new subscriptions.
    pub sub_interval: Duration,
}

pub struct Access {
    /// Bot operators (`telegram.admin_ids`).
    pub admin_ids: Vec<i64>,
    /// Per-user rate limit on submitted links.
    pub user_limiter: Arc<UserLimiter>,
}

/// Extract → prepare → upload.
pub struct Pipeline {
    pub extract_ctx: ExtractCtx,
    pub extract_limit: Arc<Semaphore>,
    pub media: MediaTools,
    pub max_attempts: u32,
    pub album_max: usize,
    pub overflow: Arc<dyn OverflowSink>,
}

/// Download / transcode tooling, cloned into each per-item preparation task.
#[derive(Clone)]
pub struct MediaTools {
    pub downloader: Downloader,
    pub processor: Arc<dyn MediaProcessor>,
    pub ytdlp: Arc<dyn Ytdlp>,
    /// Bounds concurrent downloads / transcodes across all jobs.
    pub limit: Arc<Semaphore>,
    pub max_upload_bytes: u64,
    pub max_photo_bytes: u64,
}

impl App {
    /// Bot operator listed in `telegram.admin_ids`.
    pub fn is_admin(&self, user_id: i64) -> bool {
        self.access.admin_ids.contains(&user_id)
    }

    /// Whether `user_id` may change shared state of `chat`: always in private
    /// chats; in groups only Telegram chat admins and bot operators.
    pub async fn can_manage_chat(&self, chat: ChatId, user_id: i64, private: bool) -> bool {
        if private || self.is_admin(user_id) {
            return true;
        }
        let Ok(uid) = u64::try_from(user_id) else {
            return false;
        };
        match self.bot.get_chat_member(chat, UserId(uid)).await {
            Ok(member) => member.is_privileged(),
            Err(e) => {
                tracing::warn!(error = %e, "get_chat_member failed");
                false
            }
        }
    }
}

pub fn build_bot(token: &str, api_url: &url::Url) -> TgBot {
    Bot::new(token)
        .set_api_url(api_url.clone())
        .throttle(Limits::default())
        .cache_me()
        .parse_mode(ParseMode::Html)
}

pub fn user_limiter(per_minute: u32) -> Arc<UserLimiter> {
    let quota = Quota::per_minute(NonZeroU32::new(per_minute.max(1)).unwrap_or(NonZeroU32::MIN));
    Arc::new(RateLimiter::keyed(quota))
}

pub fn semaphores(extract: u32, media: u32) -> (Arc<Semaphore>, Arc<Semaphore>) {
    (
        Arc::new(Semaphore::new(extract.max(1) as usize)),
        Arc::new(Semaphore::new(media.max(1) as usize)),
    )
}
