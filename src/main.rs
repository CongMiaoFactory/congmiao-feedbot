use std::sync::Arc;

use anyhow::Result;
use congmiao_feedbot::{
    Config, ProviderRegistry, RuntimeCredentials,
    cache::AppCache,
    login::LoginService,
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
    let credentials = RuntimeCredentials::load(storage.clone(), &config).await?;
    let cache = AppCache::new();
    let registry = ProviderRegistry::new_with_credentials(&config, credentials.clone())?;
    let login = LoginService::new(&config, credentials.clone())?;
    let client = Client::builder()
        .user_agent(concat!("CongmiaoFeedBot/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(
            config.media_download_timeout_secs,
        ))
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
        login,
        credentials,
        queue: Arc::new(Semaphore::new(queue_size)),
    })
    .await
}
