//! Subscription scheduler. Shares the extract/upload worker via [`WorkEnqueue`].

use feedbot_core::{
    AuthProvider, CredentialRepo, CredentialStatus, Publisher, StoreError, Subscription,
    SubscriptionRepo, WorkEnqueue,
};
use feedbot_extractors::ExtractorRegistry;
use feedbot_store::SqliteStore;
use std::sync::Arc;
use std::time::Duration;
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub struct FeedScheduler<Q, P> {
    store: Arc<SqliteStore>,
    queue: Q,
    registry: Arc<ExtractorRegistry>,
    publisher: P,
    tick: Duration,
    max_fail_count: i32,
}

impl<Q, P> FeedScheduler<Q, P>
where
    Q: WorkEnqueue + Clone,
    P: Publisher + Clone,
{
    pub fn new(
        store: Arc<SqliteStore>,
        queue: Q,
        registry: Arc<ExtractorRegistry>,
        publisher: P,
        tick: Duration,
        max_fail_count: i32,
    ) -> Self {
        Self {
            store,
            queue,
            registry,
            publisher,
            tick,
            max_fail_count,
        }
    }

    pub async fn run(self, cancel: CancellationToken) {
        let mut interval = tokio::time::interval(self.tick);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(e) = self.tick_once().await {
                        warn!(error = %e, "feed tick failed");
                    }
                    if let Err(e) = self.check_credentials().await {
                        warn!(error = %e, "credential check failed");
                    }
                }
            }
        }
    }

    async fn tick_once(&self) -> Result<(), StoreError> {
        let due = self.store.due(OffsetDateTime::now_utc()).await?;
        for sub in due {
            self.handle_sub(sub).await?;
        }
        Ok(())
    }

    async fn handle_sub(&self, sub: Subscription) -> Result<(), StoreError> {
        let now = OffsetDateTime::now_utc();
        let first_run = sub.cursor.is_none();
        let url = match url::Url::parse(&sub.source_ref) {
            Ok(u) => u,
            Err(_) => {
                self.fail(&sub, now).await?;
                return Ok(());
            }
        };
        if self.registry.match_url(&url).is_none() {
            self.fail(&sub, now).await?;
            return Ok(());
        }

        if first_run {
            self.store
                .mark_ran(
                    sub.id,
                    Some(sub.source_ref.as_str()),
                    now + TimeDuration::seconds(sub.interval_secs),
                    0,
                    true,
                )
                .await?;
            info!(sub = %sub.id, "initialized subscription cursor");
            return Ok(());
        }

        if let Err(e) = self
            .queue
            .enqueue_url(sub.chat_id, sub.user_id, url, String::new())
            .await
        {
            warn!(error = %e, "enqueue subscription job failed");
            self.fail(&sub, now).await?;
            return Ok(());
        }

        self.store
            .mark_ran(
                sub.id,
                Some(sub.source_ref.as_str()),
                now + TimeDuration::seconds(sub.interval_secs),
                0,
                true,
            )
            .await?;
        Ok(())
    }

    async fn fail(&self, sub: &Subscription, now: OffsetDateTime) -> Result<(), StoreError> {
        let fail_count = sub.fail_count + 1;
        let enabled = fail_count < self.max_fail_count;
        let backoff = sub.interval_secs * 2i64.pow(fail_count.min(8) as u32);
        self.store
            .mark_ran(
                sub.id,
                sub.cursor.as_deref(),
                now + TimeDuration::seconds(backoff),
                fail_count,
                enabled,
            )
            .await?;
        if !enabled {
            let _ = self
                .publisher
                .publish_text(
                    sub.chat_id,
                    &format!("订阅 {} 连续失败，已自动禁用", sub.source_ref),
                )
                .await;
        }
        Ok(())
    }

    async fn check_credentials(&self) -> Result<(), StoreError> {
        let all = self.store.all().await?;
        for (meta, cred) in all {
            let Some(ext) = self.registry.resolve_name(&meta.extractor_id) else {
                continue;
            };
            let Some(auth) = ext.auth() else {
                continue;
            };
            match AuthProvider::check(auth, &cred).await {
                Ok(CredentialStatus::Valid) => {}
                Ok(_status) => {
                    if let Ok(Some(refreshed)) = AuthProvider::refresh(auth, &cred).await {
                        self.store
                            .update_status(
                                &meta.extractor_id,
                                meta.scope,
                                meta.scope_id,
                                "valid",
                                Some(&refreshed),
                            )
                            .await?;
                    } else {
                        self.store
                            .update_status(
                                &meta.extractor_id,
                                meta.scope,
                                meta.scope_id,
                                "invalid",
                                None,
                            )
                            .await?;
                        let _ = self
                            .publisher
                            .publish_text(
                                meta.scope_id,
                                &format!(
                                    "平台 {} 的凭据已失效，请重新 /login {}",
                                    meta.extractor_id, meta.extractor_id
                                ),
                            )
                            .await;
                    }
                }
                Err(e) => warn!(error = %e, "auth check error"),
            }
        }
        Ok(())
    }
}
