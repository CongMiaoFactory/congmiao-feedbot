use crate::app::TgBot;
use feedbot_core::{PublishError, Publisher};
use teloxide::prelude::*;
use teloxide::types::ChatId;

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
    async fn publish_text(&self, chat_id: i64, text: &str) -> Result<(), PublishError> {
        self.bot
            .send_message(ChatId(chat_id), text)
            .await
            .map(|_| ())
            .map_err(|e| PublishError::Failed(e.to_string()))
    }
}

/// Context for overflow handling after media groups have been sent.
pub struct OverflowCtx<'a> {
    pub chat_id: i64,
    pub total_items: usize,
    pub group_count: usize,
    pub skipped: &'a [String],
}

/// Extension point for >N items (Telegraph later). Currently returns summary text.
#[async_trait::async_trait]
pub trait OverflowSink: Send + Sync {
    async fn deliver(&self, ctx: &OverflowCtx<'_>) -> Result<Option<String>, PublishError>;
}

/// Default overflow: keep chunking albums, optionally append a summary message.
pub struct ChunkedAlbums {
    summary_threshold: usize,
}

impl ChunkedAlbums {
    pub fn new(summary_threshold: usize) -> Self {
        Self { summary_threshold }
    }
}

#[async_trait::async_trait]
impl OverflowSink for ChunkedAlbums {
    async fn deliver(&self, ctx: &OverflowCtx<'_>) -> Result<Option<String>, PublishError> {
        if ctx.total_items <= self.summary_threshold && ctx.skipped.is_empty() {
            return Ok(None);
        }
        let mut text = format!("共 {} 项，已分 {} 组发送", ctx.total_items, ctx.group_count);
        if !ctx.skipped.is_empty() {
            text.push('\n');
            text.push_str(&ctx.skipped.join("\n"));
        }
        Ok(Some(text))
    }
}
