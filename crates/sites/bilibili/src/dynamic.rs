//! Dynamics (feed posts) and opus articles.

use super::api::media_headers;
use super::links::parse_https_url;
use feedbot_core::extract::{
    Engagement, ExtractError, Extracted, MediaItem, MediaKind, MediaSource,
};
use http::HeaderMap;
use serde_json::Value;
use std::collections::HashSet;
use url::Url;

use feedbot_site_common::{json_str, json_u64};

#[derive(Default)]
pub(super) struct OpusProjection {
    title: String,
    text: String,
    pictures: Vec<String>,
}

pub(super) fn dynamic_from_item(
    id: &str,
    item: &Value,
    headers: &HeaderMap,
) -> Result<Extracted, ExtractError> {
    let outer_dynamic = item
        .pointer("/modules/module_dynamic")
        .unwrap_or(&Value::Null);
    let outer_text = dynamic_text(outer_dynamic);
    let is_forward = json_str(item, "/type") == Some("DYNAMIC_TYPE_FORWARD")
        || item.get("orig").is_some_and(|v| !v.is_null());
    let source_item = item.get("orig").filter(|v| !v.is_null()).unwrap_or(item);
    let source_dynamic = source_item
        .pointer("/modules/module_dynamic")
        .unwrap_or(&Value::Null);
    let source_major = source_dynamic.pointer("/major").unwrap_or(&Value::Null);
    let source_id = source_id(source_item, id);
    let title = major_title(source_major);
    let mut text = dynamic_text(source_dynamic);
    let pictures = pictures_from_dynamic(source_dynamic);
    if is_forward {
        let origin_author = author_name(source_item);
        let origin = if text.is_empty() {
            "原动态已删除或不可见".into()
        } else if origin_author.is_empty() {
            text.clone()
        } else {
            format!("转发自 {origin_author}：\n{text}")
        };
        text = if outer_text.is_empty() {
            origin
        } else {
            format!("{outer_text}\n\n{origin}")
        };
    }
    let mut items = photos_from_urls(source_id, &pictures, headers);
    if items.is_empty()
        && let Some(cover) = major_cover(source_major).and_then(parse_https_url)
    {
        items.push(photo_item(source_id, 0, cover, headers));
    }
    let author_mid = json_u64(item, "/modules/module_author/mid")
        .or_else(|| json_u64(item, "/modules/module_author/author/mid"))
        .or_else(|| json_u64(item, "/basic/uid"));
    Ok(Extracted {
        extractor_id: "bilibili".into(),
        canonical_url: Url::parse(&format!("https://www.bilibili.com/opus/{id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?,
        title: (!title.is_empty()).then_some(title),
        author: {
            let n = author_name(item);
            (!n.is_empty()).then_some(n)
        },
        author_url: author_mid
            .and_then(|n| Url::parse(&format!("https://space.bilibili.com/{n}")).ok()),
        description: (!text.is_empty()).then_some(text),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: Engagement::packed(
            json_u64(item, "/modules/module_stat/view/count"),
            json_u64(item, "/modules/module_stat/like/count"),
            json_u64(item, "/modules/module_stat/forward/count"),
            json_u64(item, "/modules/module_stat/comment/count"),
        ),
        items,
    })
}

pub(super) fn apply_opus(extracted: &mut Extracted, opus: OpusProjection) {
    if !opus.title.is_empty() {
        extracted.title = Some(opus.title);
    }
    if !opus.text.is_empty() {
        extracted.description = Some(opus.text);
    }
    if !opus.pictures.is_empty()
        && let Ok(headers) = media_headers(None)
    {
        let id = extracted
            .items
            .first()
            .map(|i| i.id.as_str())
            .unwrap_or("opus");
        extracted.items = photos_from_urls(id, &opus.pictures, &headers);
    }
}

pub(super) fn opus_from_item(item: &Value) -> OpusProjection {
    let mut title = json_str(item, "/basic/title")
        .unwrap_or_default()
        .to_string();
    let mut paragraphs = Vec::new();
    let mut pictures = Vec::new();
    if let Some(modules) = item.get("modules").and_then(Value::as_array) {
        for module in modules {
            if title.is_empty() {
                title = json_str(module, "/module_title/text")
                    .unwrap_or_default()
                    .to_string();
            }
            let Some(content) = module.get("module_content") else {
                continue;
            };
            if let Some(items) = content.pointer("/paragraphs").and_then(Value::as_array) {
                for paragraph in items {
                    let line = opus_paragraph_text(paragraph);
                    if !line.is_empty() {
                        paragraphs.push(line);
                    }
                    if let Some(pics) = paragraph.pointer("/pic/pics").and_then(Value::as_array) {
                        for pic in pics {
                            if let Some(url) = json_str(pic, "/url")
                                .or_else(|| json_str(pic, "/src"))
                                .and_then(normalize_asset)
                            {
                                pictures.push(url);
                            }
                        }
                    }
                }
            }
        }
    }
    OpusProjection {
        title,
        text: paragraphs.join("\n\n"),
        pictures,
    }
}

pub(super) fn needs_opus(item: &Value) -> bool {
    json_str(
        item.pointer("/modules/module_dynamic/major")
            .unwrap_or(&Value::Null),
        "/type",
    ) == Some("MAJOR_TYPE_OPUS")
        || json_str(
            item.pointer("/orig/modules/module_dynamic/major")
                .unwrap_or(&Value::Null),
            "/type",
        ) == Some("MAJOR_TYPE_OPUS")
}

pub(super) fn source_id<'a>(item: &'a Value, fallback: &'a str) -> &'a str {
    json_str(item, "/id_str")
        .or_else(|| json_str(item, "/basic/dyn_id_str"))
        .unwrap_or(fallback)
}

pub(super) fn author_name(item: &Value) -> String {
    json_str(item, "/modules/module_author/name")
        .or_else(|| json_str(item, "/modules/module_author/uname"))
        .or_else(|| json_str(item, "/modules/module_author/author/name"))
        .unwrap_or_default()
        .to_string()
}

pub(super) fn dynamic_text(dynamic: &Value) -> String {
    let text = json_str(dynamic, "/desc/text")
        .or_else(|| json_str(dynamic, "/major/opus/summary/text"))
        .unwrap_or_default();
    if !text.is_empty() {
        return text.to_string();
    }
    dynamic
        .pointer("/desc/rich_text_nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|node| {
                    json_str(node, "/text")
                        .or_else(|| json_str(node, "/orig_text"))
                        .or_else(|| json_str(node, "/emoji/text"))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn major_title(major: &Value) -> String {
    [
        "/opus/title",
        "/archive/title",
        "/pgc/title",
        "/article/title",
        "/music/title",
        "/live/title",
        "/common/title",
        "/title",
    ]
    .into_iter()
    .find_map(|p| json_str(major, p).filter(|s| !s.is_empty()))
    .unwrap_or_default()
    .to_string()
}

pub(super) fn major_cover(major: &Value) -> Option<&str> {
    [
        "/archive/cover",
        "/pgc/cover",
        "/article/cover",
        "/article/covers/0",
        "/music/cover",
        "/live/cover",
        "/common/cover",
        "/cover",
    ]
    .into_iter()
    .find_map(|p| json_str(major, p))
}

pub(super) fn pictures_from_dynamic(dynamic: &Value) -> Vec<String> {
    dynamic
        .pointer("/major/opus/pics")
        .or_else(|| dynamic.pointer("/major/draw/items"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| {
                    json_str(v, "/url")
                        .or_else(|| json_str(v, "/src"))
                        .or_else(|| json_str(v, "/live_url"))
                        .and_then(normalize_asset)
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn embedded_url(major: &Value) -> Option<String> {
    let archive = major.get("archive").unwrap_or(&Value::Null);
    if let Some(bvid) = json_str(archive, "/bvid") {
        return Some(format!("https://www.bilibili.com/video/{bvid}"));
    }
    if let Some(aid) = json_u64(archive, "/aid") {
        return Some(format!("https://www.bilibili.com/video/av{aid}"));
    }
    for pointer in [
        "/archive/jump_url",
        "/pgc/jump_url",
        "/article/jump_url",
        "/music/jump_url",
        "/live/jump_url",
        "/common/jump_url",
    ] {
        if let Some(url) = json_str(major, pointer).and_then(normalize_asset) {
            return Some(url);
        }
    }
    if let Some(auid) = json_u64(major, "/music/id") {
        return Some(format!("https://www.bilibili.com/audio/au{auid}"));
    }
    if let Some(article_id) = json_u64(major, "/article/id") {
        return Some(format!("https://www.bilibili.com/read/cv{article_id}"));
    }
    if let Some(room_id) = json_u64(major, "/live/room_id").or_else(|| json_u64(major, "/live/id"))
    {
        return Some(format!("https://live.bilibili.com/{room_id}"));
    }
    None
}

pub(super) fn opus_paragraph_text(paragraph: &Value) -> String {
    if let Some(nodes) = paragraph.pointer("/text/nodes").and_then(Value::as_array) {
        let text: String = nodes
            .iter()
            .filter_map(|node| {
                json_str(node, "/word/words")
                    .or_else(|| json_str(node, "/rich/text"))
                    .or_else(|| json_str(node, "/rich/orig_text"))
            })
            .collect();
        if !text.is_empty() {
            return text;
        }
    }
    json_str(paragraph, "/text/text")
        .unwrap_or_default()
        .to_string()
}

pub(super) fn photos_from_urls(id: &str, urls: &[String], headers: &HeaderMap) -> Vec<MediaItem> {
    let mut seen = HashSet::new();
    urls.iter()
        .filter_map(|u| parse_https_url(u))
        .filter(|u| seen.insert(u.as_str().to_string()))
        .enumerate()
        .map(|(i, url)| photo_item(id, i, url, headers))
        .collect()
}

pub(super) fn photo_item(id: &str, index: usize, url: Url, headers: &HeaderMap) -> MediaItem {
    MediaItem {
        id: format!("{id}_{index}"),
        kind: MediaKind::Photo,
        source: MediaSource::Direct {
            url,
            headers: headers.clone(),
        },
        filename: Some(format!("bilibili-dynamic-{id}-{index}.jpg")),
        cover: None,
    }
}

pub(super) fn normalize_asset(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with("//") {
        return Some(format!("https:{value}"));
    }
    if let Some(stripped) = value.strip_prefix("http://") {
        return Some(format!("https://{stripped}"));
    }
    value.starts_with("https://").then(|| value.to_string())
}
