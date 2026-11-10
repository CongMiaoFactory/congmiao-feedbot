//! Clip layout (YouTube, Bilibili, NetEase).

use super::{build_caption, media_keyboard};
use feedbot_core::{Engagement, Extracted, MediaItem, MediaKind, MediaSource};
use http::HeaderMap;
use url::Url;

const SITES: [&str; 3] = ["youtube", "bilibili", "netease"];

fn sample(
    extractor_id: &str,
    title: Option<&str>,
    desc: Option<&str>,
    engagement: Option<Engagement>,
    with_author_url: bool,
) -> Extracted {
    Extracted {
        extractor_id: extractor_id.into(),
        canonical_url: Url::parse("https://example.com/post/1").unwrap(),
        title: title.map(str::to_string),
        author: Some("bob".into()),
        author_url: with_author_url.then(|| Url::parse("https://example.com/@bob").unwrap()),
        description: desc.map(str::to_string),
        tags: Vec::new(),
        nsfw: false,
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
    let mut extracted = sample(
        "youtube",
        Some("Hello <T>"),
        Some("desc"),
        Some(Engagement {
            views: Some(12_345),
            likes: Some(320),
            shares: None,
            comments: Some(4),
        }),
        true,
    );
    extracted.nsfw = true;
    let rendered = build_caption(&extracted);
    assert!(rendered.caption.starts_with("#NSFW Hello &lt;T&gt;"));
    assert!(rendered.caption.contains("浏览 1.2万 · 点赞 320 · 评论 4"));
    assert!(
        rendered
            .caption
            .contains("<blockquote expandable>desc</blockquote>")
    );
    assert!(!rendered.caption.contains("<a href="));
    assert!(!rendered.caption.contains(" / bob"));
}

#[test]
fn every_titled_site_clips_long_descriptions_in_place() {
    for site in SITES {
        let rendered = build_caption(&sample(
            site,
            Some("t"),
            Some(&"x".repeat(2000)),
            None,
            false,
        ));
        assert!(
            rendered.caption.contains("<blockquote expandable>"),
            "{site}"
        );
        assert!(rendered.caption.contains("简介过长"), "{site}");
        assert!(rendered.extras.is_empty(), "{site}");
    }
    let short = build_caption(&sample("youtube", Some("t"), Some("short"), None, false));
    assert!(
        short
            .caption
            .contains("<blockquote expandable>short</blockquote>")
    );
    assert!(!short.caption.contains("简介过长"));
    assert!(short.extras.is_empty());
}

#[test]
fn every_titled_site_links_author_and_original() {
    for site in SITES {
        let both = media_keyboard(&sample(site, None, None, None, true)).unwrap();
        assert_eq!(both.inline_keyboard[0].len(), 2, "{site}");
        assert_eq!(both.inline_keyboard[0][0].text, "作者", "{site}");
        assert_eq!(both.inline_keyboard[0][1].text, "原片", "{site}");
        let only = media_keyboard(&sample(site, None, None, None, false)).unwrap();
        assert_eq!(only.inline_keyboard[0].len(), 1, "{site}");
        assert_eq!(only.inline_keyboard[0][0].text, "原片", "{site}");
    }
}
