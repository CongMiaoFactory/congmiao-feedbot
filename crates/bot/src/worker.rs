use crate::album::{self, SendPlan};
use crate::app::{App, OutputSettings, TgBot};
use crate::publisher::OverflowCtx;
use crate::queue::QueuedJob;
use crate::render::{self, CaptionPage, RenderedText};
use feedbot_core::extract::{
    ExtractRequest, Extracted, MediaItem, MediaKind, MediaSource, RequesterInfo,
};
use feedbot_core::job::MediaCacheEntry;
use feedbot_core::media::MediaProcessor;
use feedbot_core::options::{merge_layers, split_option_tokens, OptionValue, OptionValues};
use feedbot_core::repo::{CredentialRepo, JobRepo, MediaCacheRepo};
use feedbot_core::{
    load_saved_options, ExtractError, JobState, MediaError, MediaInfo, TranscodeSpec, Ytdlp,
    YtdlpSpec,
};
use feedbot_extractors::canonicalize;
use feedbot_media::{Downloader, FfmpegCli, YtdlpCli};
use http::HeaderMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use teloxide::payloads::{
    SendAnimationSetters, SendAudioSetters, SendDocumentSetters, SendPhotoSetters,
    SendVideoSetters, SetMessageReactionSetters,
};
use teloxide::prelude::*;
use teloxide::sugar::request::RequestReplyExt;
use teloxide::types::{
    ChatAction, ChatId, FileId, FileUniqueId, InlineKeyboardMarkup, InputFile, InputMedia,
    InputMediaAudio, InputMediaDocument, InputMediaPhoto, InputMediaVideo, MessageId, ReactionType,
    ReplyParameters,
};
use teloxide::utils::html;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub async fn run_worker_pool(
    app: Arc<App>,
    mut rx: mpsc::Receiver<QueuedJob>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            job = rx.recv() => {
                let Some(job) = job else { break };
                let app = app.clone();
                tokio::spawn(async move {
                    if let Err(e) = process_job(app, job).await {
                        error!(error = %e, "job failed");
                    }
                });
            }
        }
    }
}

async fn process_job(app: Arc<App>, job: QueuedJob) -> anyhow::Result<()> {
    let span = tracing::info_span!("job", job_id = %job.id);
    let _g = span.enter();
    let mut presence = Presence::new(
        app.bot.clone(),
        ChatId(job.chat_id),
        job.message_id,
        app.progress_edit_interval,
    );

    if app.user_limiter.check_key(&(job.user_id as u64)).is_err() {
        presence.fail("你发得太快了，请稍后再试").await;
        app.store
            .set_state(job.id, JobState::Failed, Some("rate limited"))
            .await?;
        return Ok(());
    }

    let mut last_err = None;
    for attempt in 1..=app.max_attempts {
        app.store.bump_attempt(job.id).await.ok();
        match run_once(&app, &job, &mut presence).await {
            Ok(()) => {
                app.store.set_state(job.id, JobState::Done, None).await.ok();
                presence.succeed().await;
                return Ok(());
            }
            Err(e) if is_retryable(&e) && attempt < app.max_attempts => {
                warn!(attempt, error = %e, "retrying job");
                last_err = Some(e);
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
            }
            Err(e) => {
                presence.fail(&format!("失败：{e}")).await;
                app.store
                    .set_state(job.id, JobState::Failed, Some(&e.to_string()))
                    .await
                    .ok();
                return Ok(());
            }
        }
    }
    if let Some(e) = last_err {
        presence.fail(&format!("失败：{e}")).await;
        app.store
            .set_state(job.id, JobState::Failed, Some(&e.to_string()))
            .await
            .ok();
    }
    Ok(())
}

fn is_retryable(err: &anyhow::Error) -> bool {
    if let Some(e) = err.downcast_ref::<ExtractError>() {
        return e.retryable();
    }
    if let Some(MediaError::Download(_)) = err.downcast_ref::<MediaError>() {
        return true;
    }
    false
}

