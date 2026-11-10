//! Subscription scheduler. Shares the extract/upload worker via [`WorkEnqueue`].
//!
//! Each tick polls due subscriptions through [`Extractor::feed`], records every
//! entry in the seen set, and enqueues only entries it has not seen before. The
//! first poll of a subscription only fills the seen set so history is not pushed.
//! The same loop also re-checks stored credentials and prunes old finished jobs.

use feedbot_core::repo::StoredCredentialMeta;
use feedbot_core::{
    AuthProvider, CredentialStatus, ExtractCtx, ExtractError, ExtractRequest, Extractor,
    ExtractorLookup, FeedEntry, Publisher, RequesterInfo, Scope, Store, StoreError, Subscription,
    WorkEnqueue, split_option_tokens,
};
use futures_util::StreamExt;
use futures_util::stream;
use std::sync::Arc;
use std::time::Duration;
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// `cursor` value stored once the first poll has filled the seen set.
const CURSOR_READY: &str = "ready";
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone)]
pub struct FeedSettings {
    pub tick: Duration,
    /// Consecutive failures after which a subscription is disabled.
    pub max_fail_count: i32,
    /// New entries pushed per poll; older unseen entries are marked seen.
    pub push_limit: usize,
    /// Subscriptions polled at the same time, so one slow source does not
    /// hold up the others.
    pub poll_concurrency: usize,
    /// Upper bound for one `feed()` call.
    pub poll_timeout: Duration,
    /// Minimum time between two checks of the same credential.
    pub credential_check_interval: Duration,
    /// Finished jobs older than this are deleted.
    pub job_retention: Duration,
    /// Notified when a global credential becomes invalid.
    pub admin_ids: Vec<i64>,
}

pub struct FeedScheduler {
    store: Arc<dyn Store>,
    queue: Arc<dyn WorkEnqueue>,
    extractors: Arc<dyn ExtractorLookup>,
    publisher: Arc<dyn Publisher>,
    ctx: ExtractCtx,
    settings: FeedSettings,
}

/// Outcome of one subscription poll.
enum Poll {
    /// First poll: seen set filled, nothing pushed.
    Initialized(usize),
    Pushed(usize),
}

#[derive(Debug, thiserror::Error)]
enum PollError {
    #[error("{0}")]
    Extract(#[from] ExtractError),
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("没有可订阅该来源的解析器")]
    NoExtractor,
    #[error("拉取超时")]
    Timeout,
}

impl FeedScheduler {
    pub fn new(
        store: Arc<dyn Store>,
        queue: Arc<dyn WorkEnqueue>,
        extractors: Arc<dyn ExtractorLookup>,
        publisher: Arc<dyn Publisher>,
        ctx: ExtractCtx,
        settings: FeedSettings,
    ) -> Self {
        Self {
            store,
            queue,
            extractors,
            publisher,
            ctx,
            settings,
        }
    }

