use std::sync::Arc;

use anyhow::Result;
use congmiao_feedbot::{
    Config, ProviderRegistry,
    cache::AppCache,
    media::MediaProcessor,
    storage::Storage,
    telegram::{BotState, run},
};
use reqwest::Client;
use tokio::sync::Semaphore;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("congmiao_feedbot=info,warn")),
        )
        .init();
    let config = Config::from_env()?;
    tokio::fs::create_dir_all(&config.temp_dir).await?;
    let storage = Storage::connect(&config.database_url).await?;
    let cache = AppCache::new(config.redis_url.as_deref()).await;
    let registry = ProviderRegistry::new(&config)?;
    let client = Client::builder()
        .user_agent("CongmiaoFeedBot/0.1")
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let media = MediaProcessor::new(client, &config);
    let queue_size = config.max_queue_size.max(1);
    info!(providers = 5, "初始化完成");
    run(BotState {
        config,
        registry,
        media,
        storage,
        cache,
        queue: Arc::new(Semaphore::new(queue_size)),
    })
    .await
}
