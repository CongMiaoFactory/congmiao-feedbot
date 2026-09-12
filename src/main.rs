mod config;

use anyhow::Context;
use config::AppConfig;
use feedbot_bot::login::LoginState;
use feedbot_bot::queue::{recover_unfinished, JobQueue};
use feedbot_bot::transport::{self, Transport};
use feedbot_bot::worker;
use feedbot_bot::{
    build_bot, semaphores, user_limiter, App, ChunkedAlbums, OutputSettings, OverflowSink,
    TelegramPublisher,
};
use feedbot_core::ExtractCtx;
use feedbot_extractors::{ExtractorRegistry, ReqwestClient};
use feedbot_feed::FeedScheduler;
use feedbot_media::{Downloader, FfmpegCli, YtdlpCli};
use feedbot_store::SqliteStore;
use std::sync::Arc;
use std::time::Duration;
use teloxide::dispatching::dialogue::InMemStorage;
use teloxide::dispatching::Dispatcher;
use teloxide::prelude::*;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = AppConfig::load().context("load config")?;
    init_tracing(&cfg.log.format);

    let token = cfg.token()?.to_string();
    let master_key = cfg.resolve_master_key()?;
    let api_url = cfg.api_url()?;

    let store = Arc::new(
        SqliteStore::connect(&cfg.database.path, &master_key)
            .await
            .context("open sqlite")?,
    );
    let registry = Arc::new(ExtractorRegistry::with_sites(
        feedbot_extractors::ExtractorSites {
            bilibili: feedbot_extractors::sites::bilibili::BilibiliEndpoints {
                passport_base: cfg.bilibili.passport_base.clone(),
                api_base: cfg.bilibili.api_base.clone(),
                live_api_base: cfg.bilibili.live_api_base.clone(),
            },
            netease: feedbot_extractors::sites::netease::NeteaseEndpoints {
                api_base: cfg.netease.api_base.clone(),
            },
        },
    ));
    let (timeout, connect) = cfg.http_timeouts();
    let http = Arc::new(
        ReqwestClient::new(&cfg.http.user_agent, timeout, connect)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?,
    );
    let downloader = Downloader::new(http.inner().clone());
    let ffmpeg = FfmpegCli::new(&cfg.ffmpeg.ffmpeg, &cfg.ffmpeg.ffprobe);
    let ytdlp = YtdlpCli::new(&cfg.ytdlp.bin);
    let extract_ctx = ExtractCtx::new(http.clone(), Arc::new(ytdlp.clone()));
    let bot = build_bot(&token, &api_url);

    let (jobs, job_rx) = JobQueue::channel(256);
    let jobs = jobs.with_store(store.clone());

    let (extract_limit, media_limit) =
        semaphores(cfg.limits.extract_permits, cfg.limits.media_permits);
    let user_limiter = user_limiter(cfg.limits.per_user_per_minute);

    let output = OutputSettings {
        album_max: cfg.output.album_max as usize,
        summary_threshold: cfg.output.summary_threshold as usize,
        max_upload_bytes: cfg.output.max_upload_bytes,
        max_photo_bytes: cfg.output.max_photo_bytes,
    };
    if cfg.output.overflow.eq_ignore_ascii_case("telegraph") {
        tracing::warn!("output.overflow=telegraph 尚未实现，回退为 chunk");
    }
    let overflow: Arc<dyn OverflowSink> = Arc::new(ChunkedAlbums::new(output.summary_threshold));

    let app = Arc::new(App {
        bot: bot.clone(),
        registry: registry.clone(),
        store: store.clone(),
        extract_ctx,
        downloader,
        ffmpeg,
        ytdlp,
        jobs: jobs.clone(),
        extract_limit,
        media_limit,
        user_limiter,
        admin_ids: cfg.telegram.admin_ids.clone(),
        progress_edit_interval: Duration::from_secs(cfg.limits.progress_edit_interval_secs),
        max_attempts: cfg.limits.max_attempts,
        http_client: http.inner().clone(),
        output,
        overflow,
    });

    feedbot_bot::commands::install_menu(&bot)
        .await
        .context("set_my_commands")?;

    recover_unfinished(&app, &jobs).await;

    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let feed_cancel = cancel.clone();
    let app_worker = app.clone();
    tokio::spawn(async move {
        worker::run_worker_pool(app_worker, job_rx, worker_cancel).await;
    });

    let publisher = TelegramPublisher::new(bot.clone());
    let scheduler = FeedScheduler::new(
        store,
        jobs,
        registry,
        publisher,
        Duration::from_secs(cfg.feed.tick_secs),
        cfg.feed.max_fail_count,
    );
    tokio::spawn(async move {
        scheduler.run(feed_cancel).await;
    });

    let transport = match cfg.telegram.mode.as_str() {
        "webhook" => Transport::Webhook {
            addr: cfg.webhook_addr()?,
            public_url: cfg.telegram.webhook.public_url.parse()?,
            secret_token: cfg.telegram.webhook.secret_token.clone(),
            drop_pending: cfg.telegram.webhook.drop_pending,
        },
        _ => Transport::Polling {
            timeout: Duration::from_secs(cfg.telegram.polling.timeout_secs),
            drop_pending: cfg.telegram.polling.drop_pending,
        },
    };

    let dispatcher = Dispatcher::builder(bot.clone(), feedbot_bot::handlers::schema())
        .dependencies(dptree::deps![
            app.clone(),
            InMemStorage::<LoginState>::new()
        ])
        .enable_ctrlc_handler()
        .build();

    let shutdown = dispatcher.shutdown_token();
    let cancel_for_signal = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("shutting down");
        cancel_for_signal.cancel();
        let _ = shutdown.shutdown();
    });

    transport::dispatch(bot, dispatcher, transport)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;

    Ok(())
}

fn init_tracing(format: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if format == "json" {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .pretty()
            .init();
    }
}
