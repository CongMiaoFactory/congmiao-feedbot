use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use serde_json::Value;

use crate::{Config, RuntimeCredentials, model::*};

use super::{Provider, get_str, get_u64, json_response};

#[derive(Debug, Clone, Copy)]
enum NeteaseKind {
    Song,
    Album,
    Playlist,
    Mv,
}

pub struct NeteaseProvider {
    client: Client,
    base: String,
    credentials: RuntimeCredentials,
    regex: Regex,
}

impl NeteaseProvider {
    pub fn new(client: Client, config: &Config) -> Self {
        Self::new_with_credentials(client, config, RuntimeCredentials::memory(config))
    }

    pub fn new_with_credentials(
        client: Client,
        config: &Config,
        credentials: RuntimeCredentials,
    ) -> Self {
        Self {
            client,
            base: config.netease_api_base.trim_end_matches('/').into(),
            credentials,
            regex: Regex::new(
                r"(?i)(?:https?://)?(?:music\.163\.com|y\.music\.163\.com|163cn\.tv)/[^\s]*",
            )
            .expect("valid Netease regex"),
        }
    }
    async fn resolve(&self, raw: &str) -> ProviderResult<(NeteaseKind, String, String)> {
        let input = if raw.starts_with("http") {
            raw.to_string()
        } else {
            format!("https://{raw}")
        };
        let url = if raw.contains("y.music.163.com") || raw.contains("163cn.tv") {
            self.client
                .get(&input)
                .send()
                .await
                .map_err(|e| ProviderError::Upstream(e.to_string()))?
                .url()
                .to_string()
        } else {
            input
        };
        let parsed = url::Url::parse(&url).map_err(|_| ProviderError::InvalidUrl(raw.into()))?;
        let id = parsed
            .query_pairs()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| v.into_owned())
            .or_else(|| {
                Regex::new(r"[?&]id=(\d+)")
                    .ok()?
                    .captures(&url)
                    .map(|c| c[1].to_string())
            })
            .or_else(|| {
                Regex::new(r"/(?:song|album|playlist|mv)/(\d+)")
                    .ok()?
                    .captures(&url)
                    .map(|c| c[1].to_string())
            })
            .ok_or_else(|| ProviderError::InvalidUrl(raw.into()))?;
        let lower = url.to_ascii_lowercase();
        let kind = if lower.contains("playlist") {
            NeteaseKind::Playlist
        } else if lower.contains("album") {
            NeteaseKind::Album
        } else if lower.contains("/mv") || lower.contains("m=mv") {
            NeteaseKind::Mv
        } else {
            NeteaseKind::Song
        };
        Ok((kind, id, url))
    }
    async fn get(&self, path: &str, params: &[(&str, &str)]) -> ProviderResult<Value> {
        let mut request = self
            .client
            .get(format!("{}{}", self.base, path))
            .query(params);
        if let Some(cookie) = self.credentials.netease().await {
            request = request.query(&[("cookie", cookie)]);
        }
        json_response(
            request
                .send()
                .await
                .map_err(|e| ProviderError::Upstream(e.to_string()))?,
            "api-enhanced",
        )
        .await
    }
}

