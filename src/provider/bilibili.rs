use std::collections::BTreeMap;

use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use serde_json::Value;

use crate::{Config, RuntimeCredentials, model::*};

use super::{Provider, filename_from_url, get_str, get_u64, json_response};

pub struct BilibiliProvider {
    client: Client,
    credentials: RuntimeCredentials,
    api_base: String,
    live_api_base: String,
    www_base: String,
    matcher: Regex,
    bvid: Regex,
}

impl BilibiliProvider {
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
            credentials,
            api_base: config.bilibili_api_base.trim_end_matches('/').into(),
            live_api_base: config.bilibili_live_api_base.trim_end_matches('/').into(),
            www_base: config.bilibili_www_base.trim_end_matches('/').into(),
            matcher: Regex::new(
                r"(?i)(?:bilibili\.com|b23\.tv|(?:^|\s)BV[0-9A-Za-z]{10}|(?:^|\s)av\d+)",
            )
            .expect("valid Bilibili matcher"),
            bvid: Regex::new(r"(?i)(BV[0-9A-Za-z]{10})").expect("valid BV regex"),
        }
    }
    async fn response(&self, url: &str, params: &[(&str, &str)]) -> ProviderResult<Value> {
        let mut req = self
            .client
            .get(url)
            .query(params)
            .header("Referer", "https://www.bilibili.com/");
        if let Some(cookie) = self.credentials.bilibili().await {
            req = req.header("Cookie", cookie);
        }
        let value = json_response(
            req.send()
                .await
                .map_err(|e| ProviderError::Upstream(e.to_string()))?,
            "Bilibili API",
        )
        .await?;
        if value
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or_default()
            != 0
        {
            return Err(ProviderError::Upstream(format!(
                "Bilibili: {}",
                get_str(&value, "/message").unwrap_or("未知错误")
            )));
        }
        Ok(value)
    }
    async fn resolve(&self, raw: &str) -> ProviderResult<String> {
        if let Some(c) = self.bvid.captures(raw) {
            return Ok(format!("https://www.bilibili.com/video/{}", &c[1]));
        }
        let url = if raw.starts_with("http") {
            raw.to_string()
        } else {
            format!("https://{raw}")
        };
        let response = self
            .client
            .head(&url)
            .send()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        Ok(response.url().to_string())
    }
    async fn headers(&self) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::from([("Referer".into(), "https://www.bilibili.com/".into())]);
        if let Some(cookie) = self.credentials.bilibili().await {
            headers.insert("Cookie".into(), cookie);
        }
        headers
    }

    async fn parse_video(
        &self,
        url: &str,
        options: &ParseOptions,
    ) -> ProviderResult<ParsedContent> {
        if let Some(captures) = Regex::new(r"(?i)(?:ep|ss)(\d+)")
            .expect("valid bangumi regex")
            .captures(url)
        {
            let id = captures[1].to_string();
            let key = if url.to_ascii_lowercase().contains("ss") {
                "season_id"
            } else {
                "ep_id"
            };
            let root = self
                .response(
                    &format!("{}/pgc/view/web/season", self.api_base),
                    &[(key, &id)],
                )
                .await?;
            let episodes = root
                .pointer("/result/episodes")
                .and_then(Value::as_array)
                .ok_or_else(|| ProviderError::Unavailable(url.into()))?;
            let episode = if key == "ep_id" {
                episodes
                    .iter()
                    .find(|e| get_u64(e, "/id").map(|n| n.to_string()) == Some(id.clone()))
                    .or_else(|| episodes.first())
            } else {
                episodes.first()
            }
            .ok_or_else(|| ProviderError::Unavailable(url.into()))?;
            let bvid = get_str(episode, "/bvid")
                .ok_or_else(|| ProviderError::InvalidResponse("番剧缺少 bvid".into()))?;
            return Box::pin(
                self.parse_video(&format!("https://www.bilibili.com/video/{bvid}"), options),
            )
            .await;
        }
        let bvid = self.bvid.captures(url).map(|c| c[1].to_string());
        let aid = Regex::new(r"(?i)(?:/|^)av(\d+)")
            .expect("valid av regex")
            .captures(url)
            .map(|c| c[1].to_string());
        let mut params = Vec::new();
        if let Some(v) = &bvid {
            params.push(("bvid", v.as_str()));
        } else if let Some(v) = &aid {
            params.push(("aid", v.as_str()));
        } else {
            return Err(ProviderError::InvalidUrl(url.into()));
        }
        let root = self
            .response(&format!("{}/x/web-interface/view", self.api_base), &params)
            .await?;
        let data = root
            .get("data")
            .ok_or_else(|| ProviderError::InvalidResponse("视频详情缺少 data".into()))?;
        let cid = get_u64(data, "/cid")
            .ok_or_else(|| ProviderError::InvalidResponse("视频详情缺少 cid".into()))?
            .to_string();
        let actual_bvid = get_str(data, "/bvid")
            .unwrap_or_else(|| bvid.as_deref().unwrap_or(""))
            .to_string();
        let qn = match options.quality.unwrap_or(720) {
            1080.. => "80",
            720..=1079 => "64",
            480..=719 => "32",
            _ => "16",
        };
        let play = self
            .response(
                &format!("{}/x/player/playurl", self.api_base),
                &[
                    ("bvid", &actual_bvid),
                    ("cid", &cid),
                    ("qn", qn),
                    ("fnval", "4048"),
                    ("fourk", "1"),
                ],
            )
            .await?;
        let p = play.get("data").unwrap_or(&Value::Null);
        let mut primary = get_str(p, "/durl/0/url").map(str::to_string);
        let mut secondary = None;
        if primary.is_none() {
            let target = options.quality.unwrap_or(720) as u64;
            primary = p
                .pointer("/dash/video")
                .and_then(Value::as_array)
                .and_then(|streams| {
                    let within_target = || {
                        streams.iter().filter(|stream| {
                            get_u64(stream, "/height").unwrap_or(u64::MAX) <= target
                        })
                    };
                    // Telegram 客户端对 H.264 兼容性最好，优先 AVC，再回退其他编码。
                    within_target()
                        .filter(|stream| {
                            get_u64(stream, "/codecid") == Some(7)
                                || get_str(stream, "/codecs")
                                    .is_some_and(|codec| codec.starts_with("avc"))
                        })
                        .max_by_key(|stream| get_u64(stream, "/height").unwrap_or_default())
                        .or_else(|| {
                            within_target()
                                .max_by_key(|stream| get_u64(stream, "/height").unwrap_or_default())
                        })
                        .or_else(|| streams.first())
                })
                .and_then(|v| get_str(v, "/baseUrl").or_else(|| get_str(v, "/base_url")))
                .map(str::to_string);
            secondary = p
                .pointer("/dash/audio")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|v| get_str(v, "/baseUrl").or_else(|| get_str(v, "/base_url")))
                .map(str::to_string);
        }
        let media_headers = self.headers().await;
        let media = primary
            .map(|source_url| {
                vec![MediaItem {
                    kind: MediaKind::Video,
                    source_url,
                    secondary_url: secondary,
                    thumbnail_url: get_str(data, "/pic").map(str::to_string),
                    filename: format!("bilibili-{actual_bvid}.mp4"),
                    mime_type: Some("video/mp4".into()),
                    duration_secs: get_u64(data, "/duration"),
                    width: get_u64(data, "/dimension/width").map(|n| n as u32),
                    height: get_u64(data, "/dimension/height").map(|n| n as u32),
                    size: get_u64(p, "/durl/0/size"),
                    headers: media_headers,
                    cache_key: format!("bilibili:{actual_bvid}:{qn}"),
                    requires_download: true,
                }]
            })
            .unwrap_or_default();
        Ok(ParsedContent {
            platform: Platform::Bilibili,
            kind: ContentKind::Video,
            id: actual_bvid.clone(),
            canonical_url: format!("https://www.bilibili.com/video/{actual_bvid}"),
            author: Author {
                id: get_u64(data, "/owner/mid")
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                name: get_str(data, "/owner/name").unwrap_or_default().into(),
                url: get_u64(data, "/owner/mid").map(|n| format!("https://space.bilibili.com/{n}")),
                avatar_url: get_str(data, "/owner/face").map(str::to_string),
            },
            title: get_str(data, "/title").unwrap_or_default().into(),
            text: get_str(data, "/desc").unwrap_or_default().into(),
            sensitive: false,
            stats: Stats {
                likes: get_u64(data, "/stat/like"),
                reposts: get_u64(data, "/stat/share"),
                replies: get_u64(data, "/stat/reply"),
                views: get_u64(data, "/stat/view"),
            },
            media,
            collection_items: Vec::new(),
        })
    }
    async fn parse_live(&self, url: &str) -> ProviderResult<ParsedContent> {
        let id = Regex::new(r"live\.bilibili\.com/(?:blanc/)?(\d+)")
            .expect("valid live regex")
            .captures(url)
            .map(|c| c[1].to_string())
            .ok_or_else(|| ProviderError::InvalidUrl(url.into()))?;
        let root = self
            .response(
                &format!(
                    "{}/xlive/web-room/v1/index/getInfoByRoom",
                    self.live_api_base
                ),
                &[("room_id", &id)],
            )
            .await?;
        let room = root.pointer("/data/room_info").unwrap_or(&Value::Null);
        let anchor = root
            .pointer("/data/anchor_info/base_info")
            .unwrap_or(&Value::Null);
        let cover = get_str(room, "/cover").or_else(|| get_str(room, "/keyframe"));
        let media_headers = self.headers().await;
        Ok(ParsedContent {
            platform: Platform::Bilibili,
            kind: ContentKind::Live,
            id: id.clone(),
            canonical_url: format!("https://live.bilibili.com/{id}"),
            author: Author {
                id: get_u64(room, "/uid")
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                name: get_str(anchor, "/uname").unwrap_or_default().into(),
                url: get_u64(room, "/uid").map(|n| format!("https://space.bilibili.com/{n}")),
                avatar_url: get_str(anchor, "/face").map(str::to_string),
            },
            title: get_str(room, "/title").unwrap_or_default().into(),
            text: get_str(room, "/description").unwrap_or_default().into(),
            sensitive: false,
            stats: Stats {
                views: get_u64(room, "/online"),
                ..Default::default()
            },
            media: cover
                .map(|u| {
                    vec![MediaItem {
                        kind: MediaKind::Photo,
                        source_url: u.into(),
                        thumbnail_url: None,
                        filename: format!("bilibili-live-{id}.jpg"),
                        mime_type: None,
                        duration_secs: None,
                        width: None,
                        height: None,
                        size: None,
                        headers: media_headers,
                        cache_key: format!("bilibili:live:{id}:cover"),
                        requires_download: false,
                        secondary_url: None,
                    }]
                })
                .unwrap_or_default(),
            collection_items: Vec::new(),
        })
    }
    async fn parse_dynamic(&self, url: &str) -> ProviderResult<ParsedContent> {
        let id = Regex::new(r"(?:(?:opus|dynamic)/|t\.bilibili\.com/)(\d+)")
            .expect("valid dynamic regex")
            .captures(url)
            .map(|c| c[1].to_string())
            .ok_or_else(|| ProviderError::InvalidUrl(url.into()))?;
        let root = self
            .response(
                &format!("{}/x/polymer/web-dynamic/v1/detail", self.api_base),
                &[("id", &id)],
            )
            .await?;
        let item = root.pointer("/data/item").unwrap_or(&Value::Null);
        let author = item
            .pointer("/modules/module_author")
            .unwrap_or(&Value::Null);
        let dynamic = item
            .pointer("/modules/module_dynamic")
            .unwrap_or(&Value::Null);
        let mut media = Vec::new();
        if let Some(items) = dynamic
            .pointer("/major/opus/pics")
            .or_else(|| dynamic.pointer("/major/draw/items"))
            .and_then(Value::as_array)
        {
            for (i, p) in items.iter().enumerate() {
                if let Some(u) = get_str(p, "/url").or_else(|| get_str(p, "/src")) {
                    media.push(MediaItem {
                        kind: MediaKind::Photo,
                        source_url: u.into(),
                        thumbnail_url: None,
                        filename: filename_from_url(u, &format!("bilibili-opus-{id}-{i}.jpg")),
                        mime_type: None,
                        duration_secs: None,
                        width: get_u64(p, "/width").map(|n| n as u32),
                        height: get_u64(p, "/height").map(|n| n as u32),
                        size: None,
                        headers: self.headers().await,
                        cache_key: format!("bilibili:opus:{id}:{i}"),
                        requires_download: false,
                        secondary_url: None,
                    });
                }
            }
        }
        Ok(ParsedContent {
            platform: Platform::Bilibili,
            kind: ContentKind::Post,
            id: id.clone(),
            canonical_url: format!("https://www.bilibili.com/opus/{id}"),
            author: Author {
                id: get_u64(author, "/mid")
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                name: get_str(author, "/name").unwrap_or_default().into(),
                url: get_u64(author, "/mid").map(|n| format!("https://space.bilibili.com/{n}")),
                avatar_url: get_str(author, "/face").map(str::to_string),
            },
            title: get_str(dynamic, "/major/opus/title")
                .unwrap_or_default()
                .into(),
            text: get_str(dynamic, "/desc/text")
                .or_else(|| get_str(dynamic, "/major/opus/summary/text"))
                .unwrap_or_default()
                .into(),
            sensitive: false,
            stats: Stats {
                likes: get_u64(item, "/modules/module_stat/like/count"),
                reposts: get_u64(item, "/modules/module_stat/forward/count"),
                replies: get_u64(item, "/modules/module_stat/comment/count"),
                views: None,
            },
            media,
            collection_items: Vec::new(),
        })
    }

    async fn parse_audio(&self, url: &str) -> ProviderResult<ParsedContent> {
        let id = Regex::new(r"/audio/au(\d+)")
            .expect("valid audio regex")
            .captures(url)
            .map(|c| c[1].to_string())
            .ok_or_else(|| ProviderError::InvalidUrl(url.into()))?;
        let root = self
            .response(
                &format!("{}/audio/music-service-c/web/song/info", self.www_base),
                &[("sid", &id)],
            )
            .await?;
        let song = root.get("data").unwrap_or(&Value::Null);
        let stream = self
            .response(
                &format!("{}/audio/music-service-c/web/url", self.www_base),
                &[("sid", &id), ("quality", "2"), ("privilege", "2")],
            )
            .await?;
        let source = get_str(&stream, "/data/cdns/0")
            .ok_or_else(|| ProviderError::Unavailable(url.into()))?;
        Ok(ParsedContent {
            platform: Platform::Bilibili,
            kind: ContentKind::Audio,
            id: id.clone(),
            canonical_url: format!("https://www.bilibili.com/audio/au{id}"),
            author: Author {
                id: get_u64(song, "/uid")
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                name: get_str(song, "/uname").unwrap_or_default().into(),
                url: get_u64(song, "/uid").map(|n| format!("https://space.bilibili.com/{n}")),
                avatar_url: None,
            },
            title: get_str(song, "/title").unwrap_or_default().into(),
            text: get_str(song, "/intro").unwrap_or_default().into(),
            sensitive: false,
            stats: Stats {
                replies: get_u64(song, "/comment"),
                views: get_u64(song, "/passtime"),
                ..Default::default()
            },
            media: vec![MediaItem {
                kind: MediaKind::Audio,
                source_url: source.into(),
                thumbnail_url: get_str(song, "/cover").map(str::to_string),
                filename: format!("bilibili-au{id}.m4a"),
                mime_type: Some("audio/mp4".into()),
                duration_secs: get_u64(song, "/duration"),
                width: None,
                height: None,
                size: get_u64(song, "/size"),
                headers: self.headers().await,
                cache_key: format!("bilibili:audio:{id}"),
                requires_download: true,
                secondary_url: None,
            }],
            collection_items: Vec::new(),
        })
    }

    async fn parse_article(&self, url: &str) -> ProviderResult<ParsedContent> {
        let id = Regex::new(r"/read/(?:cv|mobile/|mobile\?id=)(\d+)")
            .expect("valid article regex")
            .captures(url)
            .map(|c| c[1].to_string())
            .ok_or_else(|| ProviderError::InvalidUrl(url.into()))?;
        let root = self
            .response(
                &format!("{}/x/article/viewinfo", self.api_base),
                &[("id", &id), ("mobi_app", "pc"), ("from", "web")],
            )
            .await?;
        let data = root.get("data").unwrap_or(&Value::Null);
        let media_headers = self.headers().await;
        Ok(ParsedContent {
            platform: Platform::Bilibili,
            kind: ContentKind::Article,
            id: id.clone(),
            canonical_url: format!("https://www.bilibili.com/read/cv{id}"),
            author: Author {
                id: get_u64(data, "/mid")
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                name: get_str(data, "/author_name").unwrap_or_default().into(),
                url: get_u64(data, "/mid").map(|n| format!("https://space.bilibili.com/{n}")),
                avatar_url: get_str(data, "/origin_image_urls/0").map(str::to_string),
            },
            title: get_str(data, "/title").unwrap_or_default().into(),
            text: get_str(data, "/summary").unwrap_or_default().into(),
            sensitive: false,
            stats: Stats {
                likes: get_u64(data, "/stats/like"),
                replies: get_u64(data, "/stats/reply"),
                views: get_u64(data, "/stats/view"),
                ..Default::default()
            },
            media: get_str(data, "/banner_url")
                .map(|u| {
                    vec![MediaItem {
                        kind: MediaKind::Photo,
                        source_url: u.into(),
                        thumbnail_url: None,
                        filename: format!("bilibili-cv{id}.jpg"),
                        mime_type: None,
                        duration_secs: None,
                        width: None,
                        height: None,
                        size: None,
                        headers: media_headers,
                        cache_key: format!("bilibili:article:{id}:cover"),
                        requires_download: false,
                        secondary_url: None,
                    }]
                })
                .unwrap_or_default(),
            collection_items: Vec::new(),
        })
    }
}

#[async_trait]
impl Provider for BilibiliProvider {
    fn platform(&self) -> Platform {
        Platform::Bilibili
    }
    fn can_handle(&self, url: &str) -> bool {
        self.matcher.is_match(url)
    }
    async fn parse(&self, request: &ParseRequest) -> ProviderResult<ParsedContent> {
        let url = self.resolve(&request.url).await?;
        if url.contains("live.bilibili.com") {
            self.parse_live(&url).await
        } else if url.contains("/opus/")
            || url.contains("/dynamic/")
            || url.contains("t.bilibili.com")
        {
            self.parse_dynamic(&url).await
        } else if url.contains("/audio/au") {
            self.parse_audio(&url).await
        } else if url.contains("/read/") {
            self.parse_article(&url).await
        } else {
            self.parse_video(&url, &request.options).await
        }
    }
}
