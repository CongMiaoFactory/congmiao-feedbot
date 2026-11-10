//! Job, subscription, cache, and chat setting domain types.

use crate::extract::MediaKind;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Extracting,
    Downloading,
    Processing,
    Uploading,
    Done,
    Failed,
    /// Stopped by the user (`/cancel`).
    Cancelled,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Extracting => "extracting",
            Self::Downloading => "downloading",
            Self::Processing => "processing",
            Self::Uploading => "uploading",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "extracting" => Self::Extracting,
            "downloading" => Self::Downloading,
            "processing" => Self::Processing,
            "uploading" => Self::Uploading,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub chat_id: i64,
    pub user_id: i64,
    pub message_id: i32,
    pub url: Url,
    /// Inline `+`/`-` option text sent with the link, re-parsed on every run.
    pub raw_args: String,
    pub state: JobState,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct MediaCacheEntry {
    pub source_key: String,
    pub idx: i32,
    pub file_id: String,
    pub file_unique_id: String,
    pub kind: MediaKind,
    pub size: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: Uuid,
    pub chat_id: i64,
    pub user_id: i64,
    pub extractor_id: String,
    pub source_ref: String,
    /// Inline `+`/`-` option text given to `/sub`, applied to every pushed item
    /// the same way as text sent next to a link.
    pub raw_args: String,
    pub cursor: Option<String>,
    pub interval_secs: i64,
    pub enabled: bool,
    pub next_due_at: OffsetDateTime,
    pub fail_count: i32,
    /// Paused until the subscriber (or an admin, globally) logs in again.
    pub needs_login: bool,
}

#[async_trait::async_trait]
pub trait WorkEnqueue: Send + Sync {
    async fn enqueue_url(
        &self,
        chat_id: i64,
        user_id: i64,
        url: Url,
        raw_args: String,
    ) -> Result<Uuid, crate::repo::StoreError>;
}

/// Outbound publish contract used by the feed crate (no Telegram types).
#[async_trait::async_trait]
pub trait Publisher: Send + Sync {
    async fn publish_text(&self, chat_id: i64, text: &str) -> Result<(), PublishError>;
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("发送失败: {0}")]
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_and_cancelled_is_terminal() {
        for state in [
            JobState::Queued,
            JobState::Extracting,
            JobState::Downloading,
            JobState::Processing,
            JobState::Uploading,
            JobState::Done,
            JobState::Failed,
            JobState::Cancelled,
        ] {
            assert_eq!(JobState::parse(state.as_str()), Some(state));
        }
        assert!(JobState::Cancelled.is_terminal());
        assert!(!JobState::Uploading.is_terminal());
    }
}
