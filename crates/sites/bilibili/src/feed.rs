//! Space (uploader) subscriptions: `space.bilibili.com/{mid}` for every
//! dynamic, `/{mid}/video` for uploads only.

use super::BilibiliExtractor;
use super::links::{is_bilibili_host, is_bvid};
use feedbot_core::extract::{ExtractCtx, ExtractError, FeedEntry};
use serde_json::Value;
use url::Url;

use feedbot_site_common::{is_digits, json_str, json_u64, parse_url};

const SPACE_FEATURES: &str = "itemOpusStyle";
const TYPE_VIDEO: &str = "DYNAMIC_TYPE_AV";
/// Deleted or invisible dynamics.
const TYPE_NONE: &str = "DYNAMIC_TYPE_NONE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SpaceSource {
    pub(super) mid: String,
    pub(super) videos_only: bool,
}

/// `space.bilibili.com/{mid}[/dynamic|/video|/upload/video]` and the mobile
/// `bilibili.com/space/{mid}`.
pub(super) fn parse_space(url: &Url) -> Option<SpaceSource> {
    let host = url.host_str()?.to_ascii_lowercase();
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let rest = if host == "space.bilibili.com" {
        segments.as_slice()
    } else if is_bilibili_host(&host) && segments.first() == Some(&"space") {
        &segments[1..]
    } else {
        return None;
    };
    let (mid, tab) = match rest {
        [mid] => (*mid, None),
        [mid, tab, ..] => (*mid, Some(tab.to_ascii_lowercase())),
        [] => return None,
    };
    if !is_digits(mid) {
        return None;
    }
    let videos_only = match tab.as_deref() {
        None | Some("dynamic") => false,
        Some("video" | "upload") => true,
        Some(_) => return None,
    };
    Some(SpaceSource {
        mid: mid.to_string(),
        videos_only,
    })
}

impl BilibiliExtractor {
    pub(super) async fn space_feed(
        &self,
        ctx: &ExtractCtx,
        source: &SpaceSource,
        cookie: Option<&str>,
    ) -> Result<Vec<FeedEntry>, ExtractError> {
        let root = self
            .wbi_json(
                ctx,
                &self.api_base,
                "/x/polymer/web-dynamic/v1/feed/space",
                &[
                    ("host_mid".into(), source.mid.clone()),
                    ("features".into(), SPACE_FEATURES.into()),
                ],
                cookie,
            )
            .await?;
        space_entries(&root, source.videos_only)
    }
}

/// Feed entries from the first page of a space feed, newest first. Video
/// dynamics point at the video so they get the video layout; others at the
/// dynamic itself. The key is always the dynamic id.
pub(super) fn space_entries(
    root: &Value,
    videos_only: bool,
) -> Result<Vec<FeedEntry>, ExtractError> {
    let items = root
        .pointer("/data/items")
        .and_then(Value::as_array)
        .ok_or_else(|| ExtractError::Failed("Bilibili 未返回动态列表".into()))?;
    let mut dated: Vec<(u64, FeedEntry)> = Vec::new();
    for item in items {
        let Some(id) = json_str(item, "/id_str").filter(|s| is_digits(s)) else {
            continue;
        };
        let kind = json_str(item, "/type").unwrap_or_default();
        if kind == TYPE_NONE {
            continue;
        }
        let bvid = json_str(item, "/modules/module_dynamic/major/archive/bvid")
            .filter(|b| kind == TYPE_VIDEO && is_bvid(b));
        if videos_only && bvid.is_none() {
            continue;
        }
        let link = match bvid {
            Some(b) => format!("https://www.bilibili.com/video/{b}"),
            None => format!("https://t.bilibili.com/{id}"),
        };
        let url = parse_url(&link)?;
        // A pinned dynamic comes first regardless of age; order by publish time
        // so it does not count as the newest entry.
        let published = json_u64(item, "/modules/module_author/pub_ts").unwrap_or(0);
        dated.push((
            published,
            FeedEntry {
                key: id.to_string(),
                url,
            },
        ));
    }
    dated.sort_by_key(|(published, _)| std::cmp::Reverse(*published));
    Ok(dated.into_iter().map(|(_, e)| e).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn space_links() {
        let all = |mid: &str| {
            Some(SpaceSource {
                mid: mid.into(),
                videos_only: false,
            })
        };
        let videos = Some(SpaceSource {
            mid: "2".into(),
            videos_only: true,
        });
        assert_eq!(parse_space(&url("https://space.bilibili.com/2")), all("2"));
        assert_eq!(
            parse_space(&url("https://space.bilibili.com/2/dynamic")),
            all("2")
        );
        assert_eq!(
            parse_space(&url("https://space.bilibili.com/2/video")),
            videos
        );
        assert_eq!(
            parse_space(&url("https://space.bilibili.com/2/upload/video")),
            videos
        );
        assert_eq!(parse_space(&url("https://bilibili.com/space/7")), all("7"));
        assert_eq!(
            parse_space(&url("https://m.bilibili.com/space/7")),
            all("7")
        );
        assert_eq!(parse_space(&url("https://space.bilibili.com/")), None);
        assert_eq!(parse_space(&url("https://space.bilibili.com/abc")), None);
        assert_eq!(
            parse_space(&url("https://space.bilibili.com/2/favlist")),
            None
        );
        assert_eq!(
            parse_space(&url("https://www.bilibili.com/video/BV1xx411c7mD")),
            None
        );
        assert_eq!(parse_space(&url("https://example.com/space/2")), None);
    }

    fn fixture() -> Value {
        json!({
            "code": 0,
            "data": {
                "items": [
                    {
                        "id_str": "100",
                        "type": "DYNAMIC_TYPE_DRAW",
                        "modules": {
                            "module_tag": { "text": "置顶" },
                            "module_author": { "pub_ts": 1000 }
                        }
                    },
                    {
                        "id_str": "300",
                        "type": "DYNAMIC_TYPE_AV",
                        "modules": {
                            "module_author": { "pub_ts": 3000 },
                            "module_dynamic": {
                                "major": { "archive": { "bvid": "BV1xx411c7mD" } }
                            }
                        }
                    },
                    {
                        "id_str": "250",
                        "type": "DYNAMIC_TYPE_NONE",
                        "modules": { "module_author": { "pub_ts": 2500 } }
                    },
                    {
                        "id_str": "200",
                        "type": "DYNAMIC_TYPE_FORWARD",
                        "modules": { "module_author": { "pub_ts": 2000 } }
                    },
                    { "type": "DYNAMIC_TYPE_WORD" }
                ]
            }
        })
    }

    #[test]
    fn entries_newest_first_with_video_links() {
        let entries = space_entries(&fixture(), false).unwrap();
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["300", "200", "100"]);
        assert_eq!(
            entries[0].url.as_str(),
            "https://www.bilibili.com/video/BV1xx411c7mD"
        );
        assert_eq!(entries[1].url.as_str(), "https://t.bilibili.com/200");
    }

    #[test]
    fn videos_only_keeps_uploads() {
        let entries = space_entries(&fixture(), true).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "300");
    }

    #[test]
    fn missing_items_is_an_error() {
        assert!(space_entries(&json!({ "code": 0, "data": {} }), false).is_err());
        assert!(
            space_entries(&json!({ "code": 0, "data": { "items": [] } }), false)
                .unwrap()
                .is_empty()
        );
    }
}
