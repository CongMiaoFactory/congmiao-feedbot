use std::{sync::Arc, time::Duration};

use anyhow::Result;
use regex::Regex;
use sha2::{Digest, Sha256};
use teloxide::{
    dispatching::UpdateFilterExt,
    dptree,
    error_handlers::LoggingErrorHandler,
    prelude::*,
    types::{
        FileId, InlineQueryResult, InlineQueryResultArticle, InlineQueryResultCachedAudio,
        InlineQueryResultCachedPhoto, InlineQueryResultCachedVideo, InlineQueryResultPhoto,
        InlineQueryResultVideo, InputFile, InputMedia, InputMediaPhoto, InputMediaVideo,
        InputMessageContent, InputMessageContentText, MessageEntityKind, MessageId, ParseMode,
        ReplyParameters,
    },
};
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use crate::{
    Config, MediaSpoilerMode, ProviderRegistry, RuntimeCredentials,
    cache::AppCache,
    login::{LoginPoll, LoginService},
    media::{MediaProcessor, telegram_photo_dimensions_valid},
    model::*,
    storage::Storage,
};

#[derive(Clone)]
pub struct BotState {
    pub config: Config,
    pub registry: ProviderRegistry,
    pub media: MediaProcessor,
    pub storage: Storage,
    pub cache: AppCache,
    pub login: LoginService,
    pub credentials: RuntimeCredentials,
    pub queue: Arc<Semaphore>,
}

pub async fn run(state: BotState) -> Result<()> {
    // teloxide's default HTTP timeout is intentionally short (17 seconds),
    // which is insufficient when a local Bot API waits for Telegram to finish
    // processing a large multipart upload. A timed-out request may still have
    // delivered the media and then be reported as a network failure.
    let telegram_client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(
            state.config.telegram_request_timeout_secs,
        ))
        .tcp_nodelay(true)
        .build()?;
    let mut bot = Bot::with_client(&state.config.telegram_token, telegram_client);
    if let Some(api) = &state.config.telegram_api_url {
        bot = bot.set_api_url(url::Url::parse(api)?);
    }
    let handler = dptree::entry()
        .branch(Update::filter_inline_query().endpoint(handle_inline))
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_channel_post().endpoint(handle_message));
    let mut dispatcher = Dispatcher::builder(bot.clone(), handler)
        .dependencies(dptree::deps![state.clone()])
        .enable_ctrlc_handler()
        .build();
    info!(
        api_url = ?state.config.telegram_api_url,
        timeout_secs = state.config.telegram_request_timeout_secs,
        "Telegram Bot 已启动"
    );
    if let Some(webhook_url) = &state.config.webhook_url {
        use teloxide::update_listeners::webhooks;
        let host: std::net::IpAddr = state.config.webhook_host.parse()?;
        let addr = std::net::SocketAddr::new(host, state.config.webhook_port);
        let listener =
            webhooks::axum(bot, webhooks::Options::new(addr, webhook_url.parse()?)).await?;
        dispatcher
            .dispatch_with_listener(
                listener,
                LoggingErrorHandler::with_custom_text("Webhook listener error"),
            )
            .await;
    } else {
        dispatcher.dispatch().await;
    }
    Ok(())
}