async fn run_once(app: &App, job: &QueuedJob, presence: &mut Presence) -> anyhow::Result<()> {
    presence.set_action(ChatAction::Typing).await;
    app.store
        .set_state(job.id, JobState::Extracting, None)
        .await?;

    let _extract_permit = app.extract_limit.acquire().await?;
    let url = canonicalize(&job.url);
    let Some(extractor) = app.registry.match_url(&url) else {
        return Err(ExtractError::NoExtractor.into());
    };
    let (tokens, leftover) = split_option_tokens(&job.raw_args);
    if !leftover.is_empty() {
        warn!(leftover = ?leftover, "ignored unrecognized option fragments");
    }
    let saved =
        load_saved_options(app.store.as_ref(), extractor.id(), job.user_id, job.chat_id).await?;
    let mut options = merge_layers(extractor.options(), &saved, &tokens)?;
    extractor.validate(&mut options)?;
    let credential = app
        .store
        .resolve(extractor.id(), job.user_id, job.chat_id)
        .await?;

    let source_key = OptionValues::cache_key(url.as_str(), extractor.options(), &options);
    let chat = ChatId(job.chat_id);
    let reply = reply_params(job);

    let req = ExtractRequest {
        url: url.clone(),
        options: options.clone(),
        credential,
        requester: RequesterInfo {
            user_id: job.user_id,
            chat_id: job.chat_id,
        },
    };
    let mut extracted = extractor.extract(&app.extract_ctx, &req).await?;
    drop(_extract_permit);

    let spoiler = media_spoiler(&options, extracted.nsfw);
    let total_items = extracted.items.len();

    let hits = app.store.get_cache_all(&source_key).await?;
    if !hits.is_empty() {
        info!("cache hit");
        presence.set_action(upload_action(hits[0].kind)).await;
        let send_total = hits.len();
        let rendered = render::build_caption_page(
            &extracted,
            CaptionPage {
                index: 0,
                total: send_total,
            },
        );
        let group_count = send_cached(
            &app.bot,
            chat,
            reply.as_ref(),
            &hits,
            &extracted,
            spoiler,
            app.output.album_max,
        )
        .await?;
        send_extras(&app.bot, chat, reply.as_ref(), &rendered).await?;
        deliver_overflow(
            app,
            job.chat_id,
            chat,
            reply.as_ref(),
            total_items,
            group_count,
            &[],
        )
        .await?;
        return Ok(());
    }

    app.store
        .set_state(job.id, JobState::Downloading, None)
        .await?;
    presence.set_action(ChatAction::Typing).await;

    let dir = tempfile::tempdir().map_err(|e| MediaError::Io(e.to_string()))?;
    let items = std::mem::take(&mut extracted.items);
    let (prepared, skipped) = prepare_all(app, items, dir.path()).await?;

    app.store
        .set_state(job.id, JobState::Uploading, None)
        .await?;
    let upload = prepared
        .first()
        .map(|item| upload_action(item.kind))
        .unwrap_or(ChatAction::Typing);
    presence.set_action(upload).await;

    let send_total = prepared.len();
    let rendered = render::build_caption_page(
        &extracted,
        CaptionPage {
            index: 0,
            total: send_total,
        },
    );

    let mut group_count = 0usize;
    let mut cache_entries = Vec::new();
    if prepared.is_empty() {
        if !rendered.caption.is_empty() {
            send_html(&app.bot, chat, reply.as_ref(), &rendered.caption).await?;
            group_count = 1;
        }
    } else {
        let kinds: Vec<MediaKind> = prepared.iter().map(|p| p.kind).collect();
        let plans = album::plan_groups_with_max(&kinds, app.output.album_max);
        group_count = plans.len();
        for plan in plans {
            match plan {
                SendPlan::Single(i) => {
                    let item = &prepared[i];
                    let cap = caption_for_index(&extracted, i, send_total);
                    let markup = single_keyboard(&extracted, send_total);
                    let msg = send_single_local(
                        &app.bot,
                        chat,
                        reply.as_ref(),
                        item,
                        cap.as_deref(),
                        spoiler,
                        markup.as_ref(),
                    )
                    .await?;
                    if let Some(entry) =
                        cache_entry_from_message(&source_key, item.idx as i32, item.kind, &msg)
                    {
                        cache_entries.push(entry);
                    }
                }
                SendPlan::Album(idxs) => {
                    let items: Vec<&PreparedMedia> = idxs.iter().map(|&i| &prepared[i]).collect();
                    let captions: Vec<Option<String>> = idxs
                        .iter()
                        .map(|&i| caption_for_index(&extracted, i, send_total))
                        .collect();
                    let msgs = send_album_local(
                        &app.bot,
                        chat,
                        reply.as_ref(),
                        &items,
                        &captions,
                        spoiler,
                    )
                    .await?;
                    for (item, msg) in items.into_iter().zip(msgs) {
                        if let Some(entry) =
                            cache_entry_from_message(&source_key, item.idx as i32, item.kind, &msg)
                        {
                            cache_entries.push(entry);
                        }
                    }
                }
            }
        }
        app.store.put_many(&cache_entries).await.ok();
    }

    send_extras(&app.bot, chat, reply.as_ref(), &rendered).await?;
    deliver_overflow(
        app,
        job.chat_id,
        chat,
        reply.as_ref(),
        total_items,
        group_count,
        &skipped,
    )
    .await?;
    drop(dir);
    Ok(())
}

