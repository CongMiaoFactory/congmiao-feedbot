use std::{sync::Arc, time::Duration};

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use sha2::{Digest, Sha256};
use teloxide::{
    dispatching::UpdateFilterExt,
    dptree,
    error_handlers::LoggingErrorHandler,
    prelude::*,
    types::{
        ChatAction, FileId, InlineQueryResult, InlineQueryResultArticle,
        InlineQueryResultCachedAudio, InlineQueryResultCachedPhoto, InlineQueryResultCachedVideo,
        InlineQueryResultPhoto, InlineQueryResultVideo, InputFile, InputMedia, InputMediaPhoto,
        InputMediaVideo, InputMessageContent, InputMessageContentText, MessageEntityKind,
        MessageId, ParseMode, ReplyParameters,
    },
};
use tokio::{sync::Semaphore, task::JoinHandle};
use tracing::{error, info, warn};

use crate::{
    Config, MediaSpoilerMode, ProviderRegistry, RuntimeCredentials,
    cache::AppCache,
    login::{LoginPoll, LoginService},
    media::{MediaProcessor, is_permanent_prepare_error},
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
        bot.send_message(
            msg.chat.id,
            "发送 X、YouTube、Pixiv、哔哩哔哩或网易云链接即可解析；链接后加 +sp 可遮罩，Pixiv 等多图可加 +p2 只发第 2 页。\n命令：/parse、/video [清晰度，默认 480p]、/file、/cover、/login [bili|netease]",
        )
        .await?;
        return Ok(());
    }
    if text.starts_with("/login") {
        handle_login(bot, &msg, &state, text).await?;
        return Ok(());
    }
    let mut global_options = parse_options(text);
    let mut urls = extract_message_urls(&msg);
    if urls.is_empty()
        && global_options.file_mode
        && let Some(replied) = msg.reply_to_message()
    {
        match state
            .storage
            .get_message_media(msg.chat.id.0, replied.id.0)
            .await
        {
            Ok(Some(target)) => {
                global_options.quality = global_options.quality.or(target.quality);
                global_options.media_cache_key = Some(target.media_cache_key);
                urls.push(target.canonical_url);
            }
            Ok(None) => {}
            Err(error) => warn!(%error, "读取回复媒体映射失败，尝试从消息链接解析"),
        }
    }
    if urls.is_empty()
        && (global_options.force_spoiler
            || global_options.page.is_some()
            || global_options.file_mode
            || global_options.cover_only)
        && let Some(replied) = msg.reply_to_message()
    {
        urls = extract_message_urls(replied);
    }
    // 剥离链接后缀（+p2 / +sp），先匹配平台，再解析；未匹配到则不发状态。
    let requests: Vec<ParseRequest> = urls
        .into_iter()
        .filter_map(|url| {
            let (clean_url, flags) = strip_url_flags(&url);
            if !state.registry.supports(&clean_url) {
                return None;
            }
            let mut options = global_options.clone();
            if flags.force_spoiler {
                options.force_spoiler = true;
            }
            if let Some(page) = flags.page {
                options.page = Some(page);
            }
            Some(ParseRequest {
                url: clean_url,
                options,
            })
        })
        .collect();
    if requests.is_empty() {
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
    // 匹配到支持链接后再显示 Telegram 原生会话状态。
    let status = ChatStatus::start(bot.clone(), msg.chat.id, ChatAction::Typing).await;
    for result in parse_cached(&state, requests).await {
        match result {
            Ok((content, options)) => {
                status.set(chat_action_for_content(&content)).await;
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
    status.finish();
    Ok(())
}

/// 通过 `sendChatAction` 维持客户端侧状态提示（约每 5 秒需刷新一次）。
struct ChatStatus {
    bot: Bot,
    chat_id: ChatId,
    action_task: JoinHandle<()>,
    action: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

impl ChatStatus {
    async fn start(bot: Bot, chat_id: ChatId, action: ChatAction) -> Self {
        let action_code =
            std::sync::Arc::new(std::sync::atomic::AtomicU8::new(action_code(action)));
        let _ = bot.send_chat_action(chat_id, action).await;
        let action_task = {
            let bot = bot.clone();
            let action_code = action_code.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    let action =
                        action_from_code(action_code.load(std::sync::atomic::Ordering::Relaxed));
                    let _ = bot.send_chat_action(chat_id, action).await;
                }
            })
        };
        Self {
            bot,
            chat_id,
            action_task,
            action: action_code,
        }
    }

    async fn set(&self, action: ChatAction) {
        self.action
            .store(action_code(action), std::sync::atomic::Ordering::Relaxed);
        let _ = self.bot.send_chat_action(self.chat_id, action).await;
    }

    fn finish(self) {}
}

// 发送流程出错提前返回时也必须停掉刷新任务，否则会永远向会话发送状态。
impl Drop for ChatStatus {
    fn drop(&mut self) {
        self.action_task.abort();
    }
}

fn action_code(action: ChatAction) -> u8 {
    match action {
        ChatAction::Typing => 0,
        ChatAction::UploadPhoto => 1,
        ChatAction::UploadVideo => 2,
        ChatAction::UploadDocument => 3,
        ChatAction::UploadVoice => 4,
        _ => 0,
    }
}

fn action_from_code(code: u8) -> ChatAction {
    match code {
        1 => ChatAction::UploadPhoto,
        2 => ChatAction::UploadVideo,
        3 => ChatAction::UploadDocument,
        4 => ChatAction::UploadVoice,
        _ => ChatAction::Typing,
    }
}

fn chat_action_for_content(content: &ParsedContent) -> ChatAction {
    match content.media.first().map(|item| item.kind) {
        Some(MediaKind::Video | MediaKind::Animation) => ChatAction::UploadVideo,
        Some(MediaKind::Photo) => ChatAction::UploadPhoto,
        Some(MediaKind::Audio) => ChatAction::UploadVoice,
        Some(MediaKind::Document) => ChatAction::UploadDocument,
        None => ChatAction::Typing,
    }
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
) -> Vec<ProviderResult<(ParsedContent, ParseOptions)>> {
    futures_util::future::join_all(requests.into_iter().map(|request| async move {
        let options = request.options.clone();
        // 页码只影响发送选择，不影响上游解析结果，因此不进入解析缓存键。
        let raw_key = format!(
            "{}:{:?}:{}",
            request.url,
            request.options.quality,
            state.credentials.revision()
        );
        let key = format!("parsed:{}", hex::encode(Sha256::digest(raw_key.as_bytes())));
        if let Some(value) = state.cache.get(&key).await
            && let Ok(content) = serde_json::from_str(&value)
        {
            return Ok((content, options));
        }
        let result = state.registry.parse_one(request).await;
        if let Ok(content) = &result
            && let Ok(value) = serde_json::to_string(content)
        {
            // 需要下载的媒体常带签名 URL，缓存不宜过久。
            let ttl = if content.kind == ContentKind::Live {
                Duration::from_secs(300)
            } else if content.media.iter().any(|item| item.requires_download) {
                Duration::from_secs(600)
            } else {
                Duration::from_secs(3600)
            };
            state.cache.set(&key, value, ttl).await;
        }
        result.map(|content| (content, options))
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
    let selected = select_media_items(&content, options)?;
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
        && selected
            .iter()
            .all(|item| matches!(item.kind, MediaKind::Photo | MediaKind::Video))
    {
        let mut offset = 0;
        for size in media_group_sizes(selected.len()) {
            let chunk = &selected[offset..offset + size];
            offset += size;
            let mut group = Vec::new();
            let mut prepared = Vec::new();
            for (i, item) in chunk.iter().enumerate() {
                let (input, resolved, prepared_media) =
                    match input_for_item(state, &content, item, options).await {
                        Ok(entry) => entry,
                        Err(error) => {
                            for prepared_media in prepared.into_iter().flatten() {
                                state.media.cleanup(prepared_media).await;
                            }
                            return Err(error);
                        }
                    };
                prepared.push(prepared_media);
                let c = if offset == size && i == 0 {
                    Some(caption_text.clone())
                } else {
                    None
                };
                group.push(match resolved.kind {
                    MediaKind::Video => {
                        let mut media = InputMediaVideo::new(input)
                            .caption(c.unwrap_or_default())
                            .parse_mode(ParseMode::Html)
                            .supports_streaming(true);
                        media.width = resolved.width.and_then(|value| u16::try_from(value).ok());
                        media.height = resolved.height.and_then(|value| u16::try_from(value).ok());
                        media.duration = resolved
                            .duration_secs
                            .and_then(|value| u16::try_from(value).ok());
                        media.cover = resolved
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
            let sent = match bot
                .send_media_group(msg.chat.id, group)
                .reply_parameters(ReplyParameters::new(msg.id))
                .await
            {
                Ok(sent) => sent,
                Err(error) => {
                    for prepared_media in prepared.into_iter().flatten() {
                        state.media.cleanup(prepared_media).await;
                    }
                    return Err(error.into());
                }
            };
            for ((item, message), prep) in chunk.iter().zip(sent.iter()).zip(prepared) {
                let telegram_item = prep.as_ref().map(|prepared| &prepared.item).unwrap_or(item);
                cache_message(state, telegram_item, item, &content, options, message).await;
                if let Some(prepared_media) = prep {
                    state.media.cleanup(prepared_media).await;
                }
            }
        }
        return Ok(());
    }
    for (index, item) in selected.into_iter().enumerate() {
        if let Some((cached_kind, file_id)) =
            lookup_cached_file(state, item, options.file_mode).await?
        {
            let sent = send_cached(
                bot,
                item,
                cached_kind,
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
            cache_message(state, item, item, &content, options, &sent).await;
            continue;
        }
        let prepared = prepare_media_with_refresh(state, &content, item, options).await;
        let prepared_thumbnail = match (prepared.as_ref(), item.kind) {
            (Ok(prepared_media), MediaKind::Video) => {
                match state
                    .media
                    .prepare_video_thumbnail(&prepared_media.path, &item.cache_key)
                    .await
                {
                    Ok(path) => Some(path),
                    Err(local_err) => {
                        if let Some(url) = &item.thumbnail_url {
                            match state.media.prepare_thumbnail(url, &item.cache_key).await {
                                Ok(path) => Some(path),
                                Err(err) => {
                                    warn!(
                                        local = %local_err,
                                        remote = %err,
                                        "视频封面准备失败，继续发送无封面视频"
                                    );
                                    None
                                }
                            }
                        } else {
                            warn!(error = %local_err, "视频封面准备失败，继续发送无封面视频");
                            None
                        }
                    }
                }
            }
            (_, MediaKind::Audio) => {
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
            }
            _ => None,
        };
        let send_result: Result<Message> = match prepared.as_ref() {
            Ok(p) => {
                send_local(
                    bot,
                    &p.item,
                    &p.path,
                    options.file_mode,
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
                .await
            }
            Err(err) => {
                warn!(?err, "媒体准备失败，回退到预览");
                if index == 0 {
                    if let Some(thumbnail) = &item.thumbnail_url {
                        Ok(bot
                            .send_photo(msg.chat.id, InputFile::url(thumbnail.parse()?))
                            .reply_parameters(ReplyParameters::new(msg.id))
                            .has_spoiler(spoiler)
                            .caption(&caption_text)
                            .parse_mode(ParseMode::Html)
                            .await?)
                    } else {
                        Ok(bot
                            .send_message(msg.chat.id, &caption_text)
                            .reply_parameters(ReplyParameters::new(msg.id))
                            .parse_mode(ParseMode::Html)
                            .await?)
                    }
                } else {
                    if let Some(path) = prepared_thumbnail {
                        state.media.cleanup_path(path).await;
                    }
                    continue;
                }
            }
        };
        let sent = match send_result {
            Ok(sent) => sent,
            Err(error) => {
                if let Ok(prepared_media) = prepared {
                    state.media.cleanup(prepared_media).await;
                }
                if let Some(path) = prepared_thumbnail {
                    state.media.cleanup_path(path).await;
                }
                return Err(error);
            }
        };
        let telegram_item = prepared
            .as_ref()
            .map(|prepared| &prepared.item)
            .unwrap_or(item);
        cache_message(state, telegram_item, item, &content, options, &sent).await;
        if let Ok(p) = prepared {
            state.media.cleanup(p).await;
        }
        if let Some(path) = prepared_thumbnail {
            state.media.cleanup_path(path).await;
        }
    }
    Ok(())
}

async fn prepare_media_with_refresh(
    state: &BotState,
    content: &ParsedContent,
    item: &MediaItem,
    options: &ParseOptions,
) -> Result<crate::media::PreparedMedia> {
    let quality = options.quality_or_default();
    match prepare_media_for_options(state, content, item, quality, options.file_mode).await {
        Ok(prepared) => Ok(prepared),
        Err(initial_error) if is_permanent_prepare_error(&initial_error) => Err(initial_error),
        Err(initial_error) => {
            warn!(
                error = %initial_error,
                url = %content.canonical_url,
                "媒体准备失败，重新解析上游地址后再试"
            );
            let refreshed = state
                .registry
                .parse_one(ParseRequest {
                    url: content.canonical_url.clone(),
                    options: options.clone(),
                })
                .await
                .map_err(|error| {
                    anyhow!("重新解析媒体地址失败: {error}; 初始错误: {initial_error}")
                })?;
            // 刷新后的签名地址写回本地解析缓存（按 canonical URL 键），缩短后续失败窗口。
            if let Ok(value) = serde_json::to_string(&refreshed) {
                let raw_key = format!(
                    "{}:{:?}:{}",
                    content.canonical_url,
                    options.quality,
                    state.credentials.revision()
                );
                let key = format!("parsed:{}", hex::encode(Sha256::digest(raw_key.as_bytes())));
                let ttl = if refreshed.kind == ContentKind::Live {
                    Duration::from_secs(300)
                } else {
                    Duration::from_secs(600)
                };
                state.cache.set(&key, value, ttl).await;
            }
            let refreshed_item = refreshed
                .media
                .iter()
                .find(|candidate| candidate.cache_key == item.cache_key)
                .or_else(|| {
                    refreshed.media.iter().find(|candidate| {
                        candidate.kind == item.kind && candidate.filename == item.filename
                    })
                })
                .ok_or_else(|| anyhow!("重新解析后未找到对应媒体；初始错误: {initial_error}"))?;
            prepare_media_for_options(
                state,
                &refreshed,
                refreshed_item,
                quality,
                options.file_mode,
            )
            .await
            .map_err(|error| {
                anyhow!("刷新地址后媒体准备仍失败: {error}; 初始错误: {initial_error}")
            })
        }
    }
}

async fn prepare_media_for_options(
    state: &BotState,
    content: &ParsedContent,
    item: &MediaItem,
    quality: u32,
    file_mode: bool,
) -> Result<crate::media::PreparedMedia> {
    if file_mode && item.kind == MediaKind::Photo {
        state.media.prepare_original(content, item, quality).await
    } else {
        state.media.prepare(content, item, quality).await
    }
}

async fn input_for_item(
    state: &BotState,
    content: &ParsedContent,
    item: &MediaItem,
    options: &ParseOptions,
) -> Result<(InputFile, MediaItem, Option<crate::media::PreparedMedia>)> {
    // 媒体组的条目类型由 item.kind 决定，document 等异类 file_id 不能混入，
    // 否则整组 sendMediaGroup 都会被 Telegram 拒绝。
    if let Some((cached_kind, id)) = lookup_cached_file(state, item, options.file_mode).await?
        && cached_kind == item.kind
    {
        return Ok((
            InputFile::file_id(teloxide::types::FileId(id)),
            item.clone(),
            None,
        ));
    }
    // 未缓存图片必须先下载并检查真实尺寸，不能仅依赖 provider 元数据或远程 URL。
    if item.kind != MediaKind::Photo && !item.requires_download && item.headers.is_empty() {
        return Ok((InputFile::url(item.source_url.parse()?), item.clone(), None));
    }
    let prepared = prepare_media_with_refresh(state, content, item, options).await?;
    let resolved = prepared.item.clone();
    let input = InputFile::file(prepared.path.clone());
    Ok((input, resolved, Some(prepared)))
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
    // `/file` 明确要求原文件；Document 不支持 spoiler，但不能因此悄悄改回 Photo。
    let req = if document || item.kind == MediaKind::Document {
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
    _item: &MediaItem,
    cached_kind: MediaKind,
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
    Ok(match cached_kind {
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
            // document file_id 不能当作 photo 发送；超尺寸图片只能继续以文件复用。
            bot.send_document(chat, input)
                .reply_parameters(ReplyParameters::new(reply_to))
                .caption(caption)
                .parse_mode(ParseMode::Html)
                .await?
        }
    })
}

async fn lookup_cached_file(
    state: &BotState,
    item: &MediaItem,
    file_mode: bool,
) -> Result<Option<(MediaKind, String)>> {
    let key = telegram_cache_key(item);
    if file_mode {
        return Ok(state
            .storage
            .get_file_id(&key, "document")
            .await?
            .map(|file_id| (MediaKind::Document, file_id)));
    }
    if item.kind == MediaKind::Photo {
        let preview_key = telegram_photo_preview_cache_key(item);
        if let Some(file_id) = state.storage.get_file_id(&preview_key, "photo").await? {
            return Ok(Some((MediaKind::Photo, file_id)));
        }
    }
    Ok(state
        .storage
        .get_file_id(&key, kind_name(item.kind))
        .await?
        .map(|file_id| (item.kind, file_id)))
}

async fn cache_message(
    state: &BotState,
    telegram_item: &MediaItem,
    source_item: &MediaItem,
    content: &ParsedContent,
    options: &ParseOptions,
    msg: &Message,
) {
    // 按 Telegram 实际返回的消息类型缓存，避免“图片被强制按文件发送”后写错 kind。
    let file = if let Some(photo) = msg.photo().and_then(|photos| photos.last()) {
        Some(("photo", &photo.file.id, &photo.file.unique_id))
    } else if let Some(video) = msg.video() {
        Some(("video", &video.file.id, &video.file.unique_id))
    } else if let Some(audio) = msg.audio() {
        Some(("audio", &audio.file.id, &audio.file.unique_id))
    } else if let Some(animation) = msg.animation() {
        Some(("animation", &animation.file.id, &animation.file.unique_id))
    } else {
        msg.document()
            .map(|document| ("document", &document.file.id, &document.file.unique_id))
    };
    if let Some((kind, id, unique)) = file {
        let cache_key =
            if kind == "photo" && source_item.kind == MediaKind::Photo && !options.file_mode {
                telegram_photo_preview_cache_key(source_item)
            } else {
                telegram_cache_key(telegram_item)
            };
        let _ = state
            .storage
            .put_file_id(&cache_key, kind, id.0.as_str(), Some(unique.0.as_str()))
            .await;
    }
    if let Err(error) = state
        .storage
        .put_message_media(
            msg.chat.id.0,
            msg.id.0,
            &content.canonical_url,
            &source_item.cache_key,
            Some(options.quality_or_default()),
        )
        .await
    {
        warn!(%error, "保存回复媒体映射失败");
    }
}

async fn handle_inline(bot: Bot, query: InlineQuery, state: BotState) -> ResponseResult<()> {
    let requests: Vec<_> = extract_urls(&query.query)
        .into_iter()
        .filter_map(|url| {
            let (clean_url, flags) = strip_url_flags(&url);
            if !state.registry.supports(&clean_url) {
                return None;
            }
            Some(ParseRequest {
                url: clean_url,
                options: ParseOptions {
                    force_spoiler: flags.force_spoiler,
                    page: flags.page,
                    ..Default::default()
                },
            })
        })
        .collect();
    let Some(request) = requests.into_iter().next() else {
        bot.answer_inline_query(query.id, Vec::<InlineQueryResult>::new())
            .cache_time(1)
            .await?;
        return Ok(());
    };
    let page = request.options.page;
    let result = state.registry.parse_one(request).await;
    let results = match result {
        Ok(mut content) => {
            if let Some(page) = page {
                match select_media_items(
                    &content,
                    &ParseOptions {
                        page: Some(page),
                        ..Default::default()
                    },
                ) {
                    Ok(selected) => {
                        content.media = selected.into_iter().cloned().collect();
                    }
                    Err(err) => {
                        bot.answer_inline_query(
                            query.id,
                            vec![InlineQueryResult::Article(InlineQueryResultArticle::new(
                                "error",
                                "页码无效",
                                InputMessageContent::Text(InputMessageContentText::new(
                                    err.to_string(),
                                )),
                            ))],
                        )
                        .cache_time(1)
                        .await?;
                        return Ok(());
                    }
                }
            }
            vec![inline_result(&state, &content).await]
        }
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
        if let Ok(Some((cached_kind, file_id))) = lookup_cached_file(state, item, false).await {
            let id = FileId(file_id);
            return match cached_kind {
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UrlFlags {
    pub force_spoiler: bool,
    pub page: Option<u32>,
}

/// 从链接末尾剥离 `+p2` / `+sp` 等标志，得到干净 URL 与选项。
pub fn strip_url_flags(url: &str) -> (String, UrlFlags) {
    let mut clean = url.trim().to_string();
    let mut flags = UrlFlags::default();
    let page_suffix = Regex::new(r"(?i)\+p(\d+)$").expect("valid page flag regex");
    loop {
        let lower = clean.to_ascii_lowercase();
        if lower.ends_with("+sp") {
            clean.truncate(clean.len().saturating_sub(3));
            flags.force_spoiler = true;
            continue;
        }
        if let Some(captures) = page_suffix.captures(&clean) {
            let full = captures.get(0).expect("regex match");
            if full.end() == clean.len()
                && let Ok(page) = captures[1].parse::<u32>()
                && page >= 1
            {
                flags.page = Some(page);
                clean.truncate(full.start());
                continue;
            }
        }
        break;
    }
    (clean, flags)
}

pub fn select_media_items<'a>(
    content: &'a ParsedContent,
    options: &ParseOptions,
) -> Result<Vec<&'a MediaItem>> {
    if let Some(cache_key) = &options.media_cache_key {
        let item = content
            .media
            .iter()
            .find(|item| &item.cache_key == cache_key)
            .ok_or_else(|| anyhow!("被回复的原媒体已不存在或内容结构已变化"))?;
        return Ok(vec![item]);
    }
    if options.cover_only {
        return Ok(content
            .media
            .iter()
            .filter(|item| item.kind == MediaKind::Photo)
            .take(1)
            .collect());
    }
    let Some(page) = options.page else {
        return Ok(content.media.iter().collect());
    };
    // Pixiv 等多图：页码按图片 1-based；若没有图片则回退到全部媒体序号。
    let photos: Vec<_> = content
        .media
        .iter()
        .filter(|item| item.kind == MediaKind::Photo)
        .collect();
    let pool = if photos.is_empty() {
        content.media.iter().collect::<Vec<_>>()
    } else {
        photos
    };
    if pool.is_empty() {
        bail!("当前内容没有可按页选择的媒体");
    }
    let index = usize::try_from(page).unwrap_or(0);
    if index == 0 || index > pool.len() {
        bail!("指定页码 {page} 超出范围（共 {} 页）", pool.len());
    }
    Ok(vec![pool[index - 1]])
}

/// sendMediaGroup 每组要求 2-10 项；把媒体均匀分组，避免出现单独 1 项的尾组。
pub fn media_group_sizes(total: usize) -> Vec<usize> {
    if total == 0 {
        return Vec::new();
    }
    let group_count = total.div_ceil(10);
    let base_size = total / group_count;
    let extra = total % group_count;
    (0..group_count)
        .map(|index| base_size + usize::from(index < extra))
        .collect()
}

pub fn extract_urls(text: &str) -> Vec<String> {
    // 裸 av 号要求至少两位数字，避免把 “AV1” 编解码器等普通词误当链接。
    let re = Regex::new(
        r"(?i)(?:https?://[^\s<>，。！？、]+|\b(?:BV[0-9A-Za-z]{10}|av\d{2,})\b(?:\?[^\s<>，。！？、]*)?)",
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
    // 清晰度只接受独立参数（如 `/video 480p`），避免误读链接里的数字串。
    let quality = lower.split_whitespace().find_map(|token| {
        let value = token.strip_suffix('p').unwrap_or(token);
        matches!(value, "360" | "480" | "720" | "1080")
            .then(|| value.parse().ok())
            .flatten()
    });
    // 独立页码参数：`+p2` / 回复链接时单独发 `+p3`。
    let page = lower.split_whitespace().find_map(|token| {
        token
            .strip_prefix("+p")
            .filter(|value| !value.is_empty() && value.chars().all(|c| c.is_ascii_digit()))
            .and_then(|value| value.parse().ok())
            .filter(|&value| value >= 1)
    });
    ParseOptions {
        quality,
        file_mode: lower.starts_with("/file"),
        cover_only: lower.starts_with("/cover"),
        force_spoiler,
        page,
        media_cache_key: None,
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
        let expandable = summary.chars().count() > 300;
        let summary = ellipsize(&summary, (max * 35 / 100).clamp(180, 1800));
        let attribute = if expandable { " expandable" } else { "" };
        parts.push(format!(
            "<blockquote{attribute}>{}</blockquote>",
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
fn telegram_photo_preview_cache_key(item: &MediaItem) -> String {
    format!("{}:telegram-photo-preview-v1", item.cache_key)
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
