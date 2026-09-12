use feedbot_core::Extracted;

use super::blocks::{Block, Span};

/// Fallback layout for extractors that have not registered a caption theme.
pub fn blocks(extracted: &Extracted) -> Vec<Block> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caption::build_caption;
    use feedbot_core::{Extracted, MediaItem, MediaKind, MediaSource};
    use http::HeaderMap;
    use url::Url;

    fn sample(pages: usize) -> Extracted {
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
            extractor_id: "unknown".into(),
            canonical_url: Url::parse("https://example.com/a").unwrap(),
            title: Some("T".into()),
            author: Some("bob".into()),
            author_url: None,
            description: Some("hello".into()),
            tags: vec!["cat".into()],
            nsfw: true,
            ai: true,
            engagement: None,
            items,
        }
    }

    #[test]
    fn skips_pixiv_page_count_and_badges() {
        let rendered = build_caption(&sample(4));
        assert!(!rendered.caption.contains(" 4P"));
        assert!(!rendered.caption.contains(" 1/4"));
        assert!(!rendered.caption.contains("#NSFW"));
        assert!(!rendered.caption.contains("#AI"));
        assert!(!rendered.caption.contains("#cat"));
        assert!(rendered
            .caption
            .contains("<blockquote expandable>hello</blockquote>"));
    }

    #[test]
    fn default_blocks_are_title_author_description() {
        let blocks = blocks(&sample(1));
        assert!(matches!(blocks[0], Block::Line(_)));
        assert!(matches!(blocks[1], Block::Expandable(_)));
        assert_eq!(blocks.len(), 2);
    }
}