async fn prepare_all(
    app: &App,
    items: Vec<MediaItem>,
    dir: &Path,
) -> anyhow::Result<(Vec<PreparedMedia>, Vec<String>)> {
    let mut set = JoinSet::new();
    for (idx, item) in items.into_iter().enumerate() {
        let downloader = app.downloader.clone();
        let ffmpeg = app.ffmpeg.clone();
        let ytdlp = app.ytdlp.clone();
        let media_limit = app.media_limit.clone();
        let dir = dir.to_path_buf();
        let output = app.output.clone();
        set.spawn(async move {
            let _permit = media_limit.acquire_owned().await?;
            prepare_one(idx, item, &downloader, &ffmpeg, &ytdlp, &dir, &output).await
        });
    }

    let mut prepared = Vec::new();
    let mut skipped = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined.map_err(|e| anyhow::anyhow!("download task: {e}"))?? {
            PrepareOutcome::Ready(item) => prepared.push(item),
            PrepareOutcome::Skip(reason) => skipped.push(reason),
        }
    }
    prepared.sort_by_key(|p| p.idx);
    Ok((prepared, skipped))
}

enum PrepareOutcome {
    Ready(PreparedMedia),
    Skip(String),
}

struct PreparedMedia {
    idx: usize,
    kind: MediaKind,
    path: PathBuf,
    filename: Option<String>,
    probe: Option<MediaInfo>,
    thumbnail: Option<PathBuf>,
}

