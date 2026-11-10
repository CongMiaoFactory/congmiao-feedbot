//! Sending prepared or cached media to Telegram as albums / single messages.

use super::prepare::PreparedMedia;
use crate::album::{self, SendPlan};
use crate::app::{App, TgBot};
use crate::caption::{self, RenderedText};
use crate::publisher::OverflowCtx;
use feedbot_core::MediaInfo;
use feedbot_core::extract::{Extracted, MediaKind};
use feedbot_core::job::MediaCacheEntry;
use feedbot_core::{CaptionPage, Present};
use std::path::PathBuf;
use teloxide::payloads::{
    SendAnimationSetters, SendAudioSetters, SendDocumentSetters, SendMediaGroupSetters,
    SendMessageSetters, SendPhotoSetters, SendVideoSetters,
};
use teloxide::prelude::*;
use teloxide::types::{
    ChatId, FileId, InlineKeyboardMarkup, InputFile, InputMedia, InputMediaAudio,
    InputMediaDocument, InputMediaPhoto, InputMediaVideo, LinkPreviewOptions, ReplyParameters,
};
use teloxide::utils::html;
use url::Url;

/// Where a job's output goes.
pub(super) struct Target<'a> {
    pub(super) bot: &'a TgBot,
    pub(super) chat: ChatId,
    pub(super) reply: Option<ReplyParameters>,
}

impl Target<'_> {
    /// An HTML message to this chat, replying to the job's message if any.
    fn message(&self, text: impl Into<String>) -> <TgBot as Requester>::SendMessage {
        let req = self.bot.send_message(self.chat, text);
        match &self.reply {
            Some(rp) => req.reply_parameters(rp.clone()),
            None => req,
        }
    }

    async fn send_html(&self, text: &str) -> anyhow::Result<()> {
        self.message(text).await?;
        Ok(())
    }
}

/// An extraction result and the caption layout its site chose.
#[derive(Clone, Copy)]
pub(super) struct Post<'a> {
    pub(super) present: Present,
    pub(super) extracted: &'a Extracted,
}

impl Post<'_> {
    fn caption(&self, page: CaptionPage) -> RenderedText {
        caption::build_caption_page(self.present, self.extracted, page)
    }
}

/// Caption overflow, text-only fallback, and the summary / skipped notice.
pub(super) async fn finish_delivery(
    app: &App,
    target: &Target<'_>,
    post: Post<'_>,
    send_total: usize,
    total_items: usize,
    mut group_count: usize,
    skipped: &[String],
) -> anyhow::Result<()> {
    let rendered = post.caption(CaptionPage::single(send_total));
    if send_total == 0 && !rendered.caption.is_empty() {
        target.send_html(&rendered.caption).await?;
        group_count = 1;
    }
    send_extras(target, &rendered).await?;
    let summary = app.pipeline.overflow.summary(&OverflowCtx {
        total_items,
        group_count,
        skipped,
    });
    if let Some(text) = summary {
        target.send_html(&html::escape(&text)).await?;
    }
    Ok(())
}

/// The whole result lives on an external page: send the caption plus a link,
/// previewed as the page. With spoilers on, the preview is suppressed.
pub(super) async fn send_page_link(
    target: &Target<'_>,
    post: Post<'_>,
    page: &Url,
    spoiler: bool,
) -> anyhow::Result<()> {
    let rendered = post.caption(CaptionPage::single(1));
    let link = format!(
        "<a href=\"{}\">在 Telegraph 查看全部 {} 项</a>",
        html::escape(page.as_str()),
        post.extracted.items.len()
    );
    let text = if rendered.caption.is_empty() {
        link
    } else {
        format!("{}\n\n{link}", rendered.caption)
    };
    target
        .message(text)
        .link_preview_options(LinkPreviewOptions {
            is_disabled: spoiler,
            url: Some(page.to_string()),
            prefer_small_media: false,
            prefer_large_media: true,
            show_above_text: false,
        })
        .await?;
    send_extras(target, &rendered).await
}

async fn send_extras(target: &Target<'_>, rendered: &RenderedText) -> anyhow::Result<()> {
    for extra in &rendered.extras {
        target.send_html(extra).await?;
    }
    Ok(())
}

/// A media item ready to send: either a fresh local file or a cached `file_id`.
pub(super) struct Outgoing {
    pub(super) kind: MediaKind,
    file: InputFile,
    probe: Option<MediaInfo>,
    thumbnail: Option<PathBuf>,
}

impl Outgoing {
    pub(super) fn local(item: &PreparedMedia) -> Self {
        let mut file = InputFile::file(item.path.clone());
        if let Some(name) = &item.filename {
            file = file.file_name(name.clone());
        }
        Self {
            kind: item.kind,
            file,
            probe: item.probe.clone(),
            thumbnail: item.thumbnail.clone(),
        }
    }

    pub(super) fn cached(hit: &MediaCacheEntry) -> Self {
        Self {
            kind: hit.kind,
            file: InputFile::file_id(FileId(hit.file_id.clone())),
            probe: None,
            thumbnail: None,
        }
    }
}