async fn handle_message(bot: Bot, msg: Message, state: BotState) -> ResponseResult<()> {
    let text = msg.text().or_else(|| msg.caption()).unwrap_or_default();
    if text == "/start" || text == "/help" {
        bot.send_message(msg.chat.id, "发送 X、YouTube、Pixiv、哔哩哔哩或网易云链接即可解析；在链接后加 +sp 可手动开启媒体遮罩。\n命令：/parse、/video [清晰度]、/file、/cover、/login [bili|netease]") .await?;
        return Ok(());
    }
    if text.starts_with("/login") {
        handle_login(bot, &msg, &state, text).await?;
        return Ok(());
    }
    let options = parse_options(text);
    let mut urls = extract_message_urls(&msg);
    if options.force_spoiler {
        for url in &mut urls {
            if url.to_ascii_lowercase().ends_with("+sp") {
                url.truncate(url.len() - 3);
            }
        }
    }
    if urls.is_empty()
        && options.force_spoiler
        && let Some(replied) = msg.reply_to_message()
    {
        urls = extract_message_urls(replied);
    }
    if urls.is_empty() {
        if msg.chat.is_private() && text.starts_with('/') {
            bot.send_message(msg.chat.id, "未找到支持的链接").await?;
        }
        return Ok(());
    }
    let subject = msg
        .from
        .as_ref()
        .map(|u| u.id.0.to_string())
        .unwrap_or_else(|| msg.chat.id.0.to_string());
    if !state
        .cache
        .allow(
            &subject,
            state.config.request_limit_count,
            Duration::from_secs(state.config.request_limit_ttl),
        )
        .await
    {
        bot.send_message(msg.chat.id, "请求次数已达到上限，请稍后再试")
            .await?;
        return Ok(());
    }
    let requests = urls
        .into_iter()
        .map(|url| ParseRequest {
            url,
            options: options.clone(),
        })
        .collect();
    for result in parse_cached(&state, requests).await {
        match result {
            Ok(content) => {
                if let Err(err) = send_content(&bot, &msg, &state, content, &options).await {
                    error!(?err, "发送内容失败");
                    bot.send_message(msg.chat.id, format!("媒体发送失败：{err}"))
                        .await?;
                }
            }
            Err(err) => {
                warn!(?err, "解析失败");
                if msg.chat.is_private() || text.starts_with('/') {
                    bot.send_message(msg.chat.id, err.to_string()).await?;
                }
            }
        }
    }
    Ok(())
}

async fn handle_login(bot: Bot, msg: &Message, state: &BotState, text: &str) -> ResponseResult<()> {
    let Some(user) = msg.from.as_ref() else {
        return Ok(());
    };
    if !msg.chat.is_private() {
        bot.send_message(
            msg.chat.id,
            "为避免登录二维码泄露，请在 Bot 私聊中使用 /login。",
        )
        .await?;
        return Ok(());
    }
    let Some(admin) = state.config.admin_user_id else {
        bot.send_message(
            msg.chat.id,
            format!(
                "当前未配置 ADMIN_USER_ID。你的 Telegram 用户 ID 是 {}。",
                user.id.0
            ),
        )
        .await?;
        return Ok(());
    };
    if user.id.0 != admin {
        bot.send_message(msg.chat.id, "你无权更新 Bot 的平台凭证。")
            .await?;
        return Ok(());
    }

    let target = text
        .split_whitespace()
        .nth(1)
        .unwrap_or("bili")
        .to_ascii_lowercase();
    let (challenge, is_netease, caption) = match target.as_str() {
        "bili" | "bilibili" | "b站" => (
            state.login.bilibili_challenge().await,
            false,
            "请使用哔哩哔哩客户端扫码并确认登录，二维码约 3 分钟后过期。",
        ),
        "netease" | "163" | "music" | "网易云" => (
            state.login.netease_challenge().await,
            true,
            "请使用网易云音乐客户端扫码并确认登录。",
        ),
        _ => {
            bot.send_message(msg.chat.id, "用法：/login bili 或 /login netease")
                .await?;
            return Ok(());
        }
    };
    let challenge = match challenge {
        Ok(challenge) => challenge,
        Err(error) => {
            bot.send_message(msg.chat.id, format!("生成登录二维码失败：{error}"))
                .await?;
            return Ok(());
        }
    };
    bot.send_photo(
        msg.chat.id,
        InputFile::memory(challenge.image).file_name(if is_netease {
            "netease-login.png"
        } else {
            "bilibili-login.png"
        }),
    )
    .caption(caption)
    .await?;

    let chat = msg.chat.id;
    let login = state.login.clone();
    tokio::spawn(async move {
        let mut scanned_notice_sent = false;
        for _ in 0..65 {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let result = if is_netease {
                login.poll_netease(&challenge.key).await
            } else {
                login.poll_bilibili(&challenge.key).await
            };
            match result {
                Ok(LoginPoll::Waiting) => {}
                Ok(LoginPoll::Scanned) => {
                    if !scanned_notice_sent {
                        let _ = bot
                            .send_message(chat, "已扫码，请在手机客户端中确认登录。")
                            .await;
                        scanned_notice_sent = true;
                    }
                }
                Ok(LoginPoll::Success) => {
                    let platform = if is_netease { "网易云" } else { "Bilibili" };
                    let _ = bot
                        .send_message(
                            chat,
                            format!("{platform} 扫码登录成功，Cookie 已持久化并立即生效。"),
                        )
                        .await;
                    return;
                }
                Ok(LoginPoll::Expired) => {
                    let _ = bot
                        .send_message(chat, "登录二维码已过期，请重新发送 /login。")
                        .await;
                    return;
                }
                Err(error) => {
                    error!(?error, "扫码登录轮询失败");
                    let _ = bot
                        .send_message(chat, format!("扫码登录失败：{error}"))
                        .await;
                    return;
                }
            }
        }
        let _ = bot
            .send_message(chat, "登录超时，请重新发送 /login。")
            .await;
    });
    Ok(())
}