#[async_trait]
impl Provider for NeteaseProvider {
    fn platform(&self) -> Platform {
        Platform::Netease
    }
    fn can_handle(&self, url: &str) -> bool {
        self.regex.is_match(url)
    }
    async fn parse(&self, request: &ParseRequest) -> ProviderResult<ParsedContent> {
        let (kind, id, resolved) = self.resolve(&request.url).await?;
        match kind {
            NeteaseKind::Song => {
                let detail = self.get("/song/detail", &[("ids", &id)]).await?;
                let song = detail
                    .pointer("/songs/0")
                    .ok_or_else(|| ProviderError::Unavailable(request.url.clone()))?;
                let audio = self
                    .get("/song/url/v1", &[("id", &id), ("level", "standard")])
                    .await?;
                let media_url = get_str(&audio, "/data/0/url")
                    .ok_or_else(|| ProviderError::Unavailable("歌曲无可用播放地址".into()))?;
                let artists = song
                    .get("ar")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.get("name").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join(" / ")
                    })
                    .unwrap_or_default();
                let cover = get_str(song, "/al/picUrl").map(str::to_string);
                let title = get_str(song, "/name").unwrap_or_default().to_string();
                let audio_format = get_str(&audio, "/data/0/type").unwrap_or("mp3");
                Ok(ParsedContent {
                    platform: Platform::Netease,
                    kind: ContentKind::Audio,
                    id: id.clone(),
                    canonical_url: resolved,
                    author: Author {
                        id: String::new(),
                        name: artists,
                        url: None,
                        avatar_url: None,
                    },
                    title: title.clone(),
                    text: get_str(song, "/al/name").unwrap_or_default().into(),
                    sensitive: false,
                    stats: Default::default(),
                    media: vec![MediaItem {
                        kind: MediaKind::Audio,
                        source_url: media_url.into(),
                        fallback_urls: vec![],
                        thumbnail_url: cover,
                        filename: song_filename(&title, &id, audio_format),
                        mime_type: Some(if audio_format == "mp3" {
                            "audio/mpeg".into()
                        } else {
                            format!("audio/{audio_format}")
                        }),
                        duration_secs: get_u64(song, "/dt").map(|n| n / 1000),
                        width: None,
                        height: None,
                        size: get_u64(&audio, "/data/0/size"),
                        headers: Default::default(),
                        // v2 invalidates old Telegram file_ids that were uploaded
                        // before audio title/performer/cover metadata was attached.
                        cache_key: format!("netease:song:{id}:standard:v2"),
                        requires_download: true,
                        secondary_url: None,
                        secondary_fallback_urls: vec![],
                    }],
                    collection_items: Vec::new(),
                })
            }
            NeteaseKind::Album => {
                self.collection(
                    &id,
                    "/album",
                    "/album/name",
                    "/album/picUrl",
                    "/songs",
                    ContentKind::Album,
                )
                .await
            }
            NeteaseKind::Playlist => {
                self.collection(
                    &id,
                    "/playlist/detail",
                    "/playlist/name",
                    "/playlist/coverImgUrl",
                    "/playlist/tracks",
                    ContentKind::Playlist,
                )
                .await
            }
            NeteaseKind::Mv => {
                let detail = self.get("/mv/detail", &[("mvid", &id)]).await?;
                let mv = detail.get("data").unwrap_or(&detail);
                let quality = request.options.quality_or_default().to_string();
                let urls = self.get("/mv/url", &[("id", &id), ("r", &quality)]).await?;
                let media_url = get_str(&urls, "/data/url")
                    .ok_or_else(|| ProviderError::Unavailable(request.url.clone()))?;
                Ok(ParsedContent {
                    platform: Platform::Netease,
                    kind: ContentKind::Video,
                    id: id.clone(),
                    canonical_url: resolved,
                    author: Author {
                        name: get_str(mv, "/artistName").unwrap_or_default().into(),
                        ..Default::default()
                    },
                    title: get_str(mv, "/name").unwrap_or_default().into(),
                    text: get_str(mv, "/desc").unwrap_or_default().into(),
                    sensitive: false,
                    stats: Stats {
                        views: get_u64(mv, "/playCount"),
                        ..Default::default()
                    },
                    media: vec![MediaItem {
                        kind: MediaKind::Video,
                        source_url: media_url.into(),
                        fallback_urls: vec![],
                        thumbnail_url: get_str(mv, "/cover").map(str::to_string),
                        filename: format!("netease-mv-{id}.mp4"),
                        mime_type: Some("video/mp4".into()),
                        duration_secs: get_u64(mv, "/duration").map(|n| n / 1000),
                        width: None,
                        height: None,
                        size: get_u64(&urls, "/data/size"),
                        headers: Default::default(),
                        cache_key: format!("netease:mv:{id}:{quality}"),
                        requires_download: true,
                        secondary_url: None,
                        secondary_fallback_urls: vec![],
                    }],
                    collection_items: Vec::new(),
                })
            }
        }
    }
}

fn song_filename(title: &str, id: &str, format: &str) -> String {
    let mut safe = title
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            {
                '_'
            } else {
                ch
            }
        })
        .take(120)
        .collect::<String>();
    safe = safe.trim_matches([' ', '.']).to_string();
    if safe.is_empty() {
        safe = format!("netease-{id}");
    }
    let extension = match format.to_ascii_lowercase().as_str() {
        "mp3" | "flac" | "m4a" | "aac" | "ogg" | "opus" => format.to_ascii_lowercase(),
        _ => "mp3".into(),
    };
    format!("{safe}.{extension}")
}

impl NeteaseProvider {
    async fn collection(
        &self,
        id: &str,
        path: &str,
        name_ptr: &str,
        cover_ptr: &str,
        tracks_ptr: &str,
        kind: ContentKind,
    ) -> ProviderResult<ParsedContent> {
        let root = self.get(path, &[("id", id)]).await?;
        let tracks = root
            .pointer(tracks_ptr)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .take(30)
                    .map(|s| {
                        let artists = s
                            .get("ar")
                            .or_else(|| s.get("artists"))
                            .and_then(Value::as_array)
                            .map(|x| {
                                x.iter()
                                    .filter_map(|v| v.get("name").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join("/ ")
                            })
                            .unwrap_or_default();
                        format!(
                            "{} — {}",
                            s.get("name").and_then(Value::as_str).unwrap_or(""),
                            artists
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let cover = get_str(&root, cover_ptr).unwrap_or_default();
        Ok(ParsedContent {
            platform: Platform::Netease,
            kind,
            id: id.into(),
            canonical_url: match kind {
                ContentKind::Album => format!("https://music.163.com/#/album?id={id}"),
                _ => format!("https://music.163.com/#/playlist?id={id}"),
            },
            author: Default::default(),
            title: get_str(&root, name_ptr).unwrap_or_default().into(),
            text: String::new(),
            sensitive: false,
            stats: Default::default(),
            media: if cover.is_empty() {
                vec![]
            } else {
                vec![MediaItem {
                    kind: MediaKind::Photo,
                    source_url: cover.into(),
                    fallback_urls: vec![],
                    thumbnail_url: None,
                    filename: format!("netease-collection-{id}.jpg"),
                    mime_type: Some("image/jpeg".into()),
                    duration_secs: None,
                    width: None,
                    height: None,
                    size: None,
                    headers: Default::default(),
                    cache_key: format!("netease:collection:{id}:cover"),
                    requires_download: false,
                    secondary_url: None,
                    secondary_fallback_urls: vec![],
                }]
            },
            collection_items: tracks,
        })
    }
}
