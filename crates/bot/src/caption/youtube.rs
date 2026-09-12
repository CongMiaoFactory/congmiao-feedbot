use feedbot_core::Extracted;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use super::blocks::{Block, Span};
use super::stats;
use super::CaptionPage;

pub fn blocks(extracted: &Extracted, _page: CaptionPage) -> Vec<Block> {
    let mut out = Vec::new();
    if let Some(line) = header_line(extracted) {
        out.push(Block::Line(line));
    }
    if let Some(stats_text) = stats::stats_line(extracted.engagement.as_ref()) {
        out.push(Block::Line(vec![Span::plain(stats_text)]));
    }
    if let Some(desc) = extracted
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push(Block::ExpandableClip(desc.to_string()));
    }
    out
}

pub fn link_keyboard(extracted: &Extracted) -> Option<InlineKeyboardMarkup> {
    let mut row = Vec::new();
    if let Some(url) = extracted.author_url.as_ref().and_then(tg_url) {
        row.push(InlineKeyboardButton::url("作者", url));
    }
    if let Some(url) = tg_url(&extracted.canonical_url) {
        row.push(InlineKeyboardButton::url("原片", url));
    }
    if row.is_empty() {
        None
    } else {
        Some(InlineKeyboardMarkup::new(vec![row]))
    }
}

fn tg_url(url: &url::Url) -> Option<reqwest::Url> {
    reqwest::Url::parse(url.as_str()).ok()
}

fn header_line(extracted: &Extracted) -> Option<Vec<Span>> {
    let mut spans = Vec::new();
    if extracted.nsfw {
        spans.push(Span::plain("#NSFW"));
        spans.push(Span::atom(" "));
    }
    if let Some(title) = extracted
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        spans.push(Span::plain(title.to_string()));
    }
    if spans.is_empty() {
        None
    } else {
        Some(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caption::build_caption;
    use feedbot_core::{Engagement, Extracted, MediaItem, MediaKind, MediaSource};
    use http::HeaderMap;
    use url::Url;

    fn sample(
        title: Option<&str>,
        desc: Option<&str>,
        nsfw: bool,
        engagement: Option<Engagement>,
        with_author_url: bool,
    ) -> Extracted {
        Extracted {
            extractor_id: "youtube".into(),
            canonical_url: Url::parse("https://youtube.com/watch?v=jNQXAC9IVRw").unwrap(),
            title: title.map(str::to_string),
            author: Some("bob".into()),
            author_url: with_author_url.then(|| Url::parse("https://youtube.com/@bob").unwrap()),
            description: desc.map(str::to_string),
            tags: Vec::new(),
            nsfw,
            ai: false,
            engagement,
            items: vec![MediaItem {
                id: "v".into(),
                kind: MediaKind::Video,
                source: MediaSource::Direct {
                    url: Url::parse("https://example.com/v.mp4").unwrap(),
                    headers: HeaderMap::new(),
                },
                filename: None,
                cover: None,
            }],
        }
    }

    #[test]
    fn caption_is_title_stats_expandable_without_links() {
        let rendered = build_caption(&sample(
            Some("Hello <T>"),
            Some("desc"),
            true,
            Some(Engagement {
                views: Some(12_345),
                likes: Some(320),
                shares: None,
                comments: Some(4),
            }),
            true,
        ));
        assert!(rendered.caption.starts_with("#NSFW Hello &lt;T&gt;"));
        assert!(rendered.caption.contains("浏览 1.2万 · 点赞 320 · 评论 4"));
        assert!(rendered
            .caption
            .contains("<blockquote expandable>desc</blockquote>"));
        assert!(!rendered.caption.contains("<a href="));
        assert!(!rendered.caption.contains(" / bob"));
    }

    #[test]
    fn long_description_notes_clip_and_does_not_send_extras() {
        let rendered = build_caption(&sample(
            Some("song"),
            Some(&"x".repeat(2000)),
            false,
            None,
            false,
        ));
        assert!(rendered.caption.contains("<blockquote expandable>"));
        assert!(rendered.caption.contains("简介过长"));
        assert!(rendered.extras.is_empty());
        let short = build_caption(&sample(Some("song"), Some("short"), false, None, false));
        assert!(short
            .caption
            .contains("<blockquote expandable>short</blockquote>"));
        assert!(!short.caption.contains("简介过长"));
        assert!(short.extras.is_empty());
    }

    #[test]
    fn keyboard_author_and_video() {
        let both = link_keyboard(&sample(None, None, false, None, true)).unwrap();
        assert_eq!(both.inline_keyboard[0].len(), 2);
        assert_eq!(both.inline_keyboard[0][0].text, "作者");
        assert_eq!(both.inline_keyboard[0][1].text, "原片");
        let only = link_keyboard(&sample(None, None, false, None, false)).unwrap();
        assert_eq!(only.inline_keyboard[0][0].text, "原片");
    }
}