async fn parse_cached(
    state: &BotState,
    requests: Vec<ParseRequest>,
) -> Vec<ProviderResult<ParsedContent>> {
    futures_util::future::join_all(requests.into_iter().map(|request| async move {
        let raw_key = format!(
            "{}:{:?}:{}:{}:{}",
            request.url,
            request.options.quality,
            request.options.file_mode,
            request.options.cover_only,
            state.credentials.revision()
        );
        let key = format!("parsed:{}", hex::encode(Sha256::digest(raw_key.as_bytes())));
        if let Some(value) = state.cache.get(&key).await
            && let Ok(content) = serde_json::from_str(&value)
        {
            return Ok(content);
        }
        let result = state.registry.parse_one(request).await;
        if let Ok(content) = &result
            && let Ok(value) = serde_json::to_string(content)
        {
            let ttl = if content.kind == ContentKind::Live {
                Duration::from_secs(300)
            } else {
                Duration::from_secs(3600)
            };
            state.cache.set(&key, value, ttl).await;
        }
        result
    }))
    .await
}

async fn send_content(
    bot: &Bot,
    msg: &Message,
    state: &BotState,
    content: ParsedContent,
    options: &ParseOptions,
) -> Result<()> {
    let _queue = state.queue.acquire().await?;
    let caption_text = caption(&content, 1024);
    let spoiler = media_has_spoiler(&state.config, &content)
        || (options.force_spoiler && has_visual_media(&content));
    if content.media.is_empty() {
        bot.send_message(msg.chat.id, caption(&content, 4096))
            .reply_parameters(ReplyParameters::new(msg.id))
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }
    let selected: Vec<_> = if options.cover_only {
        content
            .media
            .iter()
            .filter(|m| m.kind == MediaKind::Photo)
            .take(1)
            .collect()
    } else {
        content.media.iter().collect()
    };
    if selected.is_empty() {
        if let Some(url) = content.media.iter().find_map(|m| m.thumbnail_url.clone()) {
            bot.send_photo(msg.chat.id, InputFile::url(url.parse()?))
                .reply_parameters(ReplyParameters::new(msg.id))
                .has_spoiler(spoiler)
                .caption(caption_text)
                .parse_mode(ParseMode::Html)
                .await?;
        } else {
            bot.send_message(msg.chat.id, caption_text)
                .reply_parameters(ReplyParameters::new(msg.id))
                .parse_mode(ParseMode::Html)
                .await?;
        }
        return Ok(());
    }
    if !options.file_mode
        && selected.len() > 1
        && selected.iter().all(|item| match item.kind {
            MediaKind::Video => true,
            MediaKind::Photo => item
                .width
                .zip(item.height)
                .is_some_and(|(width, height)| telegram_photo_dimensions_valid(width, height)),
            _ => false,
        })
    {
        for chunk in selected.chunks(10) {
            let mut group = Vec::new();
            let mut prepared = Vec::new();
            for (i, item) in chunk.iter().enumerate() {
                let input = input_for_item(
                    state,
                    &content,
                    item,
                    options.quality.unwrap_or(720),
                    &mut prepared,
                )
                .await?;
                let c = if i == 0 {
                    Some(caption_text.clone())
                } else {
                    None
                };
                group.push(match item.kind {
                    MediaKind::Video => {
                        let mut media = InputMediaVideo::new(input)
                            .caption(c.unwrap_or_default())
                            .parse_mode(ParseMode::Html)
                            .supports_streaming(true);
                        media.width = item.width.and_then(|value| u16::try_from(value).ok());
                        media.height = item.height.and_then(|value| u16::try_from(value).ok());
                        media.duration = item
                            .duration_secs
                            .and_then(|value| u16::try_from(value).ok());
                        media.cover = item
                            .thumbnail_url
                            .as_ref()
                            .and_then(|url| url.parse().ok())
                            .map(InputFile::url);
                        media.has_spoiler = spoiler;
                        InputMedia::Video(media)
                    }
                    _ => {
                        let mut media = InputMediaPhoto::new(input)
                            .caption(c.unwrap_or_default())
                            .parse_mode(ParseMode::Html);
                        media.has_spoiler = spoiler;
                        InputMedia::Photo(media)
                    }
                });
            }
            let sent = bot
                .send_media_group(msg.chat.id, group)
                .reply_parameters(ReplyParameters::new(msg.id))
                .await?;
            for ((item, message), prep) in chunk.iter().zip(sent.iter()).zip(
                prepared
                    .into_iter()
                    .map(Some)
                    .chain(std::iter::repeat_with(|| None)),
            ) {
                cache_message(state, item, message).await;
                if let Some(p) = prep {
                    state.media.cleanup(p).await;
                }
            }
        }
        return Ok(());
    }
    for (index, item) in selected.into_iter().enumerate() {
        let cached_kind = kind_name(item.kind);
        if let Some(file_id) = state
            .storage
            .get_file_id(&telegram_cache_key(item), cached_kind)
            .await?
        {
            send_cached(
                bot,
                item,
                file_id,
                SendOptions {
                    chat: msg.chat.id,
                    caption: if index == 0 { &caption_text } else { "" },
                    spoiler,
                    reply_to: msg.id,
                    title: &content.title,
                    performer: &content.author.name,
                    thumbnail: None,
                },
            )
            .await?;
            continue;
        }
        let prepared = state
            .media
            .prepare(&content, item, options.quality.unwrap_or(720))
            .await;
        let prepared_thumbnail = if matches!(item.kind, MediaKind::Audio | MediaKind::Video) {
            if let Some(url) = &item.thumbnail_url {
                match state.media.prepare_thumbnail(url, &item.cache_key).await {
                    Ok(path) => Some(path),
                    Err(err) => {
                        warn!(?err, "媒体封面准备失败，继续发送无封面媒体");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        let sent = match prepared.as_ref() {
            Ok(p) => {
                send_local(
                    bot,
                    &p.item,
                    &p.path,
                    options.file_mode,
                    p.force_document,
                    SendOptions {
                        chat: msg.chat.id,
                        caption: if index == 0 { &caption_text } else { "" },
                        spoiler,
                        reply_to: msg.id,
                        title: &content.title,
                        performer: &content.author.name,
                        thumbnail: prepared_thumbnail.as_deref(),
                    },
                )
                .await?
            }
            Err(err) => {
                warn!(?err, "媒体准备失败，回退到预览");
                if index == 0 {
                    if let Some(thumbnail) = &item.thumbnail_url {
                        bot.send_photo(msg.chat.id, InputFile::url(thumbnail.parse()?))
                            .reply_parameters(ReplyParameters::new(msg.id))
                            .has_spoiler(spoiler)
                            .caption(&caption_text)
                            .parse_mode(ParseMode::Html)
                            .await?
                    } else {
                        bot.send_message(msg.chat.id, &caption_text)
                            .reply_parameters(ReplyParameters::new(msg.id))
                            .parse_mode(ParseMode::Html)
                            .await?
                    }
                } else {
                    continue;
                }
            }
        };
        cache_message(state, item, &sent).await;
        if let Ok(p) = prepared {
            state.media.cleanup(p).await;
        }
        if let Some(path) = prepared_thumbnail {
            state.media.cleanup_path(path).await;
        }
    }
    Ok(())
}

async fn input_for_item(
    state: &BotState,
    content: &ParsedContent,
    item: &MediaItem,
    quality: u32,
    prepared: &mut Vec<crate::media::PreparedMedia>,
) -> Result<InputFile> {
    if let Some(id) = state
        .storage
        .get_file_id(&telegram_cache_key(item), kind_name(item.kind))
        .await?
    {
        return Ok(InputFile::file_id(teloxide::types::FileId(id)));
    }
    if !item.requires_download && item.headers.is_empty() {
        return Ok(InputFile::url(item.source_url.parse()?));
    }
    let p = state.media.prepare(content, item, quality).await?;
    let input = InputFile::file(p.path.clone());
    prepared.push(p);
    Ok(input)
}

#[derive(Clone, Copy)]
struct SendOptions<'a> {
    chat: ChatId,
    caption: &'a str,
    spoiler: bool,
    reply_to: MessageId,
    title: &'a str,
    performer: &'a str,
    thumbnail: Option<&'a std::path::Path>,
}

async fn send_local(
    bot: &Bot,
    item: &MediaItem,
    path: &std::path::Path,
    document: bool,
    force_document: bool,
    options: SendOptions<'_>,
) -> Result<Message> {
    let SendOptions {
        chat,
        caption,
        spoiler,
        reply_to,
        title,
        performer,
        thumbnail,
    } = options;
    let input = InputFile::file(path.to_path_buf()).file_name(item.filename.clone());
    // Telegram documents cannot carry a spoiler. Sensitive visual media stays
    // photo/video/animation even when /file was requested.
    let req = if force_document || (document && !spoiler) || item.kind == MediaKind::Document {
        bot.send_document(chat, input)
            .reply_parameters(ReplyParameters::new(reply_to))
            .caption(caption)
            .parse_mode(ParseMode::Html)
            .await?
    } else {
        match item.kind {
            MediaKind::Photo => {
                bot.send_photo(chat, input)
                    .reply_parameters(ReplyParameters::new(reply_to))
                    .has_spoiler(spoiler)
                    .caption(caption)
                    .parse_mode(ParseMode::Html)
                    .await?
            }
            MediaKind::Video => {
                let request = bot
                    .send_video(chat, input)
                    .reply_parameters(ReplyParameters::new(reply_to))
                    .has_spoiler(spoiler)
                    .caption(caption)
                    .parse_mode(ParseMode::Html)
                    .supports_streaming(true);
                let request = if let Some(duration) = item.duration_secs {
                    request.duration(duration.min(u64::from(u32::MAX)) as u32)
                } else {
                    request
                };
                let request = if let Some(width) = item.width {
                    request.width(width)
                } else {
                    request
                };
                let request = if let Some(height) = item.height {
                    request.height(height)
                } else {
                    request
                };
                let request = if let Some(path) = thumbnail {
                    request.thumbnail(InputFile::file(path.to_path_buf()).file_name("cover.jpg"))
                } else {
                    request
                };
                request.await?
            }
            MediaKind::Audio => {
                let request = bot
                    .send_audio(chat, input)
                    .reply_parameters(ReplyParameters::new(reply_to))
                    .title(title)
                    .performer(performer)
                    .caption(caption)
                    .parse_mode(ParseMode::Html);
                let request = if let Some(path) = thumbnail {
                    request.thumbnail(InputFile::file(path.to_path_buf()).file_name("cover.jpg"))
                } else {
                    request
                };
                request.await?
            }
            MediaKind::Animation => {
                bot.send_animation(chat, input)
                    .reply_parameters(ReplyParameters::new(reply_to))
                    .has_spoiler(spoiler)
                    .caption(caption)
                    .parse_mode(ParseMode::Html)
                    .await?
            }
            MediaKind::Document => unreachable!(),
        }
    };
    Ok(req)
}

async fn send_cached(
    bot: &Bot,
    item: &MediaItem,
    id: String,
    options: SendOptions<'_>,
) -> Result<Message> {
    let SendOptions {
        chat,
        caption,
        spoiler,
        reply_to,
        title,
        performer,
        thumbnail: _,
    } = options;
    let input = InputFile::file_id(teloxide::types::FileId(id));
    Ok(match item.kind {
        MediaKind::Photo => {
            bot.send_photo(chat, input)
                .reply_parameters(ReplyParameters::new(reply_to))
                .has_spoiler(spoiler)
                .caption(caption)
                .parse_mode(ParseMode::Html)
                .await?
        }
        MediaKind::Video => {
            bot.send_video(chat, input)
                .reply_parameters(ReplyParameters::new(reply_to))
                .has_spoiler(spoiler)
                .caption(caption)
                .parse_mode(ParseMode::Html)
                .await?
        }
        MediaKind::Audio => {
            bot.send_audio(chat, input)
                .reply_parameters(ReplyParameters::new(reply_to))
                .title(title)
                .performer(performer)
                .caption(caption)
                .parse_mode(ParseMode::Html)
                .await?
        }
        MediaKind::Animation => {
            bot.send_animation(chat, input)
                .reply_parameters(ReplyParameters::new(reply_to))
                .has_spoiler(spoiler)
                .caption(caption)
                .parse_mode(ParseMode::Html)
                .await?
        }
        MediaKind::Document => {
            bot.send_document(chat, input)
                .reply_parameters(ReplyParameters::new(reply_to))
                .caption(caption)
                .parse_mode(ParseMode::Html)
                .await?
        }
    })
}

async fn cache_message(state: &BotState, item: &MediaItem, msg: &Message) {
    let file = match item.kind {
        MediaKind::Photo => msg
            .photo()
            .and_then(|p| p.last())
            .map(|p| (&p.file.id, &p.file.unique_id)),
        MediaKind::Video => msg.video().map(|x| (&x.file.id, &x.file.unique_id)),
        MediaKind::Audio => msg.audio().map(|x| (&x.file.id, &x.file.unique_id)),
        MediaKind::Animation => msg.animation().map(|x| (&x.file.id, &x.file.unique_id)),
        MediaKind::Document => msg.document().map(|x| (&x.file.id, &x.file.unique_id)),
    };
    if let Some((id, unique)) = file {
        let _ = state
            .storage
            .put_file_id(
                &telegram_cache_key(item),
                kind_name(item.kind),
                id.0.as_str(),
                Some(unique.0.as_str()),
            )
            .await;
    }
}

async fn handle_inline(bot: Bot, query: InlineQuery, state: BotState) -> ResponseResult<()> {
    let urls = extract_urls(&query.query);
    let Some(url) = urls.first() else {
        bot.answer_inline_query(query.id, Vec::<InlineQueryResult>::new())
            .cache_time(1)
            .await?;
        return Ok(());
    };
    let result = state
        .registry
        .parse_one(ParseRequest {
            url: url.clone(),
            options: Default::default(),
        })
        .await;
    let results = match result {
        Ok(content) => vec![inline_result(&state, &content).await],
        Err(err) => vec![InlineQueryResult::Article(InlineQueryResultArticle::new(
            "error",
            "解析失败",
            InputMessageContent::Text(InputMessageContentText::new(err.to_string())),
        ))],
    };
    bot.answer_inline_query(query.id, results)
        .cache_time(30)
        .is_personal(true)
        .await?;
    Ok(())
}

async fn inline_result(state: &BotState, content: &ParsedContent) -> InlineQueryResult {
    let title = if content.title.is_empty() {
        format!("{} · {}", content.platform.as_str(), content.author.name)
    } else {
        content.title.clone()
    };
    let caption_text = caption(content, 1024);
    // Inline media results have no spoiler flag in Telegram Bot API. Use a text
    // article instead so sensitive media is never inserted without a mask.
    if media_has_spoiler(&state.config, content) {
        return article_result(content, title);
    }
    if let Some(item) = content.media.first() {
        if let Ok(Some(file_id)) = state
            .storage
            .get_file_id(&telegram_cache_key(item), kind_name(item.kind))
            .await
        {
            let id = FileId(file_id);
            return match item.kind {
                MediaKind::Photo => InlineQueryResult::CachedPhoto(
                    InlineQueryResultCachedPhoto::new(content.id.clone(), id)
                        .title(title)
                        .caption(caption_text)
                        .parse_mode(ParseMode::Html),
                ),
                MediaKind::Video | MediaKind::Animation => InlineQueryResult::CachedVideo(
                    InlineQueryResultCachedVideo::new(content.id.clone(), id, title)
                        .caption(caption_text)
                        .parse_mode(ParseMode::Html),
                ),
                MediaKind::Audio => InlineQueryResult::CachedAudio(
                    InlineQueryResultCachedAudio::new(content.id.clone(), id)
                        .caption(caption_text)
                        .parse_mode(ParseMode::Html),
                ),
                MediaKind::Document => article_result(content, title),
            };
        }
        if !item.requires_download
            && item.headers.is_empty()
            && let Ok(media_url) = item.source_url.parse::<reqwest::Url>()
        {
            if item.kind == MediaKind::Photo {
                let thumb = item
                    .thumbnail_url
                    .as_deref()
                    .unwrap_or(&item.source_url)
                    .parse()
                    .unwrap_or_else(|_| media_url.clone());
                return InlineQueryResult::Photo(
                    InlineQueryResultPhoto::new(content.id.clone(), media_url, thumb)
                        .title(title)
                        .caption(caption_text)
                        .parse_mode(ParseMode::Html),
                );
            }
            if item.kind == MediaKind::Video
                && let Some(thumb) = item.thumbnail_url.as_ref().and_then(|u| u.parse().ok())
            {
                return InlineQueryResult::Video(
                    InlineQueryResultVideo::new(
                        content.id.clone(),
                        media_url,
                        "video/mp4".parse().expect("valid MIME"),
                        thumb,
                        title,
                    )
                    .caption(caption_text)
                    .parse_mode(ParseMode::Html),
                );
            }
        }
    }
    article_result(content, title)
}

pub fn media_has_spoiler(config: &Config, content: &ParsedContent) -> bool {
    if !has_visual_media(content) {
        return false;
    }
    match config.media_spoiler_mode {
        MediaSpoilerMode::Always => true,
        MediaSpoilerMode::Off => false,
        MediaSpoilerMode::Auto => {
            content.sensitive && matches!(content.platform, Platform::X | Platform::Pixiv)
        }
    }
}

fn has_visual_media(content: &ParsedContent) -> bool {
    content.media.iter().any(|item| {
        matches!(
            item.kind,
            MediaKind::Photo | MediaKind::Video | MediaKind::Animation
        )
    })
}

fn article_result(content: &ParsedContent, title: String) -> InlineQueryResult {
    InlineQueryResult::Article(InlineQueryResultArticle::new(
        content.id.clone(),
        title,
        InputMessageContent::Text(
            InputMessageContentText::new(caption(content, 4096)).parse_mode(ParseMode::Html),
        ),
    ))
}

pub fn extract_urls(text: &str) -> Vec<String> {
    let re = Regex::new(
        r"(?i)(?:https?://[^\s<>，。！？、]+|(?:BV[0-9A-Za-z]{10})(?:\?[^\s<>，。！？、]*)?)",
    )
    .expect("valid URL extractor");
    let mut out = Vec::new();
    for m in re.find_iter(text) {
        let url = m
            .as_str()
            .trim_end_matches(|c: char| ",.!?，。！？）)]}".contains(c))
            .to_string();
        if !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

fn extract_message_urls(msg: &Message) -> Vec<String> {
    let mut urls = extract_urls(msg.text().or_else(|| msg.caption()).unwrap_or_default());
    for entity in msg
        .entities()
        .into_iter()
        .flatten()
        .chain(msg.caption_entities().into_iter().flatten())
    {
        if let MessageEntityKind::TextLink { url } = &entity.kind {
            let value = url.to_string();
            if !urls.contains(&value) {
                urls.push(value);
            }
        }
    }
    urls
}

fn parse_options(text: &str) -> ParseOptions {
    let lower = text.to_ascii_lowercase();
    let force_spoiler = lower
        .split_whitespace()
        .any(|value| value == "+sp" || value == "/spoiler" || value == "/mask")
        || lower.ends_with("+sp")
        || lower.starts_with("/spoiler")
        || lower.starts_with("/mask");
    ParseOptions {
        quality: Regex::new(r"(?i)(360|480|720|1080)p?")
            .ok()
            .and_then(|r| r.captures(text))
            .and_then(|c| c[1].parse().ok()),
        file_mode: lower.starts_with("/file"),
        cover_only: lower.starts_with("/cover"),
        force_spoiler,
    }
}

pub fn caption(content: &ParsedContent, max: usize) -> String {
    let title = ellipsize(&content.title, (max * 15 / 100).clamp(60, 300));
    let source = if title.is_empty() {
        content.platform.as_str().to_string()
    } else {
        format!("{}：{title}", content.platform.as_str())
    };
    let mut parts = vec![format!(
        "<a href=\"{}\">{}</a>",
        html_escape::encode_double_quoted_attribute(&content.canonical_url),
        html_escape::encode_text(&source)
    )];

    let mut summary = content.text.clone();
    if !content.collection_items.is_empty() {
        if !summary.is_empty() {
            summary.push_str("\n\n");
        }
        summary.push_str(
            &content
                .collection_items
                .iter()
                .take(30)
                .enumerate()
                .map(|(i, item)| format!("{}. {item}", i + 1))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if !summary.is_empty() {
        let summary = ellipsize(&summary, (max * 35 / 100).clamp(180, 1800));
        parts.push(format!(
            "<blockquote>{}</blockquote>",
            html_escape::encode_text(&summary)
        ));
    }

    if !content.author.name.is_empty() {
        let author_name = ellipsize(&content.author.name, (max * 10 / 100).clamp(40, 160));
        let author = html_escape::encode_text(&author_name);
        let author = content
            .author
            .url
            .as_ref()
            .map(|url| {
                format!(
                    "<a href=\"{}\">{author}</a>",
                    html_escape::encode_double_quoted_attribute(url)
                )
            })
            .unwrap_or_else(|| author.to_string());
        parts.push(format!("作者：{author}"));
    }

    let view_label = match content.platform {
        Platform::Bilibili | Platform::YouTube => "播放",
        _ => "浏览",
    };
    let stats = [
        (view_label, content.stats.views),
        ("点赞", content.stats.likes),
        ("分享", content.stats.reposts),
        ("评论", content.stats.replies),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|value| format!("{name} {}", format_count(value))))
    .collect::<Vec<_>>()
    .join("　");
    if !stats.is_empty() {
        parts.push(format!("<blockquote>{stats}</blockquote>"));
    }
    truncate_html(&parts.join("\n"), max)
}

fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut value = text.chars().take(max.saturating_sub(1)).collect::<String>();
    value.push('…');
    value
}

fn format_count(value: u64) -> String {
    let raw = value.to_string();
    let mut formatted = String::with_capacity(raw.len() + raw.len() / 3);
    for (index, ch) in raw.chars().enumerate() {
        if index > 0 && (raw.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    formatted
}

fn truncate_html(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.into();
    }
    // 不能在 Telegram HTML 标签或实体中间截断，否则整条消息会被拒绝。
    // 首个链接是原始页面；即使正文超长也始终保留它。
    let anchor_re = Regex::new(r#"^<a href="[^"]+">[^<]*</a>"#).expect("valid source anchor regex");
    let source = anchor_re.find(text).map(|m| m.as_str()).unwrap_or_default();
    let body = text
        .strip_prefix(source)
        .unwrap_or(text)
        .trim_start_matches('\n');
    let prefix = if source.is_empty() {
        String::new()
    } else {
        format!("{source}\n\n")
    };
    if prefix.chars().count() + 1 >= max {
        return source.chars().take(max).collect();
    }

    let without_tags = Regex::new(r"<[^>]+>")
        .expect("valid HTML tag regex")
        .replace_all(body, "");
    let plain = html_escape::decode_html_entities(&without_tags);
    let mut escaped = prefix;
    for ch in plain.chars() {
        let encoded = html_escape::encode_text(&ch.to_string()).to_string();
        if escaped.chars().count() + encoded.chars().count() + 1 > max {
            break;
        }
        escaped.push_str(&encoded);
    }
    escaped.push('…');
    escaped
}
fn telegram_cache_key(item: &MediaItem) -> String {
    if item.kind == MediaKind::Video {
        format!("{}:telegram-video-v2", item.cache_key)
    } else {
        item.cache_key.clone()
    }
}

fn kind_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Photo => "photo",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        MediaKind::Animation => "animation",
        MediaKind::Document => "document",
    }
}
