use std::{
    collections::{BTreeMap, HashSet},
    sync::OnceLock,
};

use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use serde_json::Value;

use crate::{BilibiliCdnPreference, Config, RuntimeCredentials, model::*};

use super::{Provider, filename_from_url, get_str, get_u64, json_response};

#[derive(Debug, Clone)]
enum BilibiliTarget {
    Video(String),
    Bangumi(String),
    Dynamic(String),
    Live(String),
    Audio(String),
    Article(String),
}

const OPUS_FEATURES: &str = "itemOpusStyle,opusBigCover,onlyfansVote,endFooterHidden,decorationCard,onlyfansAssetsV2,ugcDelete,onlyfansQaCard,commentsNewVersion";

pub struct BilibiliProvider {
    client: Client,
    credentials: RuntimeCredentials,
    api_base: String,
    live_api_base: String,
    www_base: String,
    max_media_size: u64,
    cdn: BilibiliCdnPreference,
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
            max_media_size: if config.local_bot_api {
                2_000_000_000
            } else {
                50 * 1024 * 1024
            },
            cdn: config.bilibili_cdn,
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
        let code = value
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        if code != 0 {
            let message = get_str(&value, "/message").unwrap_or("未知错误");
            return Err(match code {
                -101 => ProviderError::Authentication(format!("Bilibili: {message}")),
                -352 | 412 => ProviderError::RateLimited(format!("Bilibili: {message}")),
                404 | 4101139 => ProviderError::Unavailable(format!("Bilibili: {message}")),
                _ => ProviderError::Upstream(format!("Bilibili: {message}")),
            });
        }
        Ok(value)
    }
    async fn resolve(&self, raw: &str) -> ProviderResult<String> {
        if let Some(c) = bare_bvid_regex().captures(raw.trim()) {
            return Ok(format!("https://www.bilibili.com/video/{}", &c[0]));
        }
        if let Some(c) = bare_av_regex().captures(raw.trim()) {
            return Ok(format!("https://www.bilibili.com/video/av{}", &c[1]));
        }
        let url = if raw.starts_with("http://") || raw.starts_with("https://") {
            raw.to_string()
        } else {
            format!("https://{raw}")
        };
        let parsed = url::Url::parse(&url).map_err(|_| ProviderError::InvalidUrl(raw.into()))?;
        let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
        if is_bilibili_host(&host) && host != "b23.tv" {
            return Ok(parsed.to_string());
        }
        if host != "b23.tv" {
            return Err(ProviderError::InvalidUrl(raw.into()));
        }
        let response = self
            .client
            .head(&url)
            .send()
            .await
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        let resolved = response.url().clone();
        let resolved_host = resolved.host_str().unwrap_or_default().to_ascii_lowercase();
        if !is_bilibili_host(&resolved_host) || resolved_host == "b23.tv" {
            return Err(ProviderError::InvalidUrl(raw.into()));
        }
        Ok(resolved.to_string())
    }

    fn target(&self, url: &str) -> ProviderResult<BilibiliTarget> {
        let parsed = url::Url::parse(url).map_err(|_| ProviderError::InvalidUrl(url.into()))?;
        let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
        let segments: Vec<_> = parsed.path_segments().into_iter().flatten().collect();
        if host == "live.bilibili.com" {
            let room = segments
                .iter()
                .find(|segment| segment.chars().all(|c| c.is_ascii_digit()));
            return room
                .map(|room| BilibiliTarget::Live((*room).to_string()))
                .ok_or_else(|| ProviderError::InvalidUrl(url.into()));
        }
        if host == "t.bilibili.com" {
            return segments
                .first()
                .filter(|id| id.chars().all(|c| c.is_ascii_digit()))
                .map(|id| BilibiliTarget::Dynamic((*id).to_string()))
                .ok_or_else(|| ProviderError::InvalidUrl(url.into()));
        }
        for (index, segment) in segments.iter().enumerate() {
            let next = segments.get(index + 1).copied();
            match segment.to_ascii_lowercase().as_str() {
                "opus" | "dynamic"
                    if next.is_some_and(|id| id.chars().all(|c| c.is_ascii_digit())) =>
                {
                    return Ok(BilibiliTarget::Dynamic(next.unwrap().to_string()));
                }
                "video"
                    if next.is_some_and(|id| {
                        self.bvid.is_match(id) || av_path_regex().is_match(id)
                    }) =>
                {
                    return Ok(BilibiliTarget::Video(next.unwrap().to_string()));
                }
                "play" if next.is_some_and(|id| ep_path_regex().is_match(id)) => {
                    return Ok(BilibiliTarget::Bangumi(next.unwrap().to_string()));
                }
                "audio" if next.is_some_and(|id| audio_path_regex().is_match(id)) => {
                    return Ok(BilibiliTarget::Audio(next.unwrap().to_string()));
                }
                "read"
                    if next == Some("mobile")
                        && segments
                            .get(index + 2)
                            .is_some_and(|id| id.chars().all(|c| c.is_ascii_digit())) =>
                {
                    return Ok(BilibiliTarget::Article(format!(
                        "mobile/{}",
                        segments[index + 2]
                    )));
                }
                "read" if next == Some("mobile") => {
                    let id = parsed
                        .query_pairs()
                        .find(|(name, _)| name == "id")
                        .map(|(_, value)| value.into_owned())
                        .ok_or_else(|| ProviderError::InvalidUrl(url.into()))?;
                    return Ok(BilibiliTarget::Article(format!("mobile?id={id}")));
                }
                "read" if next.is_some_and(|id| article_path_regex().is_match(id)) => {
                    return Ok(BilibiliTarget::Article(next.unwrap().to_string()));
                }
                _ => {}
            }
        }
        Err(ProviderError::Unsupported(url.into()))
    }
    async fn headers(&self) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::from([("Referer".into(), "https://www.bilibili.com/".into())]);
        if let Some(cookie) = self.credentials.bilibili().await {
            headers.insert("Cookie".into(), cookie);
        }
        headers
    }

    fn order_media_urls(&self, primary: String, backups: Vec<String>) -> (String, Vec<String>) {
        let mut urls: Vec<String> = match self.cdn {
            BilibiliCdnPreference::BaseUrl => {
                std::iter::once(primary.clone()).chain(backups).collect()
            }
            BilibiliCdnPreference::BackupUrl => backups
                .into_iter()
                .chain(std::iter::once(primary.clone()))
                .collect(),
            BilibiliCdnPreference::Mirror(host) => rewrite_cdn_host(&primary, host)
                .into_iter()
                .chain(std::iter::once(primary.clone()))
                .chain(backups)
                .collect(),
        };
        let mut seen = HashSet::new();
        urls.retain(|url| seen.insert(url.clone()));
        let selected = urls.first().cloned().unwrap_or(primary);
        let fallbacks = urls.into_iter().skip(1).collect();
        (selected, fallbacks)
    }

    async fn parse_video(
        &self,
        url: &str,
        options: &ParseOptions,
    ) -> ProviderResult<ParsedContent> {
        // ep/ss 必须是独立路径段，否则会命中 BV1ss411… 这类普通视频的 BV 号。
        if let Some(captures) = Regex::new(r"(?i)/(ep|ss)(\d+)(?:[/?#]|$)")
            .expect("valid bangumi regex")
            .captures(url)
        {
            let id = captures[2].to_string();
            let key = if captures[1].eq_ignore_ascii_case("ss") {
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
        let qn = match options.quality_or_default() {
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
        let mut fallback_urls = string_array(p.pointer("/durl/0/backup_url"));
        let mut secondary = None;
        let mut secondary_fallback_urls = Vec::new();
        let mut selected_width = None;
        let mut selected_height = None;
        let mut selected_size = get_u64(p, "/durl/0/size");
        if primary.is_none() {
            let target = options.quality_or_default() as u64;
            let duration = get_u64(p, "/dash/duration").or_else(|| get_u64(data, "/duration"));
            let audio_bandwidth = get_u64(p, "/dash/audio/0/bandwidth").unwrap_or_default();
            let selected_video =
                p.pointer("/dash/video")
                    .and_then(Value::as_array)
                    .and_then(|streams| {
                        let within_target = || {
                            streams.iter().filter(|stream| {
                                get_u64(stream, "/height").unwrap_or(u64::MAX) <= target
                                    && estimated_stream_size(stream, audio_bandwidth, duration)
                                        .is_none_or(|size| size <= self.max_media_size)
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
                                within_target().max_by_key(|stream| {
                                    get_u64(stream, "/height").unwrap_or_default()
                                })
                            })
                            .or_else(|| {
                                streams
                                    .iter()
                                    .filter(|stream| {
                                        get_u64(stream, "/codecid") == Some(7)
                                            || get_str(stream, "/codecs")
                                                .is_some_and(|codec| codec.starts_with("avc"))
                                    })
                                    .min_by_key(|stream| {
                                        get_u64(stream, "/height").unwrap_or(u64::MAX)
                                    })
                            })
                            .or_else(|| {
                                streams.iter().min_by_key(|stream| {
                                    get_u64(stream, "/height").unwrap_or(u64::MAX)
                                })
                            })
                    });
            if let Some(stream) = selected_video {
                primary = get_str(stream, "/baseUrl")
                    .or_else(|| get_str(stream, "/base_url"))
                    .map(str::to_string);
                fallback_urls =
                    string_array(stream.get("backupUrl").or_else(|| stream.get("backup_url")));
                selected_width = get_u64(stream, "/width").map(|value| value as u32);
                selected_height = get_u64(stream, "/height").map(|value| value as u32);
                selected_size = get_u64(stream, "/size");
            }
            if let Some(audio) = p
                .pointer("/dash/audio")
                .and_then(Value::as_array)
                .and_then(|audio| audio.first())
            {
                secondary = get_str(audio, "/baseUrl")
                    .or_else(|| get_str(audio, "/base_url"))
                    .map(str::to_string);
                secondary_fallback_urls =
                    string_array(audio.get("backupUrl").or_else(|| audio.get("backup_url")));
            }
        }
        if let Some(url) = primary.take() {
            let (selected, fallbacks) = self.order_media_urls(url, fallback_urls);
            primary = Some(selected);
            fallback_urls = fallbacks;
        }
        if let Some(url) = secondary.take() {
            let (selected, fallbacks) = self.order_media_urls(url, secondary_fallback_urls);
            secondary = Some(selected);
            secondary_fallback_urls = fallbacks;
        }
        let selected_quality = selected_height.unwrap_or(options.quality_or_default());
        let media_headers = self.headers().await;
        let media = primary
            .map(|source_url| {
                vec![MediaItem {
                    kind: MediaKind::Video,
                    source_url,
                    fallback_urls,
                    secondary_url: secondary,
                    thumbnail_url: get_str(data, "/pic").map(str::to_string),
                    filename: format!("bilibili-{actual_bvid}.mp4"),
                    mime_type: Some("video/mp4".into()),
                    duration_secs: get_u64(data, "/duration"),
                    width: selected_width
                        .or_else(|| get_u64(data, "/dimension/width").map(|n| n as u32)),
                    height: selected_height
                        .or_else(|| get_u64(data, "/dimension/height").map(|n| n as u32)),
                    size: selected_size,
                    headers: media_headers,
                    cache_key: format!("bilibili:{actual_bvid}:{qn}:{selected_quality}p"),
                    requires_download: true,
                    secondary_fallback_urls,
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
                        fallback_urls: vec![],
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
                        secondary_fallback_urls: vec![],
                    }]
                })
                .unwrap_or_default(),
            collection_items: Vec::new(),
        })
    }
    async fn parse_dynamic(
        &self,
        url: &str,
        id: &str,
        options: &ParseOptions,
    ) -> ProviderResult<ParsedContent> {
        let root = self
            .response(
                &format!("{}/x/polymer/web-dynamic/v1/detail", self.api_base),
                &[("id", id), ("features", OPUS_FEATURES)],
            )
            .await?;
        let item = root
            .pointer("/data/item")
            .filter(|value| !value.is_null())
            .ok_or_else(|| ProviderError::Unavailable(url.into()))?;
        let outer_dynamic = item
            .pointer("/modules/module_dynamic")
            .unwrap_or(&Value::Null);
        let outer_text = dynamic_text(outer_dynamic);
        let is_forward = get_str(item, "/type") == Some("DYNAMIC_TYPE_FORWARD")
            || item.get("orig").is_some_and(|value| !value.is_null());
        let source_item = item
            .get("orig")
            .filter(|value| !value.is_null())
            .unwrap_or(item);
        let source_dynamic = source_item
            .pointer("/modules/module_dynamic")
            .unwrap_or(&Value::Null);
        let source_major = source_dynamic.pointer("/major").unwrap_or(&Value::Null);
        let source_id = get_str(source_item, "/id_str")
            .or_else(|| get_str(source_item, "/basic/dyn_id_str"))
            .unwrap_or(id);
        let source_major_type = get_str(source_major, "/type").unwrap_or_default();
        let headers = self.headers().await;
        let mut title = major_title(source_major);
        let mut text = dynamic_text(source_dynamic);
        let mut pictures = pictures_from_dynamic(source_dynamic);

        if source_major_type == "MAJOR_TYPE_OPUS"
            && let Ok(projection) = self.parse_opus_detail(source_id).await
        {
            if !projection.title.is_empty() {
                title = projection.title;
            }
            if !projection.text.is_empty() {
                text = projection.text;
            }
            if !projection.pictures.is_empty() {
                pictures = projection.pictures;
            }
        }

        let mut media = media_from_pictures(source_id, pictures, &headers);
        if let Some(target_url) = embedded_url(source_major)
            && let Ok(target) = self.target(&target_url)
        {
            let embedded = match target {
                BilibiliTarget::Video(video) => {
                    self.parse_video(&format!("https://www.bilibili.com/video/{video}"), options)
                        .await
                }
                BilibiliTarget::Bangumi(episode) => {
                    self.parse_video(
                        &format!("https://www.bilibili.com/bangumi/play/{episode}"),
                        options,
                    )
                    .await
                }
                BilibiliTarget::Live(room) => {
                    self.parse_live(&format!("https://live.bilibili.com/{room}"))
                        .await
                }
                BilibiliTarget::Audio(audio) => {
                    self.parse_audio(&format!("https://www.bilibili.com/audio/{audio}"))
                        .await
                }
                BilibiliTarget::Article(article) => {
                    self.parse_article(&format!("https://www.bilibili.com/read/{article}"))
                        .await
                }
                BilibiliTarget::Dynamic(_) => Err(ProviderError::Unsupported(target_url)),
            };
            if let Ok(embedded) = embedded {
                if title.is_empty() {
                    title = embedded.title;
                }
                media.extend(embedded.media);
            }
        }
        if media.is_empty()
            && let Some(cover) = major_cover(source_major)
        {
            media.push(photo_media(source_id, 0, &cover, None, None, &headers));
        }

        if is_forward {
            let origin_author = author_from_item(source_item);
            let origin = if text.is_empty() {
                "原动态已删除或不可见".to_string()
            } else if origin_author.name.is_empty() {
                text.clone()
            } else {
                format!("转发自 {}：\n{}", origin_author.name, text)
            };
            text = if outer_text.is_empty() {
                origin
            } else {
                format!("{outer_text}\n\n{origin}")
            };
        }

        Ok(ParsedContent {
            platform: Platform::Bilibili,
            kind: ContentKind::Post,
            id: id.to_string(),
            canonical_url: format!("https://www.bilibili.com/opus/{id}"),
            author: author_from_item(item),
            title,
            text,
            sensitive: false,
            stats: stats_from_item(item),
            media,
            collection_items: Vec::new(),
        })
    }

    async fn parse_opus_detail(&self, id: &str) -> ProviderResult<OpusProjection> {
        self.ensure_buvid3().await;
        let root = self
            .response(
                &format!("{}/x/polymer/web-dynamic/v1/opus/detail", self.api_base),
                &[
                    ("id", id),
                    ("features", OPUS_FEATURES),
                    ("timezone_offset", "-480"),
                ],
            )
            .await?;
        let item = root
            .pointer("/data/item")
            .ok_or_else(|| ProviderError::InvalidResponse("Opus 详情缺少 item".into()))?;
        let modules = item
            .get("modules")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::InvalidResponse("Opus 详情缺少 modules".into()))?;
        let mut title = get_str(item, "/basic/title")
            .unwrap_or_default()
            .to_string();
        let mut paragraphs = Vec::new();
        let mut pictures = Vec::new();
        for module in modules {
            if title.is_empty() {
                title = get_str(module, "/module_title/text")
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
                    if let Some(items) = paragraph.pointer("/pic/pics").and_then(Value::as_array) {
                        pictures.extend(items.iter().filter_map(dynamic_picture));
                    }
                }
            }
        }
        Ok(OpusProjection {
            title,
            text: paragraphs.join("\n\n"),
            pictures,
        })
    }

    async fn ensure_buvid3(&self) {
        let current = self.credentials.bilibili().await;
        if current
            .as_deref()
            .is_some_and(|cookie| cookie_has_value(cookie, "buvid3"))
        {
            return;
        }
        let response = match self
            .client
            .get(format!("{}/x/frontend/finger/spi", self.api_base))
            .header("Referer", "https://www.bilibili.com/")
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return,
        };
        let value = match json_response(response, "Bilibili device API").await {
            Ok(value) => value,
            Err(_) => return,
        };
        let Some(buvid3) = get_str(&value, "/data/b_3").filter(|value| !value.is_empty()) else {
            return;
        };
        let buvid4 = get_str(&value, "/data/b_4").filter(|value| !value.is_empty());
        let mut values = parse_cookie_pairs(current.as_deref().unwrap_or_default());
        values.insert("buvid3".into(), buvid3.into());
        if let Some(buvid4) = buvid4 {
            values.insert("buvid4".into(), buvid4.into());
        }
        let cookie = values
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        let _ = self.credentials.set_bilibili(cookie).await;
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
        let mut sources = string_array(stream.pointer("/data/cdns"));
        if sources.is_empty() {
            return Err(ProviderError::Unavailable(url.into()));
        }
        let source = sources.remove(0);
        let (source, fallback_urls) = self.order_media_urls(source, sources);
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
                source_url: source,
                fallback_urls,
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
                secondary_fallback_urls: vec![],
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
                        fallback_urls: vec![],
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
                        secondary_fallback_urls: vec![],
                    }]
                })
                .unwrap_or_default(),
            collection_items: Vec::new(),
        })
    }
}

