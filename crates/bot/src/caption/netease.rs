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
    use feedbot_core::{Extracted, MediaItem, MediaKind, MediaSource};
    use http::HeaderMap;
    use url::Url;

    fn sample(title: Option<&str>, desc: Option<&str>) -> Extracted {
        Extracted {
            extractor_id: "netease".into(),
            canonical_url: Url::parse("https://music.163.com/#/song?id=65").unwrap(),
            title: title.map(str::to_string),
            author: Some("A".into()),
            author_url: None,
            description: desc.map(str::to_string),
            tags: Vec::new(),
            nsfw: false,
            ai: false,
            engagement: None,
            items: vec![MediaItem {
                id: "s".into(),
                kind: MediaKind::Audio,
                source: MediaSource::Direct {
                    url: Url::parse("https://example.com/a.mp3").unwrap(),
                    headers: HeaderMap::new(),
                },
                filename: None,
                cover: None,
            }],
        }
    }

    #[test]
    fn long_description_notes_clip_and_does_not_send_extras() {
        let rendered = build_caption(&sample(Some("song"), Some(&"x".repeat(2000))));
        assert!(rendered.caption.contains("<blockquote expandable>"));
        assert!(rendered.caption.contains("简介过长"));
        assert!(rendered.extras.is_empty());
        let kb = link_keyboard(&sample(None, None)).unwrap();
        assert_eq!(kb.inline_keyboard[0][0].text, "原片");
    }
}