/// Send all items in album/single groups. Returns the group count and every
/// sent message with the position of the item it carries.
pub(super) async fn deliver(
    app: &App,
    target: &Target<'_>,
    post: Post<'_>,
    items: &[Outgoing],
    spoiler: bool,
) -> anyhow::Result<(usize, Vec<(usize, Message)>)> {
    let total = items.len();
    let kinds: Vec<MediaKind> = items.iter().map(|i| i.kind).collect();
    let plans = album::plan_groups_with_max(&kinds, app.pipeline.album_max);
    let group_count = plans.len();
    let mut sent = Vec::with_capacity(total);
    for plan in plans {
        match plan {
            SendPlan::Single(i) => {
                let cap = caption_for_index(post, i, total);
                let markup = (total == 1)
                    .then(|| caption::media_keyboard(post.present, post.extracted))
                    .flatten();
                let msg = send_single(target, &items[i], cap.as_deref(), spoiler, markup.as_ref())
                    .await?;
                sent.push((i, msg));
            }
            SendPlan::Album(idxs) => {
                // Every album entry carries its own caption, so opening any
                // photo shows its `n/total` position.
                let media: Vec<InputMedia> = idxs
                    .iter()
                    .map(|&i| {
                        input_media(
                            &items[i],
                            caption_for_index(post, i, total).as_deref(),
                            spoiler,
                        )
                    })
                    .collect();
                let mut req = target.bot.send_media_group(target.chat, media);
                if let Some(rp) = &target.reply {
                    req = req.reply_parameters(rp.clone());
                }
                let msgs = req.await?;
                sent.extend(idxs.into_iter().zip(msgs));
            }
        }
    }
    Ok((group_count, sent))
}

/// Caption, reply target and keyboard setters shared by every `send_*` request.
macro_rules! common {
    ($req:expr_2021, $target:expr_2021, $caption:expr_2021, $markup:expr_2021) => {{
        let mut req = $req;
        if let Some(c) = $caption {
            req = req.caption(c.to_string());
        }
        if let Some(rp) = &$target.reply {
            req = req.reply_parameters(rp.clone());
        }
        if let Some(kb) = $markup {
            req = req.reply_markup(kb.clone());
        }
        req
    }};
}

async fn send_single(
    target: &Target<'_>,
    item: &Outgoing,
    caption: Option<&str>,
    spoiler: bool,
    markup: Option<&InlineKeyboardMarkup>,
) -> anyhow::Result<Message> {
    let (bot, chat, file) = (target.bot, target.chat, item.file.clone());
    let msg = match item.kind {
        MediaKind::Photo => {
            common!(bot.send_photo(chat, file), target, caption, markup)
                .has_spoiler(spoiler)
                .await?
        }
        MediaKind::Video => {
            let mut req = common!(bot.send_video(chat, file), target, caption, markup)
                .has_spoiler(spoiler)
                .supports_streaming(true);
            if let Some(info) = &item.probe {
                if let Some(d) = info.duration {
                    req = req.duration(u32::try_from(d.as_secs()).unwrap_or(u32::MAX));
                }
                if let (Some(w), Some(h)) = (info.width, info.height) {
                    req = req.width(w).height(h);
                }
            }
            if let Some(thumb) = &item.thumbnail {
                req = req.thumbnail(InputFile::file(thumb.clone()));
            }
            req.await?
        }
        MediaKind::Animation => {
            common!(bot.send_animation(chat, file), target, caption, markup)
                .has_spoiler(spoiler)
                .await?
        }
        MediaKind::Audio => {
            let mut req = common!(bot.send_audio(chat, file), target, caption, markup);
            if let Some(thumb) = &item.thumbnail {
                req = req.thumbnail(InputFile::file(thumb.clone()));
            }
            req.await?
        }
        MediaKind::Document => {
            common!(bot.send_document(chat, file), target, caption, markup).await?
        }
    };
    Ok(msg)
}

fn input_media(item: &Outgoing, caption: Option<&str>, spoiler: bool) -> InputMedia {
    let file = item.file.clone();
    let caption = caption.map(str::to_string);
    match item.kind {
        MediaKind::Photo => {
            let mut m = InputMediaPhoto::new(file);
            m.caption = caption;
            m.has_spoiler = spoiler;
            InputMedia::Photo(m)
        }
        MediaKind::Video => {
            let mut m = InputMediaVideo::new(file).supports_streaming(true);
            m.caption = caption;
            m.has_spoiler = spoiler;
            if let Some(thumb) = &item.thumbnail {
                m = m.thumbnail(InputFile::file(thumb.clone()));
            }
            if let Some(info) = &item.probe {
                if let Some(d) = info.duration {
                    m = m.duration(u16::try_from(d.as_secs()).unwrap_or(u16::MAX));
                }
                if let (Some(w), Some(h)) = (info.width, info.height) {
                    m = m.width(to_u16(w)).height(to_u16(h));
                }
            }
            InputMedia::Video(m)
        }
        MediaKind::Audio => {
            let mut m = InputMediaAudio::new(file);
            m.caption = caption;
            InputMedia::Audio(m)
        }
        MediaKind::Document => {
            let mut m = InputMediaDocument::new(file);
            m.caption = caption;
            InputMedia::Document(m)
        }
        MediaKind::Animation => unreachable!("album planner never groups animations"),
    }
}

fn caption_for_index(post: Post<'_>, index: usize, total: usize) -> Option<String> {
    let rendered = post.caption(CaptionPage { index, total });
    (!rendered.caption.is_empty()).then_some(rendered.caption)
}

fn to_u16(n: u32) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

pub(super) fn cache_entry(
    source_key: &str,
    idx: usize,
    kind: MediaKind,
    msg: &Message,
) -> Option<MediaCacheEntry> {
    let file = if let Some(photo) = msg.photo().and_then(|p| p.last()) {
        &photo.file
    } else if let Some(v) = msg.video() {
        &v.file
    } else if let Some(a) = msg.animation() {
        &a.file
    } else if let Some(a) = msg.audio() {
        &a.file
    } else {
        &msg.document()?.file
    };
    Some(MediaCacheEntry {
        source_key: source_key.to_string(),
        idx: i32::try_from(idx).ok()?,
        file_id: file.id.to_string(),
        file_unique_id: file.unique_id.to_string(),
        kind,
        size: i64::from(file.size),
    })
}
