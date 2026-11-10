mod config;

use anyhow::Context;
use config::{AppConfig, OverflowMode, TelegramMode};
use feedbot_bot::login::LoginState;
use feedbot_bot::queue::{JobQueue, recover_unfinished};
use feedbot_bot::transport::{self, Transport};
use feedbot_bot::worker;
use feedbot_bot::{
    Access, App, ChunkedAlbums, MediaTools, OverflowSink, Pipeline, TelegramPublisher,
    TelegraphOverflow, build_bot, semaphores, user_limiter,
};
use feedbot_core::{ExtractCtx, HttpClient, MediaProcessor, Store, Ytdlp};
use feedbot_extractors::{
    BilibiliEndpoints, ExtractorRegistry, ExtractorSites, NeteaseEndpoints, ReqwestClient,
    XQueryIds,
};
use feedbot_feed::{FeedScheduler, FeedSettings};
use feedbot_media::{Downloader, FfmpegCli, YtdlpCli};
use feedbot_store::SqliteStore;
use std::sync::Arc;
use std::time::Duration;
use teloxide::dispatching::Dispatcher;
use teloxide::dispatching::dialogue::InMemStorage;
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

    let store: Arc<dyn Store> = Arc::new(
        SqliteStore::connect(&cfg.database.path, &master_key)
            .await
            .context("open sqlite")?,
    );
    let (timeout, connect) = cfg.http_timeouts();
    let http: Arc<dyn HttpClient> = Arc::new(
        ReqwestClient::new(&cfg.http.user_agent, timeout, connect)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?,
    );
    let ytdlp: Arc<dyn Ytdlp> = Arc::new(
        YtdlpCli::new(&cfg.ytdlp.bin)
            .with_download_timeout(Duration::from_secs(cfg.ytdlp.download_timeout_secs))
            .with_js_runtime(&cfg.ytdlp.js_runtime),
    );
    let registry = Arc::new(ExtractorRegistry::with_sites(ExtractorSites {
        http: http.clone(),
        ytdlp: ytdlp.clone(),
        bilibili: BilibiliEndpoints {
            passport_base: cfg.bilibili.passport_base.clone(),
            api_base: cfg.bilibili.api_base.clone(),
            live_api_base: cfg.bilibili.live_api_base.clone(),
        },
        netease: NeteaseEndpoints {
            api_base: cfg.netease.api_base.clone(),
        },
        x: XQueryIds {
            tweet_result_by_rest_id: cfg.x.tweet_result_by_rest_id.clone(),
            user_by_screen_name: cfg.x.user_by_screen_name.clone(),
            user_media: cfg.x.user_media.clone(),
        },
        max_upload_bytes: Some(cfg.output.max_upload_bytes),
    }));
    let extract_ctx = ExtractCtx::new(http, ytdlp);
    let downloader = Downloader::build(
        &cfg.http.user_agent,
        connect,
        timeout,
        cfg.output.max_download_bytes,
    )?;
    let media: Arc<dyn MediaProcessor> = Arc::new(
        FfmpegCli::new(&cfg.ffmpeg.ffmpeg, &cfg.ffmpeg.ffprobe)
            .with_timeout(Duration::from_secs(cfg.ffmpeg.timeout_secs)),
    );
    let bot = build_bot(&token, &api_url);

    let (jobs, job_rx) = JobQueue::channel(256, store.clone());

    let (extract_limit, media_limit) =
        semaphores(cfg.limits.extract_permits, cfg.limits.media_permits);
    let user_limiter = user_limiter(cfg.limits.per_user_per_minute);

    let summary_threshold = cfg.output.summary_threshold as usize;
    let overflow: Arc<dyn OverflowSink> = match cfg.output.overflow {
        OverflowMode::Chunk => Arc::new(ChunkedAlbums::new(summary_threshold)),
        OverflowMode::Telegraph => Arc::new(TelegraphOverflow::new(
            extract_ctx.http.clone(),
            store.clone(),
            cfg.telegraph_settings()?,
            summary_threshold,
        )),
    };

    let app = Arc::new(App {
        bot: bot.clone(),
        registry: registry.clone(),
        store: store.clone(),
        jobs: jobs.clone(),
        access: Access {
            admin_ids: cfg.telegram.admin_ids.clone(),
            user_limiter,
        },
        pipeline: Pipeline {
            media: MediaTools {
                downloader,
                processor: media,
                ytdlp: extract_ctx.ytdlp.clone(),
                limit: media_limit,
                max_upload_bytes: cfg.output.max_upload_bytes,
                max_photo_bytes: cfg.output.max_photo_bytes,
            },
            extract_ctx: extract_ctx.clone(),
            extract_limit,
            max_attempts: cfg.limits.max_attempts,
            album_max: cfg.output.album_max as usize,
            overflow,
        },
        sub_interval: Duration::from_secs(cfg.feed.sub_interval_secs),
    });

    feedbot_bot::commands::install_menu(&bot)
        .await
        .context("set_my_commands")?;

    let cancel = CancellationToken::new();
    tokio::spawn(worker::run_worker_pool(
        app.clone(),
        job_rx,
        cfg.limits.max_jobs,
        cancel.clone(),
    ));
    // Recovery waits for queue space, so it must not hold up startup.
    let recovering = app.clone();
    tokio::spawn(async move { recover_unfinished(&recovering).await });

    let scheduler = FeedScheduler::new(
        store,
        Arc::new(jobs),
        registry,
        Arc::new(TelegramPublisher::new(bot.clone())),
        extract_ctx,
        FeedSettings {
            tick: Duration::from_secs(cfg.feed.tick_secs.max(1)),
            max_fail_count: cfg.feed.max_fail_count,
            push_limit: cfg.feed.push_limit.max(1),
            poll_concurrency: cfg.feed.poll_concurrency.max(1),
            poll_timeout: Duration::from_secs(cfg.feed.poll_timeout_secs),
            credential_check_interval: Duration::from_secs(cfg.feed.credential_check_secs),
            job_retention: Duration::from_secs(cfg.feed.job_retention_days * 86_400),
            admin_ids: cfg.telegram.admin_ids.clone(),
        },
    );
    tokio::spawn(scheduler.run(cancel.clone()));

    let transport = match cfg.telegram.mode {
        TelegramMode::Webhook => Transport::Webhook {
            addr: cfg.webhook_addr()?,
            public_url: cfg.telegram.webhook.public_url.parse()?,
            secret_token: cfg.telegram.webhook.secret_token.clone(),
            drop_pending: cfg.telegram.webhook.drop_pending,
        },
        TelegramMode::Polling => Transport::Polling {
            timeout: Duration::from_secs(cfg.telegram.polling.timeout_secs),
            drop_pending: cfg.telegram.polling.drop_pending,
        },
    };

    let dispatcher = Dispatcher::builder(bot.clone(), feedbot_bot::handlers::schema())
        .dependencies(dptree::deps![
            app.clone(),
            InMemStorage::<LoginState>::new()
        ])
        .build();

    // One signal handler stops the dispatcher and the background loops together.
    let shutdown = dispatcher.shutdown_token();
    let cancel_for_signal = cancel.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutting down");
        cancel_for_signal.cancel();
        if let Ok(done) = shutdown.shutdown() {
            done.await;
        }
    });

    transport::dispatch(bot, dispatcher, transport)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;

    Ok(())
}

/// Ctrl-C, or SIGTERM from `docker stop`, systemd, and Kubernetes.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                return;
            }
            Err(e) => tracing::warn!(error = %e, "SIGTERM handler unavailable"),
        }
    }
    tokio::signal::ctrl_c().await.ok();
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
