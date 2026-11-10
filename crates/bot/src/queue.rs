use crate::app::App;
use feedbot_core::{Job, JobState, Store, StoreError, WorkEnqueue};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::sugar::request::RequestReplyExt;
use teloxide::types::{ChatId, MessageId};
use tokio::sync::mpsc;
use url::Url;
use uuid::Uuid;

/// Persistent job queue: every job is written to the store before it is
/// handed to the worker pool, so it can be recovered after a restart.
#[derive(Clone)]
pub struct JobQueue {
    tx: mpsc::Sender<QueuedJob>,
    store: Arc<dyn Store>,
}

#[derive(Debug, Clone)]
pub struct QueuedJob {
    pub id: Uuid,
    pub chat_id: i64,
    pub user_id: i64,
    /// Message to reply to; `0` for jobs without a source message (feeds).
    pub message_id: i32,
    pub url: Url,
    pub raw_args: String,
}

impl QueuedJob {
    pub fn new(chat_id: i64, user_id: i64, message_id: i32, url: Url, raw_args: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            chat_id,
            user_id,
            message_id,
            url,
            raw_args,
        }
    }

    fn record(&self) -> Job {
        Job {
            id: self.id,
            chat_id: self.chat_id,
            user_id: self.user_id,
            message_id: self.message_id,
            url: self.url.clone(),
            raw_args: self.raw_args.clone(),
            state: JobState::Queued,
            attempts: 0,
            last_error: None,
            created_at: time::OffsetDateTime::now_utc(),
        }
    }
}

impl From<Job> for QueuedJob {
    fn from(job: Job) -> Self {
        Self {
            id: job.id,
            chat_id: job.chat_id,
            user_id: job.user_id,
            message_id: job.message_id,
            url: job.url,
            raw_args: job.raw_args,
        }
    }
}

impl JobQueue {
    pub fn channel(cap: usize, store: Arc<dyn Store>) -> (Self, mpsc::Receiver<QueuedJob>) {
        let (tx, rx) = mpsc::channel(cap);
        (Self { tx, store }, rx)
    }

    /// Persist and enqueue a new job. Fails without blocking when the queue is full.
    pub async fn submit(&self, job: QueuedJob) -> Result<Uuid, StoreError> {
        self.store.insert_job(&job.record()).await?;
        let id = job.id;
        if let Err(e) = self.tx.try_send(job) {
            self.store
                .set_state(id, JobState::Failed, Some("queue full"))
                .await?;
            return Err(StoreError::Db(format!("任务队列已满: {e}")));
        }
        Ok(id)
    }

    /// Re-enqueue an already persisted job (restart recovery).
    async fn resume(&self, job: QueuedJob) {
        let _ = self.tx.send(job).await;
    }
}

/// Re-queue jobs left unfinished by a previous process. Jobs that were already
/// uploading may have been partially delivered, so they are failed instead of
/// re-run to avoid duplicate messages.
pub async fn recover_unfinished(app: &App) {
    let jobs = match app.store.unfinished().await {
        Ok(jobs) => jobs,
        Err(e) => {
            tracing::error!(error = %e, "failed to recover jobs");
            return;
        }
    };
    for job in jobs {
        if job.state == JobState::Uploading {
            let _ = app
                .store
                .set_state(
                    job.id,
                    JobState::Failed,
                    Some("interrupted while uploading"),
                )
                .await;
            if job.message_id != 0 {
                let _ = app
                    .bot
                    .send_message(
                        ChatId(job.chat_id),
                        "bot 重启时任务正在上传，已中止以免重复发送。",
                    )
                    .reply_to(MessageId(job.message_id))
                    .await;
            }
            continue;
        }
        app.jobs.resume(job.into()).await;
    }
}

#[async_trait::async_trait]
impl WorkEnqueue for JobQueue {
    async fn enqueue_url(
        &self,
        chat_id: i64,
        user_id: i64,
        url: Url,
        raw_args: String,
    ) -> Result<Uuid, StoreError> {
        self.submit(QueuedJob::new(chat_id, user_id, 0, url, raw_args))
            .await
    }
}