    pub async fn run(self, cancel: CancellationToken) {
        let mut interval = tokio::time::interval(self.settings.tick);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_prune: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(e) = self.poll_due().await {
                        warn!(error = %e, "feed tick failed");
                    }
                    if let Err(e) = self.check_credentials().await {
                        warn!(error = %e, "credential check failed");
                    }
                    if last_prune.is_none_or(|t| t.elapsed() >= PRUNE_EVERY) {
                        self.prune_jobs().await;
                        last_prune = Some(tokio::time::Instant::now());
                    }
                }
            }
        }
    }

    async fn poll_due(&self) -> Result<(), StoreError> {
        let now = OffsetDateTime::now_utc();
        let due = self.store.due(now).await?;
        stream::iter(due)
            .for_each_concurrent(self.settings.poll_concurrency.max(1), |sub| async move {
                if let Err(e) = self.poll_and_record(&sub, now).await {
                    warn!(sub = %sub.id, error = %e, "failed to record subscription poll");
                }
            })
            .await;
        Ok(())
    }

    async fn poll_and_record(
        &self,
        sub: &Subscription,
        now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        match self.poll(sub).await {
            Ok(outcome) => {
                match outcome {
                    Poll::Initialized(n) => {
                        info!(sub = %sub.id, entries = n, "initialized subscription");
                    }
                    Poll::Pushed(0) => {}
                    Poll::Pushed(n) => info!(sub = %sub.id, pushed = n, "subscription pushed"),
                }
                self.store
                    .mark_ran(
                        sub.id,
                        Some(CURSOR_READY),
                        next_due(now, sub.interval_secs),
                        0,
                        true,
                    )
                    .await
            }
            Err(PollError::Extract(ExtractError::AuthRequired(_))) => {
                info!(sub = %sub.id, "subscription needs a login; paused");
                self.pause_for_login(sub).await
            }
            Err(e) => {
                warn!(sub = %sub.id, error = %e, "subscription poll failed");
                self.fail(sub, now, &e).await
            }
        }
    }

    async fn poll(&self, sub: &Subscription) -> Result<Poll, PollError> {
        let url = url::Url::parse(&sub.source_ref)
            .map_err(|e| ExtractError::Failed(format!("订阅地址无效: {e}")))?;
        let extractor = self
            .extractors
            .by_id(&sub.extractor_id)
            .ok_or(PollError::NoExtractor)?;
        let entries = self.fetch(extractor.as_ref(), sub, url).await?;

        // `cursor` is only set after the first successful poll.
        let first_run = sub.cursor.is_none();
        let mut fresh = Vec::new();
        for entry in entries {
            if self.store.insert_if_new(sub.id, &entry.key).await? {
                fresh.push(entry);
            }
        }
        if first_run {
            return Ok(Poll::Initialized(fresh.len()));
        }
        // Entries arrive newest first; push the newest few, oldest of them first.
        fresh.truncate(self.settings.push_limit);
        fresh.reverse();
        for (pushed, entry) in fresh.iter().enumerate() {
            let enqueued = self
                .queue
                .enqueue_url(
                    sub.chat_id,
                    sub.user_id,
                    entry.url.clone(),
                    sub.raw_args.clone(),
                )
                .await;
            if let Err(e) = enqueued {
                // Queue full or store error: un-see what was not pushed so the
                // next poll retries it instead of skipping it for good.
                for rest in &fresh[pushed..] {
                    self.store.forget_seen(sub.id, &rest.key).await?;
                }
                return Err(e.into());
            }
        }
        Ok(Poll::Pushed(fresh.len()))
    }

    async fn fetch(
        &self,
        extractor: &dyn Extractor,
        sub: &Subscription,
        url: url::Url,
    ) -> Result<Vec<FeedEntry>, PollError> {
        let saved = feedbot_core::load_saved_options(
            self.store.as_ref(),
            extractor.id(),
            sub.user_id,
            sub.chat_id,
        )
        .await?;
        let (tokens, _) = split_option_tokens(&sub.raw_args);
        let options = extractor
            .resolve_options(&saved, &tokens)
            .map_err(ExtractError::from)?;
        let credential = self.store.resolve(extractor.id(), sub.user_id).await?;
        let req = ExtractRequest {
            url,
            options,
            credential,
            requester: RequesterInfo {
                user_id: sub.user_id,
                chat_id: sub.chat_id,
            },
        };
        tokio::time::timeout(self.settings.poll_timeout, extractor.feed(&self.ctx, &req))
            .await
            .map_err(|_| PollError::Timeout)?
            .map_err(PollError::from)
    }

    /// A missing or expired login is not the source's fault: pause without
    /// counting a failure, tell the chat once, and let the next saved login
    /// resume it (`SubscriptionRepo::resume_after_login`).
    async fn pause_for_login(&self, sub: &Subscription) -> Result<(), StoreError> {
        if !self.store.pause_for_login(sub.id).await? {
            return Ok(());
        }
        let name = self
            .extractors
            .by_id(&sub.extractor_id)
            .map(|e| e.display_name().to_string())
            .unwrap_or_else(|| sub.extractor_id.clone());
        let _ = self
            .publisher
            .publish_text(
                sub.chat_id,
                &format!(
                    "订阅 {} 需要 {name} 登录，已暂停。订阅者私聊发送 /login {} 后会自动恢复。",
                    sub.source_ref, sub.extractor_id
                ),
            )
            .await;
        Ok(())
    }

    async fn fail(
        &self,
        sub: &Subscription,
        now: OffsetDateTime,
        err: &PollError,
    ) -> Result<(), StoreError> {
        let fail_count = sub.fail_count + 1;
        let enabled = fail_count < self.settings.max_fail_count;
        let backoff = sub
            .interval_secs
            .saturating_mul(2i64.pow(fail_count.clamp(0, 6) as u32));
        self.store
            .mark_ran(
                sub.id,
                sub.cursor.as_deref(),
                next_due(now, backoff),
                fail_count,
                enabled,
            )
            .await?;
        if !enabled {
            let _ = self
                .publisher
                .publish_text(
                    sub.chat_id,
                    &format!(
                        "订阅 {} 连续失败 {fail_count} 次，已自动停用：{err}\n用 /unsub {} 删除后可重新订阅。",
                        sub.source_ref, sub.id
                    ),
                )
                .await;
        }
        Ok(())
    }

    async fn check_credentials(&self) -> Result<(), StoreError> {
        let before = OffsetDateTime::now_utc()
            - TimeDuration::seconds(
                i64::try_from(self.settings.credential_check_interval.as_secs())
                    .unwrap_or(i64::MAX),
            );
        for (meta, cred) in self.store.due_for_check(before).await? {
            let Some(extractor) = self.extractors.by_id(&meta.extractor_id) else {
                continue;
            };
            let Some(auth) = extractor.auth() else {
                continue;
            };
            let (status, refreshed) = match AuthProvider::check(auth, &cred).await {
                Ok(status) if status.needs_login() => {
                    match AuthProvider::refresh(auth, &cred).await {
                        Ok(Some(fresh)) => (CredentialStatus::Valid, Some(fresh)),
                        _ => (status, None),
                    }
                }
                // Unknown is inconclusive (network, upstream error): keep the
                // stored status and try again after the next interval.
                Ok(CredentialStatus::Unknown) => (meta.status, None),
                Ok(status) => (status, None),
                Err(e) => {
                    warn!(extractor = %meta.extractor_id, error = %e, "credential check error");
                    (meta.status, None)
                }
            };
            self.store
                .update_status(
                    &meta.extractor_id,
                    meta.scope,
                    meta.scope_id,
                    status,
                    refreshed.as_ref(),
                )
                .await?;
            if status.needs_login() {
                self.notify_invalid(&meta).await;
            }
        }
        Ok(())
    }

    async fn notify_invalid(&self, meta: &StoredCredentialMeta) {
        let name = self
            .extractors
            .by_id(&meta.extractor_id)
            .map(|e| e.display_name().to_string())
            .unwrap_or_else(|| meta.extractor_id.clone());
        let recipients = match meta.scope {
            // A user's private chat id equals their user id. `Chat` is a
            // legacy scope that is no longer created or resolved.
            Scope::User | Scope::Chat => vec![meta.scope_id],
            Scope::Global => self.settings.admin_ids.clone(),
        };
        let scope_note = if meta.scope == Scope::Global {
            "全局"
        } else {
            ""
        };
        let text = format!(
            "{name} 的{scope_note}登录凭据已失效，请私聊 /login {} 重新登录。",
            meta.extractor_id
        );
        for chat in recipients {
            if let Err(e) = self.publisher.publish_text(chat, &text).await {
                warn!(chat, error = %e, "failed to notify invalid credential");
            }
        }
    }

    async fn prune_jobs(&self) {
        let retention = TimeDuration::seconds(
            i64::try_from(self.settings.job_retention.as_secs()).unwrap_or(i64::MAX),
        );
        match self
            .store
            .prune_finished(OffsetDateTime::now_utc() - retention)
            .await
        {
            Ok(0) => {}
            Ok(n) => info!(deleted = n, "pruned finished jobs"),
            Err(e) => warn!(error = %e, "prune jobs failed"),
        }
    }
}

