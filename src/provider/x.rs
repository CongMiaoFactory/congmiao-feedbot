use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use serde_json::Value;

use crate::{Config, model::*};

use super::{Provider, filename_from_url, get_str, get_u64, json_response};

static VIDEO_RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/(\d{2,5})x(\d{2,5})/").expect("valid X video resolution regex"));

pub struct XProvider {
    client: Client,
    base: String,
    regex: Regex,
}

impl XProvider {
    pub fn new(client: Client, config: &Config) -> Self {
        Self {
            client,
            base: config.fxtwitter_api_base.trim_end_matches('/').to_string(),
            regex: Regex::new(
                r"(?i)(?:https?://)?(?:www\.)?(?:x|twitter)\.com/[^/\s]+/status/(\d{2,20})",
            )
            .expect("valid X URL regex"),
        }
    }

    fn id(&self, url: &str) -> Option<String> {
        self.regex.captures(url).map(|c| c[1].to_string())
    }
}

#[async_trait]
impl Provider for XProvider {
    fn platform(&self) -> Platform {
        Platform::X
    }

    fn can_handle(&self, url: &str) -> bool {
        self.regex.is_match(url)
    }

    async fn parse(&self, request: &ParseRequest) -> ProviderResult<ParsedContent> {
        let id = self
            .id(&request.url)
            .ok_or_else(|| ProviderError::InvalidUrl(request.url.clone()))?;
        let endpoint = format!("{}/2/status/{id}", self.base);
        let root = json_response(
            self.client
                .get(&endpoint)
                .send()
                .await
                .map_err(|e| ProviderError::Upstream(e.to_string()))?,
            "FxTwitter",
        )
        .await?;
        if get_u64(&root, "/code") != Some(200) {
            return Err(ProviderError::Unavailable(request.url.clone()));
        }
        let status = root
            .get("status")
            .ok_or_else(|| ProviderError::InvalidResponse("FxTwitter 缺少 status".into()))?;
        let author = status.get("author").unwrap_or(&Value::Null);
        let canonical_url = get_str(status, "/url").unwrap_or(&request.url).to_string();
        let mut media = Vec::new();
        if let Some(items) = status.pointer("/media/photos").and_then(Value::as_array) {
            for (index, item) in items.iter().enumerate() {
                if let Some(url) = item.get("url").and_then(Value::as_str) {
                    media.push(MediaItem {
                        kind: if item.get("type").and_then(Value::as_str) == Some("gif") {
                            MediaKind::Animation
                        } else {
                            MediaKind::Photo
                        },
                        source_url: url.into(),
                        fallback_urls: vec![],
                        thumbnail_url: None,
                        filename: filename_from_url(url, &format!("x-{id}-{index}.jpg")),
                        mime_type: None,
                        duration_secs: None,
                        width: item.get("width").and_then(Value::as_u64).map(|n| n as u32),
                        height: item.get("height").and_then(Value::as_u64).map(|n| n as u32),
                        size: None,
                        headers: Default::default(),
                        cache_key: format!("x:{id}:photo:{index}"),
                        requires_download: false,
                        secondary_url: None,
                    });
                }
            }
        }
        if let Some(items) = status.pointer("/media/videos").and_then(Value::as_array) {
            let quality = request.options.quality.unwrap_or(720);
            for (index, item) in items.iter().enumerate() {
                let selected = select_video_format(item, quality);
                let url = selected
                    .as_ref()
                    .map(|format| format.url)
                    .or_else(|| item.get("url").and_then(Value::as_str));
                if let Some(url) = url {
                    let duration_secs = item.get("duration").and_then(Value::as_u64);
                    let size = selected
                        .as_ref()
                        .and_then(|format| format.size)
                        .or_else(|| item.get("filesize").and_then(Value::as_u64))
                        .or_else(|| {
                            selected
                                .as_ref()
                                .and_then(|format| format.bitrate)
                                .zip(duration_secs)
                                .map(|(bitrate, duration)| bitrate.saturating_mul(duration) / 8)
                        });
                    media.push(MediaItem {
                        kind: if item.get("type").and_then(Value::as_str) == Some("gif") {
                            MediaKind::Animation
                        } else {
                            MediaKind::Video
                        },
                        source_url: url.into(),
                        fallback_urls: vec![],
                        thumbnail_url: item
                            .get("thumbnail_url")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        filename: filename_from_url(url, &format!("x-{id}-{index}.mp4")),
                        mime_type: Some("video/mp4".into()),
                        duration_secs,
                        width: selected
                            .as_ref()
                            .and_then(|format| format.width)
                            .or_else(|| {
                                item.get("width").and_then(Value::as_u64).map(|n| n as u32)
                            }),
                        height: selected
                            .as_ref()
                            .and_then(|format| format.height)
                            .or_else(|| {
                                item.get("height").and_then(Value::as_u64).map(|n| n as u32)
                            }),
                        size,
                        headers: Default::default(),
                        cache_key: format!("x:{id}:video:{index}:{quality}p"),
                        requires_download: true,
                        secondary_url: None,
                    });
                }
            }
        }
        let quote_text = status
            .pointer("/quote/text")
            .and_then(Value::as_str)
            .map(|q| format!("\n\n引用：{q}"))
            .unwrap_or_default();
        let text = format!(
            "{}{}",
            get_str(status, "/text").unwrap_or_default(),
            quote_text
        );
        let sensitive = [
            "/possibly_sensitive",
            "/sensitive",
            "/media/possibly_sensitive",
            "/quote/possibly_sensitive",
        ]
        .into_iter()
        .any(|pointer| status.pointer(pointer).and_then(Value::as_bool) == Some(true))
            || has_adult_marker(&text);
        Ok(ParsedContent {
            platform: Platform::X,
            kind: ContentKind::Post,
            id,
            canonical_url,
            author: Author {
                id: author
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                name: author
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                url: author
                    .get("screen_name")
                    .and_then(Value::as_str)
                    .map(|n| format!("https://x.com/{n}")),
                avatar_url: author
                    .get("avatar_url")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            title: String::new(),
            text,
            sensitive,
            stats: Stats {
                likes: get_u64(status, "/likes"),
                reposts: get_u64(status, "/reposts"),
                replies: get_u64(status, "/replies"),
                views: get_u64(status, "/views"),
            },
            media,
            collection_items: Vec::new(),
        })
    }
}

struct SelectedVideoFormat<'a> {
    url: &'a str,
    width: Option<u32>,
    height: Option<u32>,
    bitrate: Option<u64>,
    size: Option<u64>,
}

