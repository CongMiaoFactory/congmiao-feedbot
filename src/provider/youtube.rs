use std::process::Stdio;

use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use serde_json::Value;
use tokio::process::Command;

use crate::{Config, model::*};

use super::{Provider, get_str, get_u64, json_response};

pub struct YouTubeProvider {
    client: Client,
    api_key: Option<String>,
    yt_dlp: String,
    cookies: Option<std::path::PathBuf>,
    regex: Regex,
}

impl YouTubeProvider {
    pub fn new(client: Client, config: &Config) -> Self {
        Self { client, api_key: config.youtube_api_key.clone(), yt_dlp: config.yt_dlp_path.clone(), cookies: config.youtube_cookies_file.clone(),
            regex: Regex::new(r"(?i)(?:youtu\.be/|youtube(?:-nocookie)?\.com/(?:watch\?(?:[^\s#]*&)?v=|shorts/|live/|embed/))([A-Za-z0-9_-]{11})").expect("valid YouTube regex") }
    }
    fn id(&self, url: &str) -> Option<String> {
        self.regex.captures(url).map(|c| c[1].to_string())
    }

    async fn yt_dlp_json(&self, url: &str) -> ProviderResult<Value> {
        let mut cmd = Command::new(&self.yt_dlp);
        cmd.args(["--dump-single-json", "--no-playlist", "--no-warnings", url])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(path) = &self.cookies {
            cmd.arg("--cookies").arg(path);
        }
        let out = cmd
            .output()
            .await
            .map_err(|e| ProviderError::Media(format!("无法启动 yt-dlp: {e}")))?;
        if !out.status.success() {
            return Err(ProviderError::Media(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        serde_json::from_slice(&out.stdout)
            .map_err(|e| ProviderError::InvalidResponse(format!("yt-dlp JSON: {e}")))
    }
}

#[async_trait]
impl Provider for YouTubeProvider {
    fn platform(&self) -> Platform {
        Platform::YouTube
    }
    fn can_handle(&self, url: &str) -> bool {
        self.regex.is_match(url)
    }

    async fn parse(&self, request: &ParseRequest) -> ProviderResult<ParsedContent> {
        let id = self
            .id(&request.url)
            .ok_or_else(|| ProviderError::InvalidUrl(request.url.clone()))?;
        let canonical = format!("https://www.youtube.com/watch?v={id}");
        let mut data = Value::Null;
        if let Some(key) = &self.api_key {
            let response = self
                .client
                .get("https://www.googleapis.com/youtube/v3/videos")
                .query(&[
                    ("part", "snippet,contentDetails,statistics"),
                    ("id", id.as_str()),
                    ("key", key.as_str()),
                ])
                .send()
                .await
                .map_err(|e| ProviderError::Upstream(e.to_string()))?;
            let root = json_response(response, "YouTube Data API").await?;
            data = root.pointer("/items/0").cloned().unwrap_or(Value::Null);
        }
        let ytdlp = self.yt_dlp_json(&canonical).await.ok();
        if data.is_null() && ytdlp.is_none() {
            let response = self
                .client
                .get("https://www.youtube.com/oembed")
                .query(&[("url", canonical.as_str()), ("format", "json")])
                .send()
                .await
                .map_err(|e| ProviderError::Upstream(e.to_string()))?;
            data = json_response(response, "YouTube oEmbed").await?;
        }
        let y = ytdlp.as_ref().unwrap_or(&Value::Null);
        let title = get_str(&data, "/snippet/title")
            .or_else(|| get_str(y, "/title"))
            .or_else(|| get_str(&data, "/title"))
            .unwrap_or_default()
            .to_string();
        let author_name = get_str(&data, "/snippet/channelTitle")
            .or_else(|| get_str(y, "/channel"))
            .or_else(|| get_str(&data, "/author_name"))
            .unwrap_or_default()
            .to_string();
        let thumbnail = get_str(&data, "/snippet/thumbnails/maxres/url")
            .or_else(|| get_str(&data, "/snippet/thumbnails/high/url"))
            .or_else(|| get_str(y, "/thumbnail"))
            .or_else(|| get_str(&data, "/thumbnail_url"))
            .map(str::to_string);
        let media = vec![MediaItem {
            kind: MediaKind::Video,
            source_url: canonical.clone(),
            thumbnail_url: thumbnail,
            filename: format!("youtube-{id}.mp4"),
            mime_type: Some("video/mp4".into()),
            duration_secs: get_u64(y, "/duration"),
            width: get_u64(y, "/width").map(|n| n as u32),
            height: get_u64(y, "/height").map(|n| n as u32),
            size: get_u64(y, "/filesize_approx"),
            headers: Default::default(),
            cache_key: format!("youtube:{id}:{}p", request.options.quality.unwrap_or(720)),
            requires_download: true,
            secondary_url: None,
        }];
        Ok(ParsedContent {
            platform: Platform::YouTube,
            kind: ContentKind::Video,
            id,
            canonical_url: canonical,
            author: Author {
                id: get_str(y, "/channel_id").unwrap_or_default().into(),
                name: author_name,
                url: get_str(y, "/channel_url").map(str::to_string),
                avatar_url: None,
            },
            title,
            text: get_str(&data, "/snippet/description")
                .or_else(|| get_str(y, "/description"))
                .unwrap_or_default()
                .to_string(),
            stats: Stats {
                likes: get_u64(&data, "/statistics/likeCount")
                    .or_else(|| get_u64(y, "/like_count")),
                reposts: None,
                replies: get_u64(&data, "/statistics/commentCount")
                    .or_else(|| get_u64(y, "/comment_count")),
                views: get_u64(&data, "/statistics/viewCount")
                    .or_else(|| get_u64(y, "/view_count")),
            },
            media,
            collection_items: Vec::new(),
        })
    }
}
