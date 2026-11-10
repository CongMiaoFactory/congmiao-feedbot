//! Chat action and reaction feedback shown while a job runs.

use crate::app::TgBot;
use std::time::Duration;
use teloxide::payloads::SetMessageReactionSetters;
use teloxide::prelude::*;
use teloxide::sugar::request::RequestReplyExt;
use teloxide::types::{ChatAction, ChatId, MessageId, ReactionType};
use teloxide::utils::html;
use tokio_util::sync::CancellationToken;

/// Telegram shows a chat action for 5 s; refresh just before it fades.
const ACTION_REFRESH: Duration = Duration::from_millis(4500);

pub(super) struct Presence {
    bot: TgBot,
    chat: ChatId,
    message_id: Option<MessageId>,
    refresh: Option<CancellationToken>,
}

impl Presence {
    pub(super) fn new(bot: TgBot, chat: ChatId, message_id: i32) -> Self {
        Self {
            bot,
            chat,
            message_id: (message_id != 0).then_some(MessageId(message_id)),
            refresh: None,
        }
    }

    /// Show `action` now and keep refreshing it until the next state change.
    pub(super) async fn set_action(&mut self, action: ChatAction) {
        self.stop_refresh();
        let _ = self.bot.send_chat_action(self.chat, action).await;
        let cancel = CancellationToken::new();
        let child = cancel.clone();
        let bot = self.bot.clone();
        let chat = self.chat;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = child.cancelled() => break,
                    _ = tokio::time::sleep(ACTION_REFRESH) => {
                        let _ = bot.send_chat_action(chat, action).await;
                    }
                }
            }
        });
        self.refresh = Some(cancel);
    }

    /// Stop the chat action and clear the 👀 reaction.
    pub(super) async fn stop(&mut self) {
        self.stop_refresh();
        self.set_reaction(&[]).await;
    }

    pub(super) async fn fail(&mut self, text: &str) {
        self.stop_refresh();
        self.reply(text).await;
        self.set_reaction(&[ReactionType::Emoji {
            emoji: "😢".into()
        }])
        .await;
    }

    /// Stop refreshing the chat action, e.g. while waiting to retry.
    pub(super) fn stop_refresh(&mut self) {
        if let Some(cancel) = self.refresh.take() {
            cancel.cancel();
        }
    }

    async fn reply(&self, text: &str) {
        let mut req = self.bot.send_message(self.chat, html::escape(text));
        if let Some(id) = self.message_id {
            req = req.reply_to(id);
        }
        let _ = req.await;
    }

    async fn set_reaction(&self, reaction: &[ReactionType]) {
        let Some(id) = self.message_id else {
            return;
        };
        let _ = self
            .bot
            .set_message_reaction(self.chat, id)
            .reaction(reaction.to_vec())
            .await;
    }
}

impl Drop for Presence {
    fn drop(&mut self) {
        self.stop_refresh();
    }
}
