use feedbot_core::Extracted;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use super::blocks::{Block, Span};
use super::stats;
use super::CaptionPage;

pub fn blocks(extracted: &Extracted, page: CaptionPage) -> Vec<Block> {
    let mut out = Vec::new();
    if let Some(line) = header_line(extracted, page) {
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
        out.push(Block::Expandable(desc.to_string()));
    }
    out
}

pub fn link_keyboard(extracted: &Extracted) -> Option<InlineKeyboardMarkup> {
    let mut row = Vec::new();
    if let Some(url) = extracted.author_url.as_ref().and_then(tg_url) {
        row.push(InlineKeyboardButton::url("作者", url));
    }
    if let Some(url) = tg_url(&extracted.canonical_url) {
        row.push(InlineKeyboardButton::url("原帖", url));
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

fn header_line(extracted: &Extracted, page: CaptionPage) -> Option<Vec<Span>> {
    let mut spans = Vec::new();
    if extracted.nsfw {
        spans.push(Span::plain("#NSFW"));
    }
    if page.total > 1 {
        if !spans.is_empty() {
            spans.push(Span::atom(" "));
        }
        spans.push(Span::atom(format!("{}/{}", page.index + 1, page.total)));
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
    use crate::caption::{build_caption, build_caption_page, CaptionPage};
    use feedbot_core::{Engagement, Extracted, MediaItem, MediaKind, MediaSource};
    use http::HeaderMap;
    use url::Url;

    fn sample(
        desc: Option<&str>,
        nsfw: bool,
        pages: usize,
        engagement: Option<Engagement>,
        with_author_url: bool,
    ) -> Extracted {
        let items = (0..pages)
            .map(|i| MediaItem {
                id: format!("p{i}"),
                kind: MediaKind::Photo,
                source: MediaSource::Direct {
                    url: Url::parse("https://example.com/p.png").unwrap(),
                    headers: HeaderMap::new(),
                },
                filename: None,
                cover: None,
            })
            .collect();
        Extracted {
            extractor_id: "x".into(),
            canonical_url: Url::parse("https://x.com/i/status/1").unwrap(),
            title: Some("ignored title".into()),
            author: Some("bob".into()),
            author_url: with_author_url.then(|| Url::parse("https://x.com/bob").unwrap()),
            description: desc.map(str::to_string),
            tags: Vec::new(),
            nsfw,
            ai: false,
            engagement,
            items,
        }
    }

    #[test]
    fn caption_is_stats_plus_expandable_without_title_or_author_links() {
        let rendered = build_caption(&sample(
            Some("hello\nworld"),
            true,
            1,
            Some(Engagement {
                views: Some(12_345),
                likes: Some(320),
                shares: Some(18),
                comments: Some(41),
            }),
            true,
        ));
        assert!(rendered.caption.starts_with("#NSFW\n"));
        assert!(rendered
            .caption
            .contains("浏览 1.2万 · 点赞 320 · 分享 18 · 评论 41"));
        assert!(rendered
            .caption
            .contains("<blockquote expandable>hello\nworld</blockquote>"));
        assert!(!rendered.caption.contains("<a href="));
        assert!(!rendered.caption.contains(" / bob"));
        assert!(!rendered.caption.contains("ignored title"));
        assert!(rendered.extras.is_empty());
    }

    #[test]
    fn album_pages_use_index_over_total() {
        let extracted = sample(Some("hi"), false, 4, None, false);
        let first = build_caption_page(&extracted, CaptionPage { index: 0, total: 4 });
        assert!(first.caption.contains("1/4"));
        assert!(!first.caption.contains("2/4"));
        let second = build_caption_page(&extracted, CaptionPage { index: 1, total: 4 });
        assert!(second.caption.contains("2/4"));
        assert!(!second.caption.contains("1/4"));
    }

    #[test]
    fn keyboard_has_author_and_post_or_post_only() {
        let both = link_keyboard(&sample(None, false, 1, None, true)).unwrap();
        let row = &both.inline_keyboard[0];
        assert_eq!(row.len(), 2);
        assert_eq!(row[0].text, "作者");
        assert_eq!(row[1].text, "原帖");

        let post_only = link_keyboard(&sample(None, false, 1, None, false)).unwrap();
        assert_eq!(post_only.inline_keyboard[0].len(), 1);
        assert_eq!(post_only.inline_keyboard[0][0].text, "原帖");
    }
}
