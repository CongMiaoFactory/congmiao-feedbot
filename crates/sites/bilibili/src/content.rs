//! Live rooms, audio tracks, and columns.

use super::links::parse_https_url;
use feedbot_core::extract::{
    Engagement, ExtractError, Extracted, MediaItem, MediaKind, MediaSource,
};
use http::HeaderMap;
use serde_json::Value;
use url::Url;

use feedbot_site_common::{json_str, json_u64};

pub(super) fn live_from_room(
    id: &str,
    root: &Value,
    headers: &HeaderMap,
) -> Result<Extracted, ExtractError> {
    let room = root.pointer("/data/room_info").unwrap_or(&Value::Null);
    let anchor = root
        .pointer("/data/anchor_info/base_info")
        .unwrap_or(&Value::Null);
    let cover = json_str(room, "/cover")
        .or_else(|| json_str(room, "/keyframe"))
        .and_then(parse_https_url)
        .ok_or_else(|| ExtractError::Failed("直播没有封面".into()))?;
    let uid = json_u64(room, "/uid");
    Ok(Extracted {
        extractor_id: "bilibili".into(),
        canonical_url: Url::parse(&format!("https://live.bilibili.com/{id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?,
        title: json_str(room, "/title").map(str::to_string),
        author: json_str(anchor, "/uname").map(str::to_string),
        author_url: uid.and_then(|n| Url::parse(&format!("https://space.bilibili.com/{n}")).ok()),
        description: json_str(room, "/description")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: Engagement::packed(json_u64(room, "/online"), None, None, None),
        items: vec![MediaItem {
            id: id.into(),
            kind: MediaKind::Photo,
            source: MediaSource::Direct {
                url: cover,
                headers: headers.clone(),
            },
            filename: Some(format!("bilibili-live-{id}.jpg")),
            cover: None,
        }],
    })
}

pub(super) fn audio_from_api(
    id: &str,
    song_root: &Value,
    stream: &Value,
    headers: &HeaderMap,
) -> Result<Extracted, ExtractError> {
    let song = song_root.get("data").unwrap_or(&Value::Null);
    let mut sources: Vec<Url> = stream
        .pointer("/data/cdns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(parse_https_url)
        .collect();
    if sources.is_empty() {
        return Err(ExtractError::Failed("没有可用的音频地址".into()));
    }
    let url = sources.remove(0);
    let cover = json_str(song, "/cover").and_then(parse_https_url);
    let uid = json_u64(song, "/uid");
    Ok(Extracted {
        extractor_id: "bilibili".into(),
        canonical_url: Url::parse(&format!("https://www.bilibili.com/audio/au{id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?,
        title: json_str(song, "/title").map(str::to_string),
        author: json_str(song, "/uname").map(str::to_string),
        author_url: uid.and_then(|n| Url::parse(&format!("https://space.bilibili.com/{n}")).ok()),
        description: json_str(song, "/intro")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: Engagement::packed(
            json_u64(song, "/passtime"),
            None,
            None,
            json_u64(song, "/comment"),
        ),
        items: vec![MediaItem {
            id: format!("au{id}"),
            kind: MediaKind::Audio,
            source: MediaSource::Direct {
                url,
                headers: headers.clone(),
            },
            filename: Some(format!("bilibili-au{id}.m4a")),
            cover,
        }],
    })
}

pub(super) fn article_from_api(
    id: &str,
    root: &Value,
    headers: &HeaderMap,
) -> Result<Extracted, ExtractError> {
    let data = root.get("data").unwrap_or(&Value::Null);
    let cover = json_str(data, "/banner_url").and_then(parse_https_url);
    let mid = json_u64(data, "/mid");
    let items = match cover {
        Some(url) => vec![MediaItem {
            id: format!("cv{id}"),
            kind: MediaKind::Photo,
            source: MediaSource::Direct {
                url,
                headers: headers.clone(),
            },
            filename: Some(format!("bilibili-cv{id}.jpg")),
            cover: None,
        }],
        None => Vec::new(),
    };
    Ok(Extracted {
        extractor_id: "bilibili".into(),
        canonical_url: Url::parse(&format!("https://www.bilibili.com/read/cv{id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?,
        title: json_str(data, "/title").map(str::to_string),
        author: json_str(data, "/author_name").map(str::to_string),
        author_url: mid.and_then(|n| Url::parse(&format!("https://space.bilibili.com/{n}")).ok()),
        description: json_str(data, "/summary")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: Engagement::packed(
            json_u64(data, "/stats/view"),
            json_u64(data, "/stats/like"),
            None,
            json_u64(data, "/stats/reply"),
        ),
        items,
    })
}
