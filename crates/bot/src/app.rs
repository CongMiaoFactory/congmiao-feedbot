use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use teloxide::adaptors::throttle::Limits;
use teloxide::adaptors::{CacheMe, DefaultParseMode, Throttle};
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tokio::sync::Semaphore;

use crate::publisher::OverflowSink;
use crate::queue::JobQueue;
use crate::transport::Transport;
use feedbot_core::ExtractCtx;
use feedbot_extractors::ExtractorRegistry;
use feedbot_media::{Downloader, FfmpegCli, YtdlpCli};
use feedbot_store::SqliteStore;

pub type TgBot = DefaultParseMode<CacheMe<Throttle<Bot>>>;
pub type UserLimiter = RateLimiter<u64, DefaultKeyedStateStore<u64>, DefaultClock>;

#[derive(Debug, Clone)]
pub struct OutputSettings {
    pub album_max: usize,
    pub summary_threshold: usize,
    pub max_upload_bytes: u64,
    pub max_photo_bytes: u64,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self {
            album_max: 10,
            summary_threshold: 20,
            max_upload_bytes: 50 * 1024 * 1024,
            max_photo_bytes: 10 * 1024 * 1024,
        }
    }
}

pub struct App {
    pub bot: TgBot,
    pub registry: Arc<ExtractorRegistry>,
    pub store: Arc<SqliteStore>,
    pub extract_ctx: ExtractCtx,
    pub downloader: Downloader,
    pub ffmpeg: FfmpegCli,
    pub ytdlp: YtdlpCli,
    pub jobs: JobQueue,
    pub extract_limit: Arc<Semaphore>,
    pub media_limit: Arc<Semaphore>,
    pub user_limiter: Arc<UserLimiter>,
    pub admin_ids: Vec<i64>,
    pub progress_edit_interval: Duration,
    pub max_attempts: u32,
    pub http_client: reqwest::Client,
    pub output: OutputSettings,
    pub overflow: Arc<dyn OverflowSink>,
}

impl App {
    pub fn is_admin(&self, user_id: i64) -> bool {
        self.admin_ids.contains(&user_id)
    }
}

pub fn build_bot(token: &str, api_url: &url::Url) -> TgBot {
    let api = reqwest::Url::parse(api_url.as_str()).expect("telegram api_url");
    Bot::new(token)
        .set_api_url(api)
        .throttle(Limits::default())
        .cache_me()
        .parse_mode(ParseMode::Html)
}

pub fn user_limiter(per_minute: u32) -> Arc<UserLimiter> {
    let quota = Quota::per_minute(NonZeroU32::new(per_minute.max(1)).unwrap());
    Arc::new(RateLimiter::keyed(quota))
}

pub fn semaphores(extract: u32, media: u32) -> (Arc<Semaphore>, Arc<Semaphore>) {
    (
        Arc::new(Semaphore::new(extract.max(1) as usize)),
        Arc::new(Semaphore::new(media.max(1) as usize)),
    )
}

pub struct RuntimeParams {
    pub transport: Transport,
    pub extract_permits: u32,
    pub media_permits: u32,
    pub per_user_per_minute: u32,
    pub progress_edit_interval: Duration,
    pub max_attempts: u32,
    pub admin_ids: Vec<i64>,
}