#[derive(Debug, Clone)]
struct DynamicPicture {
    url: String,
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Debug, Default)]
struct OpusProjection {
    title: String,
    text: String,
    pictures: Vec<DynamicPicture>,
}

fn is_bilibili_host(host: &str) -> bool {
    host == "bilibili.com" || host.ends_with(".bilibili.com") || host == "b23.tv"
}

fn bare_bvid_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX
        .get_or_init(|| Regex::new(r"(?i)^BV[0-9A-Za-z]{10}(?:\?|$)").expect("valid bare BV regex"))
}

fn bare_av_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?i)^av(\d{2,})(?:\?|$)").expect("valid bare av regex"))
}

fn av_path_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?i)^av\d+$").expect("valid av path regex"))
}

fn ep_path_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?i)^(?:ep|ss)\d+$").expect("valid episode regex"))
}

fn audio_path_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?i)^au\d+$").expect("valid audio path regex"))
}

fn article_path_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?i)^(?:cv\d+|mobile)$").expect("valid article path regex"))
}

fn author_from_item(item: &Value) -> Author {
    let value = item
        .pointer("/modules/module_author")
        .and_then(|author| author.get("author").or(Some(author)))
        .unwrap_or(&Value::Null);
    let mid = get_u64(value, "/mid").or_else(|| get_u64(item, "/basic/uid"));
    Author {
        id: mid.map(|value| value.to_string()).unwrap_or_default(),
        name: get_str(value, "/name")
            .or_else(|| get_str(value, "/uname"))
            .unwrap_or_default()
            .into(),
        url: mid.map(|value| format!("https://space.bilibili.com/{value}")),
        avatar_url: get_str(value, "/face")
            .or_else(|| get_str(value, "/avatar"))
            .map(str::to_string),
    }
}