async fn prepare_one(
    idx: usize,
    item: MediaItem,
    downloader: &Downloader,
    ffmpeg: &FfmpegCli,
    ytdlp: &YtdlpCli,
    dir: &Path,
    output: &OutputSettings,
) -> anyhow::Result<PrepareOutcome> {
    let cover = item.cover.clone();
    let mut provided_thumb = None;
    let (mut path, mut filename, mut kind) = match item.source {
        MediaSource::Direct { url, headers } => {
            let name = safe_name(item.filename.as_deref(), idx);
            let path = dir.join(&name);
            downloader.to_file(&url, &headers, &path).await?;
            (path, item.filename, item.kind)
        }
        MediaSource::Separated {
            video,
            audio,
            headers,
        } => {
            let vpath = dir.join(format!("{idx}_v.bin"));
            let apath = dir.join(format!("{idx}_a.bin"));
            let out = dir.join(format!("{idx}_out.mp4"));
            downloader.to_file(&video, &headers, &vpath).await?;
            downloader.to_file(&audio, &headers, &apath).await?;
            ffmpeg.remux(&[vpath, apath], &out).await?;
            (out, Some("video.mp4".into()), MediaKind::Video)
        }
        MediaSource::Segmented {
            manifest, headers, ..
        } => {
            let src = dir.join(format!("{idx}_src.bin"));
            let out = dir.join(format!("{idx}_out.mp4"));
            downloader.to_file(&manifest, &headers, &src).await?;
            ffmpeg.remux(&[src], &out).await?;
            (out, Some("video.mp4".into()), MediaKind::Video)
        }
        MediaSource::UgoiraZip {
            zip,
            headers,
            frames,
        } => {
            let zip_path = dir.join(format!("{idx}_ugoira.zip"));
            let work = dir.join(format!("{idx}_ugoira"));
            tokio::fs::create_dir_all(&work)
                .await
                .map_err(|e| MediaError::Io(e.to_string()))?;
            let out = dir.join(format!("{idx}_ugoira.mp4"));
            downloader.to_file(&zip, &headers, &zip_path).await?;
            ffmpeg
                .ugoira_to_mp4(&zip_path, &frames, &work, &out)
                .await?;
            (out, Some("ugoira.mp4".into()), MediaKind::Animation)
        }
        MediaSource::YtDlp {
            url,
            audio_only,
            max_height,
            thumbnail,
            cookies,
        } => {
            let dest = if audio_only {
                dir.join(format!("{idx}.m4a"))
            } else {
                dir.join(format!("{idx}.mp4"))
            };
            ytdlp
                .download(
                    &url,
                    &dest,
                    &YtdlpSpec {
                        audio_only,
                        max_height,
                        cookies,
                    },
                )
                .await?;
            if let Some(thumb_url) = thumbnail {
                let cover = dir.join(format!("{idx}_cover.jpg"));
                if downloader
                    .to_file(&thumb_url, &HeaderMap::new(), &cover)
                    .await
                    .is_ok()
                {
                    provided_thumb = Some(cover);
                }
            }
            let (filename, kind) = if audio_only {
                (Some("audio.m4a".into()), MediaKind::Audio)
            } else {
                (Some("video.mp4".into()), MediaKind::Video)
            };
            (dest, filename, kind)
        }
    };

    if provided_thumb.is_none() {
        if let Some(thumb_url) = cover {
            let cover_path = dir.join(format!("{idx}_cover.jpg"));
            if downloader
                .to_file(&thumb_url, &HeaderMap::new(), &cover_path)
                .await
                .is_ok()
            {
                provided_thumb = Some(cover_path);
            }
        }
    }

    let mut size = tokio::fs::metadata(&path)
        .await
        .map_err(|e| MediaError::Io(e.to_string()))?
        .len();

    if kind == MediaKind::Photo && size > output.max_photo_bytes {
        kind = MediaKind::Document;
    }

    if matches!(kind, MediaKind::Video | MediaKind::Animation) && size > output.max_upload_bytes {
        let tc = dir.join(format!("{idx}_tc.mp4"));
        let spec = TranscodeSpec {
            max_bytes: output.max_upload_bytes.saturating_sub(1024 * 1024).max(1),
            ..TranscodeSpec::default()
        };
        match ffmpeg.transcode(&path, &tc, &spec).await {
            Ok(()) => {
                path = tc;
                filename = Some("video.mp4".into());
                kind = MediaKind::Video;
                size = tokio::fs::metadata(&path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(size);
            }
            Err(e) => {
                return Ok(PrepareOutcome::Skip(format!(
                    "第 {} 项转码失败：{e}",
                    idx + 1
                )));
            }
        }
    }

    if size > output.max_upload_bytes {
        return Ok(PrepareOutcome::Skip(format!(
            "第 {} 项超过上传上限（{size} 字节）",
            idx + 1
        )));
    }

    let probe = if matches!(kind, MediaKind::Video | MediaKind::Animation) {
        ffmpeg.probe(&path).await.ok()
    } else {
        None
    };
    let thumbnail = if let Some(cover) = provided_thumb {
        Some(cover)
    } else if kind == MediaKind::Video {
        let thumb = dir.join(format!("{idx}_thumb.jpg"));
        if ffmpeg.thumbnail(&path, &thumb).await.is_ok() {
            Some(thumb)
        } else {
            None
        }
    } else {
        None
    };

    Ok(PrepareOutcome::Ready(PreparedMedia {
        idx,
        kind,
        path,
        filename,
        probe,
        thumbnail,
    }))
}

fn safe_name(name: Option<&str>, idx: usize) -> String {
    name.and_then(|n| Path::new(n).file_name())
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| format!("{idx}_{s}"))
        .unwrap_or_else(|| format!("item_{idx}"))
}

fn reply_params(job: &QueuedJob) -> Option<ReplyParameters> {
    if job.message_id == 0 {
        None
    } else {
        Some(ReplyParameters::new(MessageId(job.message_id)))
    }
}

