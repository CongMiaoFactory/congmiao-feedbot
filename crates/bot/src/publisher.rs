use crate::app::TgBot;
use feedbot_core::extract::Extracted;
use feedbot_core::{PublishError, Publisher};
use teloxide::prelude::*;
use teloxide::types::ChatId;
use teloxide::utils::html;
use url::Url;

#[derive(Clone)]
pub struct TelegramPublisher {
    bot: TgBot,
}

impl TelegramPublisher {
    pub fn new(bot: TgBot) -> Self {
        Self { bot }
    }
}

#[async_trait::async_trait]
impl Publisher for TelegramPublisher {
    /// `text` is plain text; it is escaped for the bot's HTML parse mode.
    async fn publish_text(&self, chat_id: i64, text: &str) -> Result<(), PublishError> {
        self.bot
            .send_message(ChatId(chat_id), html::escape(text))
            .await
            .map(|_| ())
            .map_err(|e| PublishError::Failed(e.to_string()))
    }
}

/// Context for overflow handling after media groups have been sent.
pub struct OverflowCtx<'a> {
    pub total_items: usize,
    pub group_count: usize,
    pub skipped: &'a [String],
}

/// What to do with results that have many items.
#[async_trait::async_trait]
pub trait OverflowSink: Send + Sync {
    /// Called after extraction, before anything is downloaded. `Some(url)`
    /// means the whole result was published there (e.g. a Telegraph page)
    /// and only a link is sent; `None` sends the media as albums.
    /// `source_key` identifies the source and its output options.
    async fn publish_page(&self, _extracted: &Extracted, _source_key: &str) -> Option<Url> {
        None
    }

    /// After albums were sent: optional summary / skipped-items text.
    fn summary(&self, ctx: &OverflowCtx<'_>) -> Option<String>;
}

/// Default overflow: keep chunking albums, optionally append a summary message.
pub struct ChunkedAlbums {
    summary_threshold: usize,
}

impl ChunkedAlbums {
    pub fn new(summary_threshold: usize) -> Self {
        Self { summary_threshold }
    }

    pub(crate) fn threshold(&self) -> usize {
        self.summary_threshold
    }
}

#[async_trait::async_trait]
impl OverflowSink for ChunkedAlbums {
    fn summary(&self, ctx: &OverflowCtx<'_>) -> Option<String> {
        if ctx.total_items <= self.summary_threshold && ctx.skipped.is_empty() {
            return None;
        }
        let mut text = format!("共 {} 项，已分 {} 组发送", ctx.total_items, ctx.group_count);
        if !ctx.skipped.is_empty() {
            text.push('\n');
            text.push_str(&ctx.skipped.join("\n"));
        }
        Some(text)
    }
}
