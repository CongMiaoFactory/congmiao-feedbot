use crate::app::App;
use feedbot_core::{Job, JobState, StoreError, WorkEnqueue};
use std::sync::Arc;
use tokio::sync::mpsc;
use url::Url;
use uuid::Uuid;

#[derive(Clone)]
pub struct JobQueue {
    tx: mpsc::Sender<QueuedJob>,
    store: Option<Arc<feedbot_store::SqliteStore>>,
}

#[derive(Debug, Clone)]
pub struct QueuedJob {
    pub id: Uuid,
    pub chat_id: i64,
    pub user_id: i64,
    pub message_id: i32,
    pub url: Url,
    pub raw_args: String,
}

impl JobQueue {
    pub fn channel(cap: usize) -> (Self, mpsc::Receiver<QueuedJob>) {
        let (tx, rx) = mpsc::channel(cap);
        (Self { tx, store: None }, rx)
    }

    pub fn with_store(mut self, store: Arc<feedbot_store::SqliteStore>) -> Self {
        self.store = Some(store);
        self
    }

    #[allow(clippy::result_large_err)]
    pub async fn enqueue(&self, job: QueuedJob) -> Result<(), mpsc::error::SendError<QueuedJob>> {
        self.tx.send(job).await
    }
}

pub fn job_record(q: &QueuedJob) -> Job {
    Job {
        id: q.id,
        chat_id: q.chat_id,
        user_id: q.user_id,
        message_id: q.message_id,
        url: q.url.clone(),
        options: feedbot_core::OptionValues::new(),
        state: JobState::Queued,
        attempts: 0,
        last_error: None,
        created_at: time::OffsetDateTime::now_utc(),
    }
}

pub async fn recover_unfinished(app: &App, tx: &JobQueue) {
    use feedbot_core::JobRepo;
    match app.store.unfinished().await {
        Ok(jobs) => {
            for job in jobs {
                let _ = tx
                    .enqueue(QueuedJob {
                        id: job.id,
                        chat_id: job.chat_id,
                        user_id: job.user_id,
                        message_id: job.message_id,
                        url: job.url,
                        raw_args: String::new(),
                    })
                    .await;
            }
        }
        Err(e) => tracing::error!(error = %e, "failed to recover jobs"),
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
        use feedbot_core::JobRepo;
        let job = QueuedJob {
            id: Uuid::new_v4(),
            chat_id,
            user_id,
            message_id: 0,
            url,
            raw_args,
        };
        if let Some(store) = &self.store {
            store.insert_job(&job_record(&job)).await?;
        }
        let id = job.id;
        self.tx
            .send(job)
            .await
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(id)
    }
}
