use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use serde_json::Value;

use crate::{Config, model::*};

use super::{Provider, filename_from_url, get_str, get_u64, json_response};

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
            for (index, item) in items.iter().enumerate() {
                if let Some(url) = item.get("url").and_then(Value::as_str) {
                    media.push(MediaItem {
                        kind: if item.get("type").and_then(Value::as_str) == Some("gif") {
                            MediaKind::Animation
                        } else {
                            MediaKind::Video
                        },
                        source_url: url.into(),
                        thumbnail_url: item
                            .get("thumbnail_url")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        filename: filename_from_url(url, &format!("x-{id}-{index}.mp4")),
                        mime_type: Some("video/mp4".into()),
                        duration_secs: item
                            .get("duration")
                            .and_then(Value::as_u64)
                            .map(|n| n / 1000),
                        width: item.get("width").and_then(Value::as_u64).map(|n| n as u32),
                        height: item.get("height").and_then(Value::as_u64).map(|n| n as u32),
                        size: item.get("filesize").and_then(Value::as_u64),
                        headers: Default::default(),
                        cache_key: format!("x:{id}:video:{index}"),
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
            text: format!(
                "{}{}",
                get_str(status, "/text").unwrap_or_default(),
                quote_text
            ),
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
