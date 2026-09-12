use feedbot_core::Extracted;

use super::blocks::{Block, Span};
use super::CaptionPage;

pub fn blocks(extracted: &Extracted, page: CaptionPage) -> Vec<Block> {
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

fn tag_line(tags: &[String]) -> Option<String> {
    let line = tags
        .iter()
        .filter(|t| !t.is_empty())
        .map(|t| format!("#{t}"))
        .collect::<Vec<_>>()
        .join(" ");
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use crate::caption::{
        build_caption, build_caption_page, utf16_len, CaptionPage, CAPTION_LIMIT,
    };
    use feedbot_core::{Extracted, MediaItem, MediaKind, MediaSource};
    use http::HeaderMap;
    use url::Url;

    #[allow(clippy::too_many_arguments)]
    fn sample(
        title: Option<&str>,
        author: Option<&str>,
        desc: Option<&str>,
        tags: &[&str],
        pages: usize,
        nsfw: bool,
        ai: bool,
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
            extractor_id: "pixiv".into(),
            canonical_url: Url::parse("https://example.com/a?x=1&y=2").unwrap(),
            title: title.map(str::to_string),
            author: author.map(str::to_string),
            author_url: with_author_url.then(|| Url::parse("https://example.com/u/1").unwrap()),
            description: desc.map(str::to_string),
            tags: tags.iter().map(|s| (*s).to_string()).collect(),
            nsfw,
            ai,
            engagement: None,
            items,
        }
    }

    fn extracted(title: Option<&str>, author: Option<&str>, desc: Option<&str>) -> Extracted {
        sample(title, author, desc, &[], 1, false, false, true)
    }

    #[test]
    fn escapes_html_in_title_and_uses_link() {
        let rendered = build_caption(&extracted(Some("A <B> & C"), Some("bob"), None));
        assert!(rendered
            .caption
            .starts_with("<a href=\"https://example.com/a?x=1&amp;y=2\">A &lt;B&gt; &amp; C</a>"));
        assert!(rendered
            .caption
            .contains(" / <a href=\"https://example.com/u/1\">bob</a>"));
        assert!(rendered.extras.is_empty());
    }

    #[test]
    fn tags_and_description_are_expandable() {
        let rendered = build_caption(&sample(
            Some("T"),
            Some("bob"),
            Some("hello\nworld"),
            &["猫", "original"],
            3,
            true,
            true,
            true,
        ));
        assert!(rendered.caption.starts_with("#NSFW #AI "));
        assert!(rendered
            .caption
            .contains("<a href=\"https://example.com/a?x=1&amp;y=2\">T</a>"));
        assert!(rendered
            .caption
            .contains(" / <a href=\"https://example.com/u/1\">bob</a>"));
        assert!(rendered.caption.contains(" 1/3"));
        assert!(rendered
            .caption
            .contains("<blockquote expandable>#猫 #original</blockquote>"));
        assert!(rendered
            .caption
            .contains("<blockquote expandable>hello\nworld</blockquote>"));
        assert!(rendered.extras.is_empty());
    }

    #[test]
    fn description_overflows_before_tags() {
        let desc = "x".repeat(2000);
        let rendered = build_caption(&sample(
            Some("T"),
            None,
            Some(&desc),
            &["cat"],
            1,
            false,
            false,
            false,
        ));
        assert!(rendered
            .caption
            .contains("<blockquote expandable>#cat</blockquote>"));
        assert!(rendered.caption.contains("<blockquote expandable>"));
        assert!(!rendered.extras.is_empty());
        let extras_plain: String = rendered
            .extras
            .iter()
            .map(|s| {
                s.replace("&amp;", "&")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
            })
            .collect();
        assert!(extras_plain.contains('x'));
        assert!(utf16_len(&visible_text_approx(&rendered.caption)) <= CAPTION_LIMIT);
    }

    fn visible_text_approx(html: &str) -> String {
        let mut out = String::new();
        let mut rest = html;
        while let Some(i) = rest.find('<') {
            out.push_str(&rest[..i]);
            rest = rest[i..]
                .find('>')
                .map(|e| &rest[i + e + 1..])
                .unwrap_or("");
        }
        out.push_str(rest);
        out.replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
    }

    #[test]
    fn truncates_caption_to_1024_utf16_and_overflows_rest() {
        let desc = "x".repeat(2000);
        let rendered = build_caption(&extracted(Some("T"), None, Some(&desc)));
        assert!(!rendered.extras.is_empty());
        assert!(rendered.caption.contains("<a href="));
        assert!(rendered.caption.contains("<blockquote expandable>"));
    }

    #[test]
    fn empty_title_falls_back_to_url() {
        let rendered = build_caption(&extracted(None, None, None));
        assert!(rendered
            .caption
            .contains("https://example.com/a?x=1&amp;y=2"));
        assert!(rendered.extras.is_empty());
    }

    #[test]
    fn author_without_url_is_plain_text() {
        let rendered = build_caption(&sample(
            Some("T"),
            Some("bob"),
            None,
            &[],
            1,
            false,
            false,
            false,
        ));
        assert!(rendered.caption.contains(" / bob"));
        assert!(!rendered.caption.contains("https://example.com/u/1"));
        assert!(!rendered.caption.contains(" 1/1"));
    }

    #[test]
    fn later_album_group_uses_starting_page() {
        let extracted = sample(Some("T"), Some("bob"), None, &[], 12, false, false, true);
        let rendered = build_caption_page(
            &extracted,
            CaptionPage {
                index: 6,
                total: 12,
            },
        );
        assert!(rendered.caption.contains(" 7/12"));
        assert!(!rendered.caption.contains(" 1/12"));
    }

    #[test]
    fn each_media_group_item_has_its_own_page() {
        let extracted = sample(Some("T"), Some("bob"), None, &[], 12, false, false, true);
        for (index, token) in [(0, " 1/12"), (1, " 2/12"), (9, " 10/12"), (11, " 12/12")] {
            let rendered = build_caption_page(&extracted, CaptionPage { index, total: 12 });
            assert!(
                rendered.caption.contains(token),
                "index {index} should contain {token}"
            );
        }
    }
}