fn select_video_format(item: &Value, quality: u32) -> Option<SelectedVideoFormat<'_>> {
    let candidates: Vec<_> = item
        .get("formats")?
        .as_array()?
        .iter()
        .filter(|format| {
            !matches!(
                format.get("codec").and_then(Value::as_str),
                Some("hevc" | "vp9" | "av1")
            )
        })
        .filter_map(|format| {
            let url = format.get("url")?.as_str()?;
            if url.contains(".m3u8") || !url.contains(".mp4") {
                return None;
            }
            let dimensions = format_dimensions(format, url);
            Some(SelectedVideoFormat {
                url,
                width: dimensions.map(|value| value.0),
                height: dimensions.map(|value| value.1),
                bitrate: format.get("bitrate").and_then(Value::as_u64),
                size: format.get("size").and_then(Value::as_u64),
            })
        })
        .collect();
    candidates
        .iter()
        .filter(|format| {
            format
                .width
                .zip(format.height)
                .is_some_and(|(width, height)| width.min(height) <= quality)
        })
        .max_by_key(|format| {
            (
                format
                    .width
                    .zip(format.height)
                    .map(|(width, height)| width.min(height))
                    .unwrap_or_default(),
                format.bitrate.unwrap_or_default(),
            )
        })
        .or_else(|| {
            candidates
                .iter()
                .min_by_key(|format| format.bitrate.unwrap_or(u64::MAX))
        })
        .map(|format| SelectedVideoFormat {
            url: format.url,
            width: format.width,
            height: format.height,
            bitrate: format.bitrate,
            size: format.size,
        })
}

fn format_dimensions(format: &Value, url: &str) -> Option<(u32, u32)> {
    format
        .get("width")
        .and_then(Value::as_u64)
        .zip(format.get("height").and_then(Value::as_u64))
        .and_then(|(width, height)| Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?)))
        .or_else(|| {
            let captures = VIDEO_RESOLUTION.captures(url)?;
            Some((captures[1].parse().ok()?, captures[2].parse().ok()?))
        })
}

fn has_adult_marker(text: &str) -> bool {
    text.split(|c: char| c.is_whitespace() || ",，。!！?？/\\|".contains(c))
        .map(|word| word.trim_matches('#').to_ascii_lowercase())
        .any(|word| {
            matches!(
                word.as_str(),
                "r18" | "r-18" | "r18g" | "r-18g" | "nsfw" | "18+" | "成人向"
            )
        })
}