fn local_file(path: &Path, filename: Option<&str>) -> InputFile {
    let mut file = InputFile::file(path.to_path_buf());
    if let Some(name) = filename {
        file = file.file_name(name.to_string());
    }
    file
}

fn to_u16(n: u32) -> u16 {
    n.min(u16::MAX as u32) as u16
}

async fn send_single_local(
    bot: &TgBot,
    chat: ChatId,
    reply: Option<&ReplyParameters>,
    item: &PreparedMedia,
    caption: Option<&str>,
    spoiler: bool,
    markup: Option<&InlineKeyboardMarkup>,
) -> anyhow::Result<Message> {
    let file = local_file(&item.path, item.filename.as_deref());
    match item.kind {
        MediaKind::Photo => {
            let mut req = bot.send_photo(chat, file);
            if let Some(c) = caption {
                req = req.caption(c.to_string());
            }
            if spoiler {
                req = req.has_spoiler(true);
            }
            if let Some(rp) = reply {
                req = req.reply_parameters(rp.clone());
            }
            if let Some(kb) = markup {
                req = req.reply_markup(kb.clone());
            }
            Ok(req.await?)
        }
        MediaKind::Video => {
            let mut req = bot.send_video(chat, file);
            if let Some(c) = caption {
                req = req.caption(c.to_string());
            }
            if spoiler {
                req = req.has_spoiler(true);
            }
            if let Some(rp) = reply {
                req = req.reply_parameters(rp.clone());
            }
            if let Some(info) = &item.probe {
                if let Some(d) = info.duration {
                    req = req.duration(d.as_secs() as u32);
                }
                if let (Some(w), Some(h)) = (info.width, info.height) {
                    req = req.width(w).height(h);
                }
            }
            if let Some(thumb) = &item.thumbnail {
                req = req.thumbnail(InputFile::file(thumb.clone()));
            }
            req = req.supports_streaming(true);
            if let Some(kb) = markup {
                req = req.reply_markup(kb.clone());
            }
            Ok(req.await?)
        }
        MediaKind::Animation => {
            let mut req = bot.send_animation(chat, file);
            if let Some(c) = caption {
                req = req.caption(c.to_string());
            }
            if spoiler {
                req = req.has_spoiler(true);
            }
            if let Some(rp) = reply {
                req = req.reply_parameters(rp.clone());
            }
            if let Some(kb) = markup {
                req = req.reply_markup(kb.clone());
            }
            Ok(req.await?)
        }
        MediaKind::Audio => {
            let mut req = bot.send_audio(chat, file);
            if let Some(c) = caption {
                req = req.caption(c.to_string());
            }
            if let Some(rp) = reply {
                req = req.reply_parameters(rp.clone());
            }
            if let Some(thumb) = &item.thumbnail {
                req = req.thumbnail(InputFile::file(thumb.clone()));
            }
            if let Some(kb) = markup {
                req = req.reply_markup(kb.clone());
            }
            Ok(req.await?)
        }
        MediaKind::Document => {
            let mut req = bot.send_document(chat, file);
            if let Some(c) = caption {
                req = req.caption(c.to_string());
            }
            if let Some(rp) = reply {
                req = req.reply_parameters(rp.clone());
            }
            if let Some(kb) = markup {
                req = req.reply_markup(kb.clone());
            }
            Ok(req.await?)
        }
    }
}

fn to_input_media(item: &PreparedMedia, caption: Option<&str>, spoiler: bool) -> InputMedia {
    let file = local_file(&item.path, item.filename.as_deref());
    match item.kind {
        MediaKind::Photo => {
            let mut m = InputMediaPhoto::new(file);
            if let Some(c) = caption {
                m = m.caption(c.to_string());
            }
            if spoiler {
                m = m.spoiler();
            }
            InputMedia::Photo(m)
        }
        MediaKind::Video => {
            let mut m = InputMediaVideo::new(file).supports_streaming(true);
            if let Some(c) = caption {
                m = m.caption(c.to_string());
            }
            if spoiler {
                m = m.spoiler();
            }
            if let Some(thumb) = &item.thumbnail {
                m = m.thumbnail(InputFile::file(thumb.clone()));
            }
            if let Some(info) = &item.probe {
                if let Some(d) = info.duration {
                    m = m.duration(d.as_secs().min(u16::MAX as u64) as u16);
                }
                if let (Some(w), Some(h)) = (info.width, info.height) {
                    m = m.width(to_u16(w)).height(to_u16(h));
                }
            }
            InputMedia::Video(m)
        }
        MediaKind::Audio => {
            let mut m = InputMediaAudio::new(file);
            if let Some(c) = caption {
                m = m.caption(c.to_string());
            }
            InputMedia::Audio(m)
        }
        MediaKind::Document => {
            let mut m = InputMediaDocument::new(file);
            if let Some(c) = caption {
                m = m.caption(c.to_string());
            }
            InputMedia::Document(m)
        }
        MediaKind::Animation => unreachable!("animation cannot join an album"),
    }
}

