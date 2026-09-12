use crate::app::{App, TgBot};
use crate::commands::{self, Command};
use crate::login::{self, LoginDialogue, LoginState};
use crate::queue::QueuedJob;
use crate::ui::{self, UiCallback};
use feedbot_core::JobRepo;
use feedbot_extractors::canonicalize;
use std::collections::HashSet;
use std::sync::Arc;
use teloxide::dispatching::dialogue::InMemStorage;
use teloxide::dispatching::{dialogue, UpdateHandler};
use teloxide::payloads::{AnswerCallbackQuerySetters, SetMessageReactionSetters};
use teloxide::prelude::*;
use teloxide::sugar::request::RequestReplyExt;
use teloxide::types::{ChatAction, ChatKind, MessageEntityKind, ReactionType};
use uuid::Uuid;

pub fn schema() -> UpdateHandler<anyhow::Error> {
    let command_handler = Update::filter_message()
        .filter_command::<Command>()
        .endpoint(crate::commands::handle);

    let message_handler = Update::filter_message().endpoint(login::on_text);

    let callback_handler = Update::filter_callback_query().endpoint(handle_callback);

    dialogue::enter::<Update, InMemStorage<LoginState>, LoginState, _>()
        .branch(command_handler)
        .branch(message_handler)
        .branch(callback_handler)
}

async fn handle_callback(
    bot: TgBot,
    q: CallbackQuery,
    app: Arc<App>,
    dialogue: LoginDialogue,
) -> anyhow::Result<()> {
    let Some(data) = q.data.as_deref() else {
        bot.answer_callback_query(q.id).await.ok();
        return Ok(());
    };
    let Some(cb) = ui::decode(data) else {
        bot.answer_callback_query(q.id).await.ok();
        return Ok(());
    };
    let Some(msg) = q.regular_message().cloned() else {
        bot.answer_callback_query(q.id)
            .text("消息已过期，请重新发送命令")
            .show_alert(true)
            .await
            .ok();
        return Ok(());
    };
    let user_id = q.from.id.0 as i64;
    let private = matches!(msg.chat.kind, ChatKind::Private(_));

    match cb {
        UiCallback::HelpOverview => {
            let plats = ui::named_platforms(app.registry.all(), false);
            let _ = ui::edit_page(
                &bot,
                msg.chat.id,
                msg.id,
                commands::help_overview(&app.registry),
                ui::platform_keyboard(UiCallback::HelpPlatform, &plats),
            )
            .await;
            bot.answer_callback_query(q.id).await.ok();
        }
        UiCallback::HelpPlatform(id) => {
            let (text, kb) = commands::help_platform_page(&app.registry, &id);
            let _ = ui::edit_page(&bot, msg.chat.id, msg.id, text, kb).await;
            bot.answer_callback_query(q.id).await.ok();
        }
        UiCallback::Set(id) => {
            let (text, kb) = commands::set_view_page(&app, &id, user_id, msg.chat.id.0).await?;
            let _ = ui::edit_page(&bot, msg.chat.id, msg.id, text, kb).await;
            bot.answer_callback_query(q.id).await.ok();
        }
        UiCallback::Logout(id) => {
            let (text, kb) =
                commands::logout_html(&app, &id, user_id, msg.chat.id.0, private).await?;
            let _ = ui::edit_page(&bot, msg.chat.id, msg.id, text, kb).await;
            bot.answer_callback_query(q.id).await.ok();
        }
        UiCallback::Login(id) => {
            if !private {
                bot.answer_callback_query(q.id)
                    .text("请私聊使用 /login")
                    .show_alert(true)
                    .await
                    .ok();
                return Ok(());
            }
            bot.answer_callback_query(q.id).await.ok();
            login::begin(&bot, &msg, app, dialogue, id).await?;
        }
        UiCallback::CancelLogin => {
            let exited = login::abort(&dialogue).await;
            bot.answer_callback_query(q.id)
                .text(if exited {
                    "已退出登录"
                } else {
                    "当前没有进行中的登录"
                })
                .await
                .ok();
            if exited {
                bot.send_message(msg.chat.id, "已退出登录。").await?;
            }
        }
    }
    Ok(())
}

pub async fn handle_url_message(bot: TgBot, msg: Message, app: Arc<App>) -> anyhow::Result<()> {
    let urls = dedupe_urls(extract_urls(&msg));
    if urls.is_empty() {
        return Ok(());
    }
    let raw_args = strip_urls(msg.text().unwrap_or(""), &urls);
    let _ = bot
        .set_message_reaction(msg.chat.id, msg.id)
        .reaction([ReactionType::Emoji {
            emoji: "👀".into()
        }])
        .await;
    let _ = bot.send_chat_action(msg.chat.id, ChatAction::Typing).await;
    for url in urls {
        let job = QueuedJob {
            id: Uuid::new_v4(),
            chat_id: msg.chat.id.0,
            user_id: msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0),
            message_id: msg.id.0,
            url,
            raw_args: raw_args.clone(),
        };
        let rec = crate::queue::job_record(&job);
        app.store.insert_job(&rec).await?;
        if app.jobs.enqueue(job).await.is_err() {
            let _ = bot
                .send_message(msg.chat.id, "队列已满")
                .reply_to(msg.id)
                .await;
        }
    }
    Ok(())
}

fn extract_urls(msg: &Message) -> Vec<url::Url> {
    let mut out = Vec::new();
    let text = msg.text().unwrap_or("");
    if let Some(entities) = msg.entities() {
        for ent in entities {
            match &ent.kind {
                MessageEntityKind::Url => {
                    if let Some(slice) = utf16_slice(text, ent.offset, ent.length) {
                        if let Ok(u) = url::Url::parse(&slice) {
                            out.push(u);
                        }
                    }
                }
                MessageEntityKind::TextLink { url } => out.push(url.clone()),
                _ => {}
            }
        }
    }
    if out.is_empty() {
        for token in text.split_whitespace() {
            if let Ok(u) = url::Url::parse(token) {
                if matches!(u.scheme(), "http" | "https") {
                    out.push(u);
                }
            }
        }
    }
    out
}

fn dedupe_urls(urls: Vec<url::Url>) -> Vec<url::Url> {
    let mut seen = HashSet::new();
    urls.into_iter()
        .filter(|u| seen.insert(canonicalize(u).to_string()))
        .collect()
}

fn utf16_slice(text: &str, offset: usize, length: usize) -> Option<String> {
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let end = offset.checked_add(length)?;
    if end > utf16.len() {
        return None;
    }
    String::from_utf16(&utf16[offset..end]).ok()
}

fn strip_urls(text: &str, urls: &[url::Url]) -> String {
    let mut rest = text.to_string();
    for u in urls {
        rest = rest.replace(u.as_str(), " ");
    }
    rest
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn dedupe_music_urls_that_only_differ_by_tracking() {
        let a = Url::parse("https://music.youtube.com/watch?v=jNQXAC9IVRw&si=abc").unwrap();
        let b = Url::parse("https://music.youtube.com/watch?v=jNQXAC9IVRw").unwrap();
        let out = dedupe_urls(vec![a.clone(), b]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], a);
    }

    #[test]
    fn keeps_youtube_video_and_music_distinct() {
        let video = Url::parse("https://youtube.com/watch?v=jNQXAC9IVRw").unwrap();
        let music = Url::parse("https://music.youtube.com/watch?v=jNQXAC9IVRw").unwrap();
        let out = dedupe_urls(vec![video.clone(), music.clone()]);
        assert_eq!(out, vec![video, music]);
    }
}