fn next_due(now: OffsetDateTime, secs: i64) -> OffsetDateTime {
    now + TimeDuration::seconds(secs.max(60))
}

#[cfg(test)]
mod tests {
    use super::*;
    use feedbot_core::{
        HttpClient, HttpRequest, HttpResponse, MediaError, PublishError, SubscriptionRepo, Ytdlp,
        YtdlpSpec,
    };
    use feedbot_store::SqliteStore;
    use serde_json::Value;
    use std::path::Path;
    use std::sync::Mutex;
    use url::Url;
    use uuid::Uuid;

    const CHAT: i64 = 10;
    const USER: i64 = 20;

    /// What the fake source returns on the next poll.
    enum Source {
        Entries(Vec<u32>),
        NeedsLogin,
        Broken,
    }

    struct FakeExtractor {
        source: Mutex<Source>,
    }

    #[async_trait::async_trait]
    impl Extractor for FakeExtractor {
        fn meta(&self) -> feedbot_core::SiteMeta {
            feedbot_core::SiteMeta {
                id: "fake",
                name: "Fake",
                aliases: &[],
                priority: 0,
                kinds: &[],
                examples: &[],
                feed_examples: &[],
            }
        }
        fn matches(&self, _url: &Url) -> bool {
            true
        }
        async fn feed(
            &self,
            _ctx: &ExtractCtx,
            _req: &ExtractRequest,
        ) -> Result<Vec<FeedEntry>, ExtractError> {
            match &*self.source.lock().unwrap() {
                // Newest first, like real feeds.
                Source::Entries(ids) => Ok(ids
                    .iter()
                    .map(|id| FeedEntry {
                        key: id.to_string(),
                        url: Url::parse(&format!("https://example.com/{id}")).unwrap(),
                    })
                    .collect()),
                Source::NeedsLogin => Err(ExtractError::AuthRequired("fake".into())),
                Source::Broken => Err(ExtractError::Failed("broken".into())),
            }
        }
        async fn extract(
            &self,
            _ctx: &ExtractCtx,
            _req: &ExtractRequest,
        ) -> Result<feedbot_core::Extracted, ExtractError> {
            Err(ExtractError::Unsupported)
        }
    }

