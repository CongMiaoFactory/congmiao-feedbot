use crate::app::{App, TgBot};
use crate::commands::{self, Command};
use crate::login::{self, LoginDialogue, LoginState};
use crate::queue::QueuedJob;
use crate::ui::{self, UiCallback};
use feedbot_core::Scope;
use feedbot_extractors::ExtractorRegistry;
use std::collections::HashSet;
use std::sync::Arc;
use teloxide::dispatching::dialogue::InMemStorage;
use teloxide::dispatching::{UpdateHandler, dialogue};
use teloxide::payloads::{AnswerCallbackQuerySetters, SetMessageReactionSetters};
use teloxide::prelude::*;
use teloxide::sugar::request::RequestReplyExt;
use teloxide::types::{ChatAction, ChatKind, MessageEntityKind, ReactionType};

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
            let (text, kb) = commands::logout_html(&app, &id, (Scope::User, user_id)).await?;
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
            login::begin(&bot, &msg, app, dialogue, id, false).await?;
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
    let found = extract_urls(&msg);
    if found.is_empty() {
        return Ok(());
    }
    let private = matches!(msg.chat.kind, ChatKind::Private(_));
    let raw_args = strip_urls(msg.text().unwrap_or(""), &found);
    // Groups see plenty of unrelated links; only react to supported ones.
    let urls: Vec<url::Url> =
        dedupe_urls(&app.registry, found.into_iter().map(|f| f.url).collect())
            .into_iter()
            .filter(|u| app.registry.match_url(u).is_some())
            .collect();
    if urls.is_empty() {
        if private {
            bot.send_message(msg.chat.id, "没有可解析的链接。用 /help 查看支持的平台。")
                .reply_to(msg.id)
                .await?;
        }
        return Ok(());
    }
    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);
    let _ = bot
        .set_message_reaction(msg.chat.id, msg.id)
        .reaction([ReactionType::Emoji {
            emoji: "👀".into()
        }])
        .await;
    let _ = bot.send_chat_action(msg.chat.id, ChatAction::Typing).await;
    let mut notice = None;
    for url in urls {
        if app
            .access
            .user_limiter
            .check_key(&(user_id as u64))
            .is_err()
        {
            notice = Some("发送太快了，部分链接已忽略，请稍后再试。");
            break;
        }
        let job = QueuedJob::new(msg.chat.id.0, user_id, msg.id.0, url, raw_args.clone());
        if let Err(e) = app.jobs.submit(job).await {
            tracing::warn!(error = %e, "enqueue failed");
            notice = Some("任务队列已满，请稍后再试。");
            break;
        }
    }
    if let Some(text) = notice {
        let _ = bot.send_message(msg.chat.id, text).reply_to(msg.id).await;
    }
    Ok(())
}

/// A link found in a message, with the exact text it occupied (if any).
struct FoundUrl {
    url: url::Url,
    raw: Option<String>,
}

fn extract_urls(msg: &Message) -> Vec<FoundUrl> {
    let mut out = Vec::new();
    let text = msg.text().unwrap_or("");
    if let Some(entities) = msg.entities() {
        for ent in entities {
            match &ent.kind {
                MessageEntityKind::Url => {
                    if let Some(slice) = utf16_slice(text, ent.offset, ent.length) {
                        // Telegram also marks bare `example.com/x` as URLs.
                        let url = parse_web_url(&slice)
                            .or_else(|| parse_web_url(&format!("https://{slice}")));
                        if let Some(url) = url {
                            out.push(FoundUrl {
                                url,
                                raw: Some(slice),
                            });
                        }
                    }
                }
                MessageEntityKind::TextLink { url } => out.push(FoundUrl {
                    url: url.clone(),
                    raw: None,
                }),
                _ => {}
            }
        }
    }
    if out.is_empty() {
        for token in text.split_whitespace() {
            if let Some(url) = parse_web_url(token) {
                out.push(FoundUrl {
                    url,
                    raw: Some(token.to_string()),
                });
            }
        }
    }
    out
}

fn parse_web_url(raw: &str) -> Option<url::Url> {
    let parsed = url::Url::parse(raw).ok()?;
    matches!(parsed.scheme(), "http" | "https").then_some(parsed)
}

fn dedupe_urls(registry: &ExtractorRegistry, urls: Vec<url::Url>) -> Vec<url::Url> {
    let mut seen = HashSet::new();
    urls.into_iter()
        .filter(|u| seen.insert(registry.canonicalize(u).to_string()))
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

/// Remove links as they were written (not their normalized form), leaving the
/// option text around them.
fn strip_urls(text: &str, found: &[FoundUrl]) -> String {
    let mut rest = text.to_string();
    for raw in found.iter().filter_map(|f| f.raw.as_deref()) {
        rest = rest.replacen(raw, " ", 1);
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
        let out = dedupe_urls(&ExtractorRegistry::builtin(), vec![a.clone(), b]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], a);
    }

    #[test]
    fn strips_links_as_written() {
        let found = vec![FoundUrl {
            url: Url::parse("https://www.pixiv.net/artworks/1").unwrap(),
            raw: Some("HTTPS://WWW.PIXIV.NET/artworks/1".into()),
        }];
        let rest = strip_urls("+q:original HTTPS://WWW.PIXIV.NET/artworks/1 +sp", &found);
        let (tokens, leftover) = feedbot_core::split_option_tokens(&rest);
        assert_eq!(tokens, vec!["+q:original", "+sp"]);
        assert!(leftover.is_empty());
    }

    #[test]
    fn only_web_schemes_are_links() {
        assert!(parse_web_url("https://x.com/a/status/1").is_some());
        assert!(parse_web_url("ftp://x.com/a").is_none());
        assert!(parse_web_url("+q:original").is_none());
    }

    #[test]
    fn keeps_youtube_video_and_music_distinct() {
        let video = Url::parse("https://youtube.com/watch?v=jNQXAC9IVRw").unwrap();
        let music = Url::parse("https://music.youtube.com/watch?v=jNQXAC9IVRw").unwrap();
        let out = dedupe_urls(
            &ExtractorRegistry::builtin(),
            vec![video.clone(), music.clone()],
        );
        assert_eq!(out, vec![video, music]);
    }

    #[test]
    fn dedupe_across_site_host_aliases() {
        let registry = ExtractorRegistry::builtin();
        let urls = [
            "https://youtu.be/jNQXAC9IVRw?si=a",
            "https://www.youtube.com/watch?v=jNQXAC9IVRw",
            "https://x.com/a/status/1",
            "https://twitter.com/a/status/1",
            "https://b23.tv/BV1xx411c7mD",
            "https://www.bilibili.com/video/BV1xx411c7mD",
        ];
        let out = dedupe_urls(
            &registry,
            urls.iter().map(|u| Url::parse(u).unwrap()).collect(),
        );
        assert_eq!(out.len(), 3);
    }
}