async fn send_album_local(
    bot: &TgBot,
    chat: ChatId,
    reply: Option<&ReplyParameters>,
    items: &[&PreparedMedia],
    captions: &[Option<String>],
    spoiler: bool,
) -> anyhow::Result<Vec<Message>> {
    // Pixiv_bot sendMediaGroup: every InputMedia carries its own caption + parse_mode.
    // Telegram album preview still highlights the first, but opening each photo shows 2/12, 3/12, …
    let media: Vec<InputMedia> = items
        .iter()
        .enumerate()
        .map(|(i, item)| to_input_media(item, captions.get(i).and_then(|c| c.as_deref()), spoiler))
        .collect();
    let mut req = bot.send_media_group(chat, media);
    if let Some(rp) = reply {
        req = req.reply_parameters(rp.clone());
    }
    Ok(req.await?)
}

fn media_spoiler(options: &OptionValues, nsfw: bool) -> bool {
    match options.get("sp") {
        Some(OptionValue::Flag(true)) => true,
        Some(OptionValue::Flag(false)) => false,
        _ => nsfw,
    }
}

fn caption_for_index(extracted: &Extracted, index: usize, total: usize) -> Option<String> {
    let rendered = render::build_caption_page(extracted, CaptionPage { index, total });
    if rendered.caption.is_empty() {
        None
    } else {
        Some(rendered.caption)
    }
}

fn single_keyboard(extracted: &Extracted, send_total: usize) -> Option<InlineKeyboardMarkup> {
    (send_total == 1)
        .then(|| render::media_keyboard(extracted))
        .flatten()
}

async fn send_cached(
    bot: &TgBot,
    chat: ChatId,
    reply: Option<&ReplyParameters>,
    hits: &[MediaCacheEntry],
    extracted: &Extracted,
    spoiler: bool,
    album_max: usize,
) -> anyhow::Result<usize> {
    let total = hits.len();
    let kinds: Vec<MediaKind> = hits.iter().map(|h| h.kind).collect();
    let plans = album::plan_groups_with_max(&kinds, album_max);
    let group_count = plans.len();
    for plan in plans {
        match plan {
            SendPlan::Single(i) => {
                let hit = &hits[i];
                let cap = caption_for_index(extracted, i, total);
                let markup = single_keyboard(extracted, total);
                send_single_cached(
                    bot,
                    chat,
                    reply,
                    hit,
                    cap.as_deref(),
                    spoiler,
                    markup.as_ref(),
                )
                .await?;
            }
            SendPlan::Album(idxs) => {
                let captions: Vec<Option<String>> = idxs
                    .iter()
                    .map(|&i| caption_for_index(extracted, i, total))
                    .collect();
                let media: Vec<InputMedia> = idxs
                    .iter()
                    .enumerate()
                    .map(|(j, &i)| {
                        cached_input_media(
                            &hits[i],
                            captions.get(j).and_then(|c| c.as_deref()),
                            spoiler,
                        )
                    })
                    .collect();
                let mut req = bot.send_media_group(chat, media);
                if let Some(rp) = reply {
                    req = req.reply_parameters(rp.clone());
                }
                req.await?;
            }
        }
    }
    Ok(group_count)
}

