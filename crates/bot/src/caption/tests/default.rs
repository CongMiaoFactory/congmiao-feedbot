//! Default layout (sites without their own).

use super::build_caption;
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
    assert!(
        rendered
            .caption
            .contains("<blockquote expandable>hello</blockquote>")
    );
}