    struct Lookup(Arc<FakeExtractor>);

    impl ExtractorLookup for Lookup {
        fn by_id(&self, id: &str) -> Option<Arc<dyn Extractor>> {
            (id == "fake").then(|| self.0.clone() as Arc<dyn Extractor>)
        }
    }

    /// Records pushed URLs; refuses everything after `capacity` pushes.
    struct FakeQueue {
        pushed: Mutex<Vec<String>>,
        capacity: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl WorkEnqueue for FakeQueue {
        async fn enqueue_url(
            &self,
            _chat_id: i64,
            _user_id: i64,
            url: Url,
            _raw_args: String,
        ) -> Result<Uuid, StoreError> {
            let mut pushed = self.pushed.lock().unwrap();
            if pushed.len() >= *self.capacity.lock().unwrap() {
                return Err(StoreError::Db("queue full".into()));
            }
            pushed.push(url.path().trim_start_matches('/').to_string());
            Ok(Uuid::new_v4())
        }
    }

    #[derive(Default)]
    struct FakePublisher(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl Publisher for FakePublisher {
        async fn publish_text(&self, _chat_id: i64, text: &str) -> Result<(), PublishError> {
            self.0.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    /// The fake extractor never touches HTTP or yt-dlp.
    struct NoTools;

    #[async_trait::async_trait]
    impl HttpClient for NoTools {
        async fn fetch(&self, _req: HttpRequest) -> Result<HttpResponse, ExtractError> {
            Err(ExtractError::Unsupported)
        }
    }

    #[async_trait::async_trait]
    impl Ytdlp for NoTools {
        async fn probe_json(&self, _: &Url, _: Option<&str>) -> Result<Value, MediaError> {
            Err(MediaError::MissingYtdlp)
        }
        async fn probe_playlist(
            &self,
            _: &Url,
            _: Option<&str>,
            _: u32,
        ) -> Result<Value, MediaError> {
            Err(MediaError::MissingYtdlp)
        }
        async fn download(&self, _: &Url, _: &Path, _: &YtdlpSpec) -> Result<(), MediaError> {
            Err(MediaError::MissingYtdlp)
        }
    }

    struct Harness {
        _dir: tempfile::TempDir,
        store: Arc<SqliteStore>,
        source: Arc<FakeExtractor>,
        queue: Arc<FakeQueue>,
        publisher: Arc<FakePublisher>,
        scheduler: FeedScheduler,
        sub: Uuid,
    }

    impl Harness {
        async fn new(push_limit: usize, max_fail_count: i32) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(
                SqliteStore::connect(&dir.path().join("t.db"), &"11".repeat(32))
                    .await
                    .unwrap(),
            );
            let source = Arc::new(FakeExtractor {
                source: Mutex::new(Source::Entries(Vec::new())),
            });
            let queue = Arc::new(FakeQueue {
                pushed: Mutex::new(Vec::new()),
                capacity: Mutex::new(usize::MAX),
            });
            let publisher = Arc::new(FakePublisher::default());
            let tools = Arc::new(NoTools);
            let scheduler = FeedScheduler::new(
                store.clone(),
                queue.clone(),
                Arc::new(Lookup(source.clone())),
                publisher.clone(),
                ExtractCtx::new(tools.clone(), tools),
                FeedSettings {
                    tick: Duration::from_secs(1),
                    max_fail_count,
                    push_limit,
                    poll_concurrency: 2,
                    poll_timeout: Duration::from_secs(5),
                    credential_check_interval: Duration::from_secs(3600),
                    job_retention: Duration::from_secs(3600),
                    admin_ids: Vec::new(),
                },
            );
            let sub = Subscription {
                id: Uuid::new_v4(),
                chat_id: CHAT,
                user_id: USER,
                extractor_id: "fake".into(),
                source_ref: "https://example.com/feed".into(),
                raw_args: String::new(),
                cursor: None,
                interval_secs: 3600,
                enabled: true,
                next_due_at: OffsetDateTime::now_utc(),
                fail_count: 0,
                needs_login: false,
            };
            store.insert_subscription(&sub).await.unwrap();
            Self {
                _dir: dir,
                store,
                source,
                queue,
                publisher,
                scheduler,
                sub: sub.id,
            }
        }

        fn set_source(&self, source: Source) {
            *self.source.source.lock().unwrap() = source;
        }

        async fn state(&self) -> Subscription {
            let list = self.store.list_for_chat(CHAT).await.unwrap();
            list.into_iter().find(|s| s.id == self.sub).unwrap()
        }

        /// Poll now, as if the subscription's next due time had passed.
        async fn tick(&self) {
            let s = self.state().await;
            let past = OffsetDateTime::now_utc() - TimeDuration::seconds(1);
            self.store
                .mark_ran(s.id, s.cursor.as_deref(), past, s.fail_count, s.enabled)
                .await
                .unwrap();
            self.scheduler.poll_due().await.unwrap();
        }

        fn pushed(&self) -> Vec<String> {
            self.queue.pushed.lock().unwrap().clone()
        }

        fn notices(&self) -> Vec<String> {
            self.publisher.0.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn first_poll_only_records_then_pushes_newest_oldest_first() {
        let h = Harness::new(2, 8).await;
        h.set_source(Source::Entries(vec![3, 2, 1]));
        h.tick().await;
        assert!(h.pushed().is_empty(), "history is not pushed");
        assert_eq!(h.state().await.cursor.as_deref(), Some(CURSOR_READY));

        // Three new entries, push_limit 2: the newest two, oldest of them first.
        h.set_source(Source::Entries(vec![6, 5, 4, 3, 2, 1]));
        h.tick().await;
        assert_eq!(h.pushed(), ["5", "6"]);

        // Entry 4 was over the limit: marked seen, not pushed later.
        h.tick().await;
        assert_eq!(h.pushed(), ["5", "6"]);
    }

    #[tokio::test]
    async fn full_queue_retries_unpushed_entries() {
        let h = Harness::new(10, 8).await;
        h.set_source(Source::Entries(vec![1]));
        h.tick().await;

        h.set_source(Source::Entries(vec![3, 2, 1]));
        *h.queue.capacity.lock().unwrap() = 1;
        h.tick().await;
        assert_eq!(h.pushed(), ["2"]);
        assert_eq!(h.state().await.fail_count, 1);

        *h.queue.capacity.lock().unwrap() = usize::MAX;
        h.tick().await;
        assert_eq!(h.pushed(), ["2", "3"]);
        assert_eq!(h.state().await.fail_count, 0);
    }

    #[tokio::test]
    async fn needs_login_pauses_without_failing() {
        let h = Harness::new(10, 2).await;
        h.set_source(Source::NeedsLogin);
        h.tick().await;
        let state = h.state().await;
        assert!(state.needs_login && state.enabled);
        assert_eq!(state.fail_count, 0);
        assert_eq!(h.notices().len(), 1);
        assert!(h.notices()[0].contains("/login fake"));

        // Paused: not polled again, no second notice.
        h.tick().await;
        assert_eq!(h.notices().len(), 1);

        let now = OffsetDateTime::now_utc();
        assert_eq!(
            h.store
                .resume_after_login("fake", Some(USER), now)
                .await
                .unwrap(),
            1
        );
        h.set_source(Source::Entries(vec![1]));
        h.scheduler.poll_due().await.unwrap();
        assert!(!h.state().await.needs_login);
    }

    #[tokio::test]
    async fn repeated_failures_disable_and_notify_once() {
        let h = Harness::new(10, 2).await;
        h.set_source(Source::Broken);
        h.tick().await;
        let state = h.state().await;
        assert!(state.enabled);
        assert_eq!(state.fail_count, 1);
        assert!(state.next_due_at > OffsetDateTime::now_utc() + TimeDuration::seconds(3600));
        assert!(h.notices().is_empty());

        h.tick().await;
        assert!(!h.state().await.enabled);
        assert_eq!(h.notices().len(), 1);
        assert!(h.notices()[0].contains("已自动停用"));
    }
}