fn cached_input_media(hit: &MediaCacheEntry, caption: Option<&str>, spoiler: bool) -> InputMedia {
    let file = InputFile::file_id(FileId(hit.file_id.clone()));
    match hit.kind {
        MediaKind::Photo => {
            let mut m = InputMediaPhoto::new(file);
            if let Some(c) = caption {
                m = m.caption(c.to_string());
            }
            if spoiler {
                m = m.spoiler();
            }
            InputMedia::Photo(m)
        }
        MediaKind::Video => {
            let mut m = InputMediaVideo::new(file).supports_streaming(true);
            if let Some(c) = caption {
                m = m.caption(c.to_string());
            }
            if spoiler {
                m = m.spoiler();
            }
            InputMedia::Video(m)
        }
        MediaKind::Audio => {
            let mut m = InputMediaAudio::new(file);
            if let Some(c) = caption {
                m = m.caption(c.to_string());
            }
            InputMedia::Audio(m)
        }
        MediaKind::Document => {
            let mut m = InputMediaDocument::new(file);
            if let Some(c) = caption {
                m = m.caption(c.to_string());
            }
            InputMedia::Document(m)
        }
        MediaKind::Animation => unreachable!("animation cannot join an album"),
    }
}

async fn send_single_cached(
    bot: &TgBot,
    chat: ChatId,
    reply: Option<&ReplyParameters>,
    hit: &MediaCacheEntry,
    caption: Option<&str>,
    spoiler: bool,
    markup: Option<&InlineKeyboardMarkup>,
) -> anyhow::Result<Message> {
    let file = InputFile::file_id(FileId(hit.file_id.clone()));
    match hit.kind {
        MediaKind::Photo => {
            let mut req = bot.send_photo(chat, file);
            if let Some(c) = caption {
                req = req.caption(c.to_string());
            }
            if spoiler {
                req = req.has_spoiler(true);
            }
            if let Some(rp) = reply {
                req = req.reply_parameters(rp.clone());
            }
            if let Some(kb) = markup {
                req = req.reply_markup(kb.clone());
            }
            Ok(req.await?)
        }
        MediaKind::Video => {
            let mut req = bot.send_video(chat, file);
            if let Some(c) = caption {
                req = req.caption(c.to_string());
            }
            if spoiler {
                req = req.has_spoiler(true);
            }
            if let Some(rp) = reply {
                req = req.reply_parameters(rp.clone());
            }
            if let Some(kb) = markup {
                req = req.reply_markup(kb.clone());
            }
            Ok(req.await?)
        }
        MediaKind::Animation => {
            let mut req = bot.send_animation(chat, file);
            if let Some(c) = caption {
                req = req.caption(c.to_string());
            }
            if spoiler {
                req = req.has_spoiler(true);
            }
            if let Some(rp) = reply {
                req = req.reply_parameters(rp.clone());
            }
            if let Some(kb) = markup {
                req = req.reply_markup(kb.clone());
            }
            Ok(req.await?)
        }
        MediaKind::Audio => {
            let mut req = bot.send_audio(chat, file);
            if let Some(c) = caption {
                req = req.caption(c.to_string());
            }
            if let Some(rp) = reply {
                req = req.reply_parameters(rp.clone());
            }
            if let Some(kb) = markup {
                req = req.reply_markup(kb.clone());
            }
            Ok(req.await?)
        }
        MediaKind::Document => {
            let mut req = bot.send_document(chat, file);
            if let Some(c) = caption {
                req = req.caption(c.to_string());
            }
            if let Some(rp) = reply {
                req = req.reply_parameters(rp.clone());
            }
            if let Some(kb) = markup {
                req = req.reply_markup(kb.clone());
            }
            Ok(req.await?)
        }
    }
}

async fn send_html(
    bot: &TgBot,
    chat: ChatId,
    reply: Option<&ReplyParameters>,
    text: &str,
) -> anyhow::Result<()> {
    let mut req = bot.send_message(chat, text);
    if let Some(rp) = reply {
        req = req.reply_parameters(rp.clone());
    }
    req.await?;
    Ok(())
}

async fn send_extras(
    bot: &TgBot,
    chat: ChatId,
    reply: Option<&ReplyParameters>,
    rendered: &RenderedText,
) -> anyhow::Result<()> {
    for extra in &rendered.extras {
        send_html(bot, chat, reply, extra).await?;
    }
    Ok(())
}