fn stats_from_item(item: &Value) -> Stats {
    Stats {
        likes: get_u64(item, "/modules/module_stat/like/count"),
        reposts: get_u64(item, "/modules/module_stat/forward/count"),
        replies: get_u64(item, "/modules/module_stat/comment/count"),
        views: get_u64(item, "/modules/module_stat/view/count"),
    }
}

fn dynamic_text(dynamic: &Value) -> String {
    let text = get_str(dynamic, "/desc/text")
        .or_else(|| get_str(dynamic, "/major/opus/summary/text"))
        .unwrap_or_default();
    if !text.is_empty() {
        return text.to_string();
    }
    dynamic
        .pointer("/desc/rich_text_nodes")
        .and_then(Value::as_array)
        .map(|nodes| render_rich_text_nodes(nodes))
        .unwrap_or_default()
}

fn render_rich_text_nodes(nodes: &[Value]) -> String {
    nodes
        .iter()
        .filter_map(|node| {
            get_str(node, "/text")
                .or_else(|| get_str(node, "/orig_text"))
                .or_else(|| get_str(node, "/emoji/text"))
        })
        .collect()
}

fn major_title(major: &Value) -> String {
    let title = [
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
    .find_map(|pointer| get_str(major, pointer).filter(|value| !value.is_empty()));
    if let Some(title) = title {
        return title.to_string();
    }
    live_rcmd_value(major)
        .as_ref()
        .and_then(|value| {
            ["/title", "/room_info/title"]
                .into_iter()
                .find_map(|pointer| get_str(value, pointer))
        })
        .unwrap_or_default()
        .to_string()
}

fn major_cover(major: &Value) -> Option<String> {
    let cover = [
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
    .find_map(|pointer| get_str(major, pointer).and_then(normalize_asset_url));
    cover.or_else(|| {
        live_rcmd_value(major).as_ref().and_then(|value| {
            ["/cover", "/keyframe", "/room_info/cover"]
                .into_iter()
                .find_map(|pointer| get_str(value, pointer).and_then(normalize_asset_url))
        })
    })
}

fn live_rcmd_value(major: &Value) -> Option<Value> {
    get_str(major, "/live_rcmd/content").and_then(|content| serde_json::from_str(content).ok())
}

fn pictures_from_dynamic(dynamic: &Value) -> Vec<DynamicPicture> {
    dynamic
        .pointer("/major/opus/pics")
        .or_else(|| dynamic.pointer("/major/draw/items"))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(dynamic_picture).collect())
        .unwrap_or_default()
}

fn dynamic_picture(value: &Value) -> Option<DynamicPicture> {
    let url = get_str(value, "/url")
        .or_else(|| get_str(value, "/src"))
        .or_else(|| get_str(value, "/live_url"))
        .and_then(normalize_asset_url)?;
    Some(DynamicPicture {
        url,
        width: get_u64(value, "/width").and_then(|value| u32::try_from(value).ok()),
        height: get_u64(value, "/height").and_then(|value| u32::try_from(value).ok()),
    })
}

fn normalize_asset_url(value: &str) -> Option<String> {
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

fn media_from_pictures(
    id: &str,
    pictures: Vec<DynamicPicture>,
    headers: &BTreeMap<String, String>,
) -> Vec<MediaItem> {
    let mut seen = HashSet::new();
    pictures
        .into_iter()
        .filter(|picture| seen.insert(picture.url.clone()))
        .enumerate()
        .map(|(index, picture)| {
            photo_media(
                id,
                index,
                &picture.url,
                picture.width,
                picture.height,
                headers,
            )
        })
        .collect()
}

fn photo_media(
    id: &str,
    index: usize,
    url: &str,
    width: Option<u32>,
    height: Option<u32>,
    headers: &BTreeMap<String, String>,
) -> MediaItem {
    MediaItem {
        kind: MediaKind::Photo,
        source_url: url.to_string(),
        fallback_urls: Vec::new(),
        thumbnail_url: None,
        filename: filename_from_url(url, &format!("bilibili-dynamic-{id}-{index}.jpg")),
        mime_type: None,
        duration_secs: None,
        width,
        height,
        size: None,
        headers: headers.clone(),
        cache_key: format!("bilibili:dynamic:{id}:photo:{index}"),
        requires_download: true,
        secondary_url: None,
        secondary_fallback_urls: Vec::new(),
    }
}

fn embedded_url(major: &Value) -> Option<String> {
    let archive = major.get("archive").unwrap_or(&Value::Null);
    if let Some(bvid) = get_str(archive, "/bvid") {
        return Some(format!("https://www.bilibili.com/video/{bvid}"));
    }
    if let Some(aid) = get_u64(archive, "/aid") {
        return Some(format!("https://www.bilibili.com/video/av{aid}"));
    }
    for pointer in [
        "/archive/jump_url",
        "/pgc/jump_url",
        "/article/jump_url",
        "/music/jump_url",
        "/live/jump_url",
        "/common/jump_url",
        "/ugc_season/jump_url",
    ] {
        if let Some(url) = get_str(major, pointer).and_then(normalize_asset_url) {
            return Some(url);
        }
    }
    if let Some(auid) = get_u64(major, "/music/id") {
        return Some(format!("https://www.bilibili.com/audio/au{auid}"));
    }
    if let Some(article_id) = get_u64(major, "/article/id") {
        return Some(format!("https://www.bilibili.com/read/cv{article_id}"));
    }
    if let Some(room_id) = get_u64(major, "/live/room_id").or_else(|| get_u64(major, "/live/id")) {
        return Some(format!("https://live.bilibili.com/{room_id}"));
    }
    if let Some(value) = live_rcmd_value(major)
        && let Some(room_id) = get_u64(&value, "/room_id")
            .or_else(|| get_u64(&value, "/live_id"))
            .or_else(|| get_u64(&value, "/room_info/room_id"))
    {
        return Some(format!("https://live.bilibili.com/{room_id}"));
    }
    None
}

fn opus_paragraph_text(paragraph: &Value) -> String {
    if let Some(nodes) = paragraph.pointer("/text/nodes").and_then(Value::as_array) {
        let text = nodes
            .iter()
            .filter_map(|node| {
                get_str(node, "/word/words")
                    .or_else(|| get_str(node, "/rich/text"))
                    .or_else(|| get_str(node, "/rich/orig_text"))
            })
            .collect::<String>();
        if !text.is_empty() {
            return text;
        }
    }
    if let Some(content) = get_str(paragraph, "/code/content") {
        return content.to_string();
    }
    if let Some(items) = paragraph.pointer("/list/items").and_then(Value::as_array) {
        return items
            .iter()
            .filter_map(|item| {
                item.pointer("/nodes")
                    .and_then(Value::as_array)
                    .map(|nodes| {
                        nodes
                            .iter()
                            .filter_map(|node| {
                                get_str(node, "/word/words").or_else(|| get_str(node, "/rich/text"))
                            })
                            .collect::<String>()
                    })
            })
            .filter(|text| !text.is_empty())
            .map(|text| format!("- {text}"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    get_str(paragraph, "/text/text")
        .unwrap_or_default()
        .to_string()
}

fn cookie_has_value(cookie: &str, name: &str) -> bool {
    parse_cookie_pairs(cookie)
        .get(name)
        .is_some_and(|value| !value.is_empty())
}

fn parse_cookie_pairs(cookie: &str) -> BTreeMap<String, String> {
    cookie
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

fn rewrite_cdn_host(value: &str, host: &str) -> Option<String> {
    let mut url = url::Url::parse(value).ok()?;
    url.set_host(Some(host)).ok()?;
    url.set_port(None).ok()?;
    Some(url.into())
}

fn estimated_stream_size(
    stream: &Value,
    audio_bandwidth: u64,
    duration_secs: Option<u64>,
) -> Option<u64> {
    let duration_secs = duration_secs?;
    let video_bandwidth = get_u64(stream, "/bandwidth")?;
    Some(
        video_bandwidth
            .saturating_add(audio_bandwidth)
            .saturating_mul(duration_secs)
            / 8,
    )
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

#[async_trait]
impl Provider for BilibiliProvider {
    fn platform(&self) -> Platform {
        Platform::Bilibili
    }
    fn can_handle(&self, raw: &str) -> bool {
        if bare_bvid_regex().is_match(raw.trim()) || bare_av_regex().is_match(raw.trim()) {
            return true;
        }
        let url = if raw.starts_with("http://") || raw.starts_with("https://") {
            raw.to_string()
        } else {
            format!("https://{raw}")
        };
        url::Url::parse(&url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
            .is_some_and(|host| is_bilibili_host(&host))
    }
    async fn parse(&self, request: &ParseRequest) -> ProviderResult<ParsedContent> {
        let url = self.resolve(&request.url).await?;
        match self.target(&url)? {
            BilibiliTarget::Live(room) => {
                self.parse_live(&format!("https://live.bilibili.com/{room}"))
                    .await
            }
            BilibiliTarget::Dynamic(id) => self.parse_dynamic(&url, &id, &request.options).await,
            BilibiliTarget::Audio(audio) => {
                self.parse_audio(&format!("https://www.bilibili.com/audio/{audio}"))
                    .await
            }
            BilibiliTarget::Article(article) => {
                self.parse_article(&format!("https://www.bilibili.com/read/{article}"))
                    .await
            }
            BilibiliTarget::Video(video) => {
                self.parse_video(
                    &format!("https://www.bilibili.com/video/{video}"),
                    &request.options,
                )
                .await
            }
            BilibiliTarget::Bangumi(episode) => {
                self.parse_video(
                    &format!("https://www.bilibili.com/bangumi/play/{episode}"),
                    &request.options,
                )
                .await
            }
        }
    }
}
