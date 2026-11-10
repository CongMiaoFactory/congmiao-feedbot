//! Caption layouts, independent of the chat platform. A site picks a
//! [`Present`] variant through [`crate::Extractor::present`]; only a layout
//! that fits none of them implements [`Caption`]. The bot renders the blocks
//! (HTML, UTF-16 limits) and turns actions into buttons.

use crate::{Engagement, Extracted};
use url::Url;

/// 0-based index of the first item in a send group, plus how many items are being sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptionPage {
    pub index: usize,
    pub total: usize,
}

impl CaptionPage {
    pub fn single(total: usize) -> Self {
        Self { index: 0, total }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    /// Visible text that may be truncated to fit the caption budget.
    Plain(String),
    /// Visible text that is kept only if the whole string fits.
    Atom(String),
    Link {
        href: String,
        text: String,
    },
}

impl Span {
    pub fn plain(s: impl Into<String>) -> Self {
        Self::Plain(s.into())
    }

    pub fn atom(s: impl Into<String>) -> Self {
        Self::Atom(s.into())
    }

    pub fn link(href: impl Into<String>, text: impl Into<String>) -> Self {
        Self::Link {
            href: href.into(),
            text: text.into(),
        }
    }

    pub fn visible(&self) -> &str {
        match self {
            Self::Plain(s) | Self::Atom(s) => s,
            Self::Link { text, .. } => text,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Line(Vec<Span>),
    Expandable(String),
    /// Like [`Self::Expandable`], but leftover text is dropped and the caption
    /// gets a `简介过长` note instead of a follow-up message.
    ExpandableClip(String),
}

impl Block {
    pub fn plain_text(&self) -> String {
        match self {
            Self::Line(spans) => spans.iter().map(Span::visible).collect::<Vec<_>>().join(""),
            Self::Expandable(s) | Self::ExpandableClip(s) => s.clone(),
        }
    }
}

/// A link button. The chat platform turns this into its own keyboard widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaAction {
    Url { label: &'static str, href: Url },
}

/// Layout selected by a site. [`Present::Custom`] is the escape hatch.
#[derive(Clone, Copy)]
pub enum Present {
    /// Title link, author, expandable description.
    Default,
    /// NSFW/AI badges, title link, page index, tags, expandable description.
    Gallery,
    /// Stats, expandable body, and an origin button such as `原帖`.
    Post {
        origin: &'static str,
    },
    /// Title, stats, clipped description, and an origin button such as `原片`.
    Clip {
        origin: &'static str,
    },
    Custom(&'static dyn Caption),
}

pub trait Caption: Send + Sync {
    fn blocks(&self, extracted: &Extracted, page: CaptionPage) -> Vec<Block>;
    fn actions(&self, extracted: &Extracted) -> Vec<MediaAction> {
        let _ = extracted;
        Vec::new()
    }
}

pub fn blocks(style: Present, extracted: &Extracted, page: CaptionPage) -> Vec<Block> {
    match style {
        Present::Default => default_blocks(extracted),
        Present::Gallery => gallery_blocks(extracted, page),
        Present::Post { .. } => post_blocks(extracted, page),
        Present::Clip { .. } => clip_blocks(extracted),
        Present::Custom(caption) => caption.blocks(extracted, page),
    }
}

pub fn actions(style: Present, extracted: &Extracted) -> Vec<MediaAction> {
    match style {
        Present::Post { origin } | Present::Clip { origin } => link_actions(extracted, origin),
        Present::Custom(caption) => caption.actions(extracted),
        Present::Default | Present::Gallery => Vec::new(),
    }
}

fn default_blocks(extracted: &Extracted) -> Vec<Block> {
    let url = extracted.canonical_url.as_str();
    let title = extracted
        .title
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(url);
    let mut spans = vec![Span::link(url, title)];
    if let Some(author) = extracted.author.as_deref().filter(|s| !s.is_empty()) {
        spans.push(Span::atom(" / "));
        match extracted.author_url.as_ref() {
            Some(author_url) => spans.push(Span::link(author_url.as_str(), author)),
            None => spans.push(Span::plain(author)),
        }
    }
    let mut out = vec![Block::Line(spans)];
    if let Some(desc) = trimmed(&extracted.description) {
        out.push(Block::Expandable(desc.to_string()));
    }
    out
}

fn gallery_blocks(extracted: &Extracted, page: CaptionPage) -> Vec<Block> {
    let mut spans = Vec::new();
    let mut badges = Vec::new();
    if extracted.nsfw {
        badges.push("#NSFW");
    }
    if extracted.ai {
        badges.push("#AI");
    }
    if !badges.is_empty() {
        spans.push(Span::plain(badges.join(" ")));
        spans.push(Span::atom(" "));
    }

    let url = extracted.canonical_url.as_str();
    let title = extracted
        .title
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(url);
    spans.push(Span::link(url, title));

    if let Some(author) = extracted.author.as_deref().filter(|s| !s.is_empty()) {
        spans.push(Span::atom(" / "));
        match extracted.author_url.as_ref() {
            Some(author_url) => spans.push(Span::link(author_url.as_str(), author)),
            None => spans.push(Span::plain(author)),
        }
    }

    if page.total > 1 {
        spans.push(Span::atom(format!(" {}/{}", page.index + 1, page.total)));
    }

    let mut out = vec![Block::Line(spans)];
    if let Some(tags) = tag_line(&extracted.tags) {
        out.push(Block::Expandable(tags));
    }
    if let Some(desc) = trimmed(&extracted.description) {
        out.push(Block::Expandable(desc.to_string()));
    }
    out
}

fn post_blocks(extracted: &Extracted, page: CaptionPage) -> Vec<Block> {
    let mut out = Vec::new();
    if let Some(line) = post_header(extracted, page) {
        out.push(Block::Line(line));
    }
    if let Some(stats_text) = stats_line(extracted.engagement.as_ref()) {
        out.push(Block::Line(vec![Span::plain(stats_text)]));
    }
    if let Some(desc) = trimmed(&extracted.description) {
        out.push(Block::Expandable(desc.to_string()));
    }
    out
}

fn clip_blocks(extracted: &Extracted) -> Vec<Block> {
    let mut out = Vec::new();
    if let Some(line) = clip_header(extracted) {
        out.push(Block::Line(line));
    }
    if let Some(stats_text) = stats_line(extracted.engagement.as_ref()) {
        out.push(Block::Line(vec![Span::plain(stats_text)]));
    }
    if let Some(desc) = trimmed(&extracted.description) {
        out.push(Block::ExpandableClip(desc.to_string()));
    }
    out
}

fn post_header(extracted: &Extracted, page: CaptionPage) -> Option<Vec<Span>> {
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
    if spans.is_empty() { None } else { Some(spans) }
}

fn clip_header(extracted: &Extracted) -> Option<Vec<Span>> {
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
    if spans.is_empty() { None } else { Some(spans) }
}

fn link_actions(extracted: &Extracted, origin: &'static str) -> Vec<MediaAction> {
    let mut row = Vec::new();
    if let Some(url) = extracted.author_url.as_ref() {
        row.push(MediaAction::Url {
            label: "作者",
            href: url.clone(),
        });
    }
    row.push(MediaAction::Url {
        label: origin,
        href: extracted.canonical_url.clone(),
    });
    row
}

fn tag_line(tags: &[String]) -> Option<String> {
    let line = tags
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| format!("#{t}"))
        .collect::<Vec<_>>()
        .join(" ");
    if line.is_empty() { None } else { Some(line) }
}

fn trimmed(text: &Option<String>) -> Option<&str> {
    text.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

pub fn stats_line(engagement: Option<&Engagement>) -> Option<String> {
    let e = engagement?;
    let mut parts = Vec::new();
    if let Some(n) = e.views {
        parts.push(format!("浏览 {}", format_count(n)));
    }
    if let Some(n) = e.likes {
        parts.push(format!("点赞 {}", format_count(n)));
    }
    if let Some(n) = e.shares {
        parts.push(format!("分享 {}", format_count(n)));
    }
    if let Some(n) = e.comments {
        parts.push(format!("评论 {}", format_count(n)));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

pub fn format_count(n: u64) -> String {
    if n >= 100_000_000 {
        one_decimal(n as f64 / 100_000_000.0, "亿")
    } else if n >= 10_000 {
        one_decimal(n as f64 / 10_000.0, "万")
    } else {
        n.to_string()
    }
}

fn one_decimal(v: f64, unit: &str) -> String {
    let s = format!("{v:.1}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}{unit}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MediaItem, MediaKind, MediaSource};
    use http::HeaderMap;

    fn item() -> MediaItem {
        MediaItem {
            id: "p".into(),
            kind: MediaKind::Photo,
            source: MediaSource::Direct {
                url: Url::parse("https://example.com/p.png").unwrap(),
                headers: HeaderMap::new(),
            },
            filename: None,
            cover: None,
        }
    }

    fn extracted() -> Extracted {
        Extracted {
            extractor_id: "pixiv".into(),
            canonical_url: Url::parse("https://example.com/a").unwrap(),
            title: Some("T".into()),
            author: Some("bob".into()),
            author_url: Some(Url::parse("https://example.com/u/1").unwrap()),
            description: Some("hello".into()),
            tags: vec!["cat".into()],
            nsfw: true,
            ai: true,
            engagement: Some(Engagement {
                views: Some(12_345),
                likes: Some(320),
                shares: Some(18),
                comments: Some(41),
            }),
            items: vec![item(), item()],
        }
    }

    #[test]
    fn gallery_keeps_badges_tags_and_page() {
        let page = CaptionPage {
            index: 1,
            total: 12,
        };
        let blocks = blocks(Present::Gallery, &extracted(), page);
        assert!(blocks[0].plain_text().starts_with("#NSFW #AI "));
        assert!(blocks[0].plain_text().contains("T"));
        assert!(blocks[0].plain_text().contains(" / bob"));
        assert!(blocks[0].plain_text().contains(" 2/12"));
        assert!(matches!(blocks[1], Block::Expandable(ref s) if s == "#cat"));
        assert!(matches!(blocks[2], Block::Expandable(ref s) if s == "hello"));
        assert!(actions(Present::Gallery, &extracted()).is_empty());
    }

    #[test]
    fn post_skips_title_and_uses_origin_button() {
        let blocks = blocks(
            Present::Post { origin: "原帖" },
            &extracted(),
            CaptionPage::single(1),
        );
        let text = blocks
            .iter()
            .map(Block::plain_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("#NSFW"));
        assert!(text.contains("浏览 1.2万 · 点赞 320 · 分享 18 · 评论 41"));
        assert!(!text.contains(" / bob"));
        assert!(matches!(blocks.last(), Some(Block::Expandable(_))));
        let actions = actions(Present::Post { origin: "原帖" }, &extracted());
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[1],
            MediaAction::Url {
                label: "原帖", ..
            }
        ));
    }

    #[test]
    fn clip_uses_clipped_description_and_origin_button() {
        let blocks = blocks(
            Present::Clip { origin: "原片" },
            &extracted(),
            CaptionPage::single(1),
        );
        assert!(matches!(blocks.last(), Some(Block::ExpandableClip(_))));
        let actions = actions(Present::Clip { origin: "原片" }, &extracted());
        assert!(matches!(
            actions.last(),
            Some(MediaAction::Url {
                label: "原片", ..
            })
        ));
    }

    #[test]
    fn default_is_title_author_description() {
        let blocks = blocks(Present::Default, &extracted(), CaptionPage::single(4));
        assert!(matches!(blocks[0], Block::Line(_)));
        assert!(!blocks[0].plain_text().contains("#NSFW"));
        assert!(!blocks[0].plain_text().contains("1/4"));
        assert!(matches!(blocks[1], Block::Expandable(_)));
    }

    #[test]
    fn format_count_uses_wan_and_yi() {
        assert_eq!(format_count(320), "320");
        assert_eq!(format_count(10_000), "1万");
        assert_eq!(format_count(12_345), "1.2万");
        assert_eq!(format_count(100_000_000), "1亿");
    }

    #[test]
    fn stats_omit_missing_and_do_not_invent_zero() {
        let stats = |views, likes| {
            stats_line(Some(&Engagement {
                views,
                likes,
                shares: None,
                comments: None,
            }))
        };
        assert_eq!(
            stats(Some(12_345), Some(320)),
            Some("浏览 1.2万 · 点赞 320".into())
        );
        assert_eq!(stats(None, Some(7)), Some("点赞 7".into()));
        assert!(stats_line(None).is_none());
        assert!(stats_line(Some(&Engagement::default())).is_none());
    }
}