async fn deliver_overflow(
    app: &App,
    chat_id: i64,
    chat: ChatId,
    reply: Option<&ReplyParameters>,
    total_items: usize,
    group_count: usize,
    skipped: &[String],
) -> anyhow::Result<()> {
    if let Some(text) = app
        .overflow
        .deliver(&OverflowCtx {
            chat_id,
            total_items,
            group_count,
            skipped,
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
    {
        send_html(&app.bot, chat, reply, &html::escape(&text)).await?;
    }
    Ok(())
}

fn upload_action(kind: MediaKind) -> ChatAction {
    match kind {
        MediaKind::Photo => ChatAction::UploadPhoto,
        MediaKind::Video | MediaKind::Animation => ChatAction::UploadVideo,
        MediaKind::Audio => ChatAction::UploadVoice,
        MediaKind::Document => ChatAction::UploadDocument,
    }
}

struct Presence {
    bot: TgBot,
    chat: ChatId,
    message_id: Option<MessageId>,
    interval: Duration,
    refresh: Option<CancellationToken>,
}

impl Presence {
    fn new(bot: TgBot, chat: ChatId, message_id: i32, interval: Duration) -> Self {
        Self {
            bot,
            chat,
            message_id: (message_id != 0).then_some(MessageId(message_id)),
            interval,
            refresh: None,
        }
    }

    async fn set_action(&mut self, action: ChatAction) {
        self.stop_refresh();
        let _ = self.bot.send_chat_action(self.chat, action).await;
        let cancel = CancellationToken::new();
        let child = cancel.clone();
        let bot = self.bot.clone();
        let chat = self.chat;
        let interval = self.interval;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = child.cancelled() => break,
                    _ = tokio::time::sleep(interval) => {
                        let _ = bot.send_chat_action(chat, action).await;
                    }
                }
            }
        });
        self.refresh = Some(cancel);
    }

    async fn succeed(&mut self) {
        self.stop_refresh();
        self.set_reaction(&[]).await;
    }

    async fn fail(&mut self, text: &str) {
        self.stop_refresh();
        self.reply(text).await;
        self.set_reaction(&[ReactionType::Emoji {
            emoji: "😢".into()
        }])
        .await;
    }

    fn stop_refresh(&mut self) {
        if let Some(cancel) = self.refresh.take() {
            cancel.cancel();
        }
    }

    async fn reply(&self, text: &str) {
        let mut req = self.bot.send_message(self.chat, text);
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

fn cache_entry_from_message(
    source_key: &str,
    idx: i32,
    kind: MediaKind,
    msg: &Message,
) -> Option<MediaCacheEntry> {
    let (file_id, file_unique_id, size) = file_ids_from_message(msg)?;
    Some(MediaCacheEntry {
        source_key: source_key.to_string(),
        idx,
        file_id: file_id.to_string(),
        file_unique_id: file_unique_id.to_string(),
        kind,
        size: size as i64,
    })
}

fn file_ids_from_message(msg: &Message) -> Option<(FileId, FileUniqueId, u32)> {
    if let Some(photo) = msg.photo().and_then(|p| p.last()) {
        return Some((
            photo.file.id.clone(),
            photo.file.unique_id.clone(),
            photo.file.size,
        ));
    }
    if let Some(v) = msg.video() {
        return Some((v.file.id.clone(), v.file.unique_id.clone(), v.file.size));
    }
    if let Some(a) = msg.animation() {
        return Some((a.file.id.clone(), a.file.unique_id.clone(), a.file.size));
    }
    if let Some(a) = msg.audio() {
        return Some((a.file.id.clone(), a.file.unique_id.clone(), a.file.size));
    }
    let d = msg.document()?;
    Some((d.file.id.clone(), d.file.unique_id.clone(), d.file.size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r18_spoilers_by_default() {
        let empty = OptionValues::new();
        assert!(media_spoiler(&empty, true));
        assert!(!media_spoiler(&empty, false));
    }

    #[test]
    fn plus_sp_spoilers_sfw() {
        let mut v = OptionValues::new();
        v.insert("sp", OptionValue::Flag(true));
        assert!(media_spoiler(&v, false));
        assert!(media_spoiler(&v, true));
    }

    #[test]
    fn minus_sp_disables_r18() {
        let mut v = OptionValues::new();
        v.insert("sp", OptionValue::Flag(false));
        assert!(!media_spoiler(&v, true));
        assert!(!media_spoiler(&v, false));
    }
}
