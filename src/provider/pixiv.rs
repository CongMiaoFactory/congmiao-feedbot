use std::collections::BTreeMap;

use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{Config, model::*};

use super::{Provider, filename_from_url, get_str, get_u64, json_response};

pub struct PixivProvider {
    client: Client,
    base: String,
    refresh_token: Option<String>,
    access_token: Mutex<Option<String>>,
    regex: Regex,
}

impl PixivProvider {
    pub fn new(client: Client, config: &Config) -> Self {
        Self { client, base: config.pixiv_web_api_base.trim_end_matches('/').to_string(), refresh_token: config.pixiv_refresh_token.clone(), access_token: Mutex::new(None),
            regex: Regex::new(r"(?i)(?:https?://)?(?:www\.)?pixiv\.net/(?:[a-z]{2}/)?artworks/(\d+)|(?:https?://)?(?:www\.)?pixiv\.net/member_illust\.php\?[^\s]*illust_id=(\d+)").expect("valid Pixiv regex") }
    }
    fn id(&self, url: &str) -> Option<String> {
        let c = self.regex.captures(url)?;
        c.get(1)
            .or_else(|| c.get(2))
            .map(|m| m.as_str().to_string())
    }

    async fn app_token(&self) -> ProviderResult<Option<String>> {
        let Some(refresh_token) = &self.refresh_token else {
            return Ok(None);
        };
        if let Some(token) = self.access_token.lock().await.clone() {
            return Ok(Some(token));
        }
        let response = self
            .client
            .post("https://oauth.secure.pixiv.net/auth/token")
            .header(
                "User-Agent",
                "PixivAndroidApp/5.0.234 (Android 11; Pixel 5)",
            )
            .form(&[
                ("client_id", "MOBrBDS8blbauoSck0ZfDbtuzpyT"),
                ("client_secret", "lsACyCD94FhDUtGTXi3QzcFE2uU1hqtDaKeqrdwj"),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.as_str()),
                ("include_policy", "true"),
            ])
            .send()
            .await
            .map_err(|e| ProviderError::Authentication(e.to_string()))?;
        let value = json_response(response, "Pixiv OAuth").await?;
        let token = get_str(&value, "/access_token")
            .or_else(|| get_str(&value, "/response/access_token"))
            .ok_or_else(|| ProviderError::Authentication("refresh token 无效".into()))?
            .to_string();
        *self.access_token.lock().await = Some(token.clone());
        Ok(Some(token))
    }

    async fn app_api_get(
        &self,
        endpoint: &str,
        id: &str,
        source: &str,
    ) -> ProviderResult<Option<Value>> {
        // access token 约一小时过期；首次失败时清掉缓存，用 refresh token 换新后重试一次。
        for attempt in 0..2 {
            let Some(token) = self.app_token().await? else {
                return Ok(None);
            };
            let response = self
                .client
                .get(endpoint)
                .query(&[("illust_id", id)])
                .bearer_auth(token)
                .header("App-OS", "android")
                .header("App-OS-Version", "11")
                .header("App-Version", "5.0.234")
                .send()
                .await
                .map_err(|e| ProviderError::Upstream(e.to_string()))?;
            match json_response(response, source).await {
                Ok(root) => return Ok(Some(root)),
                Err(error) if attempt == 0 => {
                    *self.access_token.lock().await = None;
                    tracing::debug!(%error, "Pixiv App API 请求失败，刷新 token 后重试");
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("app_api_get 循环总会返回")
    }

    async fn app_detail(&self, id: &str) -> ProviderResult<Option<Value>> {
        Ok(self
            .app_api_get(
                "https://app-api.pixiv.net/v1/illust/detail",
                id,
                "Pixiv App API",
            )
            .await?
            .and_then(|root| root.get("illust").cloned()))
    }

    async fn app_ugoira(&self, id: &str) -> ProviderResult<Option<Value>> {
        Ok(self
            .app_api_get(
                "https://app-api.pixiv.net/v1/ugoira/metadata",
                id,
                "Pixiv Ugoira API",
            )
            .await?
            .and_then(|root| root.get("ugoira_metadata").cloned()))
    }
}

#[async_trait]
impl Provider for PixivProvider {
    fn platform(&self) -> Platform {
        Platform::Pixiv
    }
    fn can_handle(&self, url: &str) -> bool {
        self.regex.is_match(url)
    }

    async fn parse(&self, request: &ParseRequest) -> ProviderResult<ParsedContent> {
        let id = self
            .id(&request.url)
            .ok_or_else(|| ProviderError::InvalidUrl(request.url.clone()))?;
        let app = self.app_detail(&id).await.ok().flatten();
        let root = if app.is_none() {
            let response = self
                .client
                .get(format!("{}/ajax/illust/{id}", self.base))
                .query(&[("lang", "zh")])
                .header("Referer", "https://www.pixiv.net/")
                .send()
                .await
                .map_err(|e| ProviderError::Upstream(e.to_string()))?;
            json_response(response, "Pixiv Web API").await?
        } else {
            Value::Null
        };
        let body = app
            .as_ref()
            .unwrap_or_else(|| root.get("body").unwrap_or(&root));
        let author_name = get_str(body, "/author/name")
            .or_else(|| get_str(body, "/userName"))
            .or_else(|| get_str(body, "/user/name"))
            .unwrap_or_default();
        let author_id = get_str(body, "/author/id")
            .or_else(|| get_str(body, "/userId"))
            .or_else(|| get_str(body, "/user/id"))
            .unwrap_or_default();
        let mut urls: Vec<(String, Option<String>)> = Vec::new();
        for pointer in ["/images", "/urls", "/imageUrls"] {
            if let Some(array) = body.pointer(pointer).and_then(Value::as_array) {
                urls.extend(array.iter().filter_map(|value| {
                    let original = value
                        .as_str()
                        .or_else(|| value.get("original").and_then(Value::as_str))
                        .or_else(|| value.get("url").and_then(Value::as_str))?;
                    let thumbnail = value
                        .get("regular")
                        .or_else(|| value.get("large"))
                        .or_else(|| value.get("medium"))
                        .and_then(Value::as_str)
                        .filter(|url| *url != original)
                        .map(str::to_string);
                    Some((original.to_string(), thumbnail))
                }));
            }
        }
        if let Some(pages) = body.pointer("/meta_pages").and_then(Value::as_array) {
            urls.extend(pages.iter().filter_map(|page| {
                let original = get_str(page, "/image_urls/original")
                    .or_else(|| get_str(page, "/image_urls/large"))?;
                let thumbnail = get_str(page, "/image_urls/large")
                    .or_else(|| get_str(page, "/image_urls/medium"))
                    .filter(|url| *url != original)
                    .map(str::to_string);
                Some((original.to_string(), thumbnail))
            }));
        }
        if app.is_none() && get_u64(body, "/pageCount").unwrap_or(1) > 1 {
            let response = self
                .client
                .get(format!("{}/ajax/illust/{id}/pages", self.base))
                .query(&[("lang", "zh")])
                .header("Referer", "https://www.pixiv.net/")
                .send()
                .await
                .map_err(|e| ProviderError::Upstream(e.to_string()))?;
            let pages = json_response(response, "Pixiv Pages API").await?;
            urls.clear();
            if let Some(items) = pages.pointer("/body").and_then(Value::as_array) {
                urls.extend(items.iter().filter_map(|page| {
                    let original = get_str(page, "/urls/original")?;
                    let thumbnail = get_str(page, "/urls/regular")
                        .or_else(|| get_str(page, "/urls/small"))
                        .filter(|url| *url != original)
                        .map(str::to_string);
                    Some((original.to_string(), thumbnail))
                }));
            }
        }
        if urls.is_empty()
            && let Some(url) = get_str(body, "/image")
                .or_else(|| get_str(body, "/url"))
                .or_else(|| get_str(body, "/urls/original"))
                .or_else(|| get_str(body, "/meta_single_page/original_image_url"))
                .or_else(|| get_str(body, "/image_urls/large"))
        {
            let thumbnail = get_str(body, "/urls/regular")
                .or_else(|| get_str(body, "/image_urls/large"))
                .filter(|thumbnail| *thumbnail != url)
                .map(str::to_string);
            urls.push((url.to_string(), thumbnail));
        }
        let canonical = format!("https://www.pixiv.net/artworks/{id}");
        let mut media = Vec::new();
        let is_ugoira =
            get_str(body, "/type") == Some("ugoira") || get_u64(body, "/illustType") == Some(2);
        let ugoira = if is_ugoira {
            if let Some(metadata) = self.app_ugoira(&id).await.ok().flatten() {
                Some(metadata)
            } else {
                let response = self
                    .client
                    .get(format!("{}/ajax/illust/{id}/ugoira_meta", self.base))
                    .query(&[("lang", "zh")])
                    .header("Referer", "https://www.pixiv.net/")
                    .send()
                    .await
                    .map_err(|e| ProviderError::Upstream(e.to_string()))?;
                let root = json_response(response, "Pixiv Ugoira Web API").await?;
                root.get("body").cloned()
            }
        } else {
            None
        };
        if let Some(metadata) = ugoira
            && let Some(zip_url) = get_str(&metadata, "/zip_urls/medium")
                .or_else(|| get_str(&metadata, "/originalSrc"))
                .or_else(|| get_str(&metadata, "/src"))
        {
            let mut headers = BTreeMap::new();
            headers.insert("Referer".into(), "https://www.pixiv.net/".into());
            let frames = metadata
                .get("frames")
                .cloned()
                .unwrap_or(Value::Array(vec![]));
            media.push(MediaItem {
                kind: MediaKind::Animation,
                source_url: zip_url.into(),
                fallback_urls: vec![],
                thumbnail_url: get_str(body, "/image_urls/large").map(str::to_string),
                filename: format!("pixiv-{id}-ugoira.mp4"),
                mime_type: Some("video/mp4".into()),
                duration_secs: None,
                width: get_u64(body, "/width").map(|n| n as u32),
                height: get_u64(body, "/height").map(|n| n as u32),
                size: None,
                headers,
                cache_key: format!("pixiv:{id}:ugoira"),
                requires_download: true,
                secondary_url: Some(format!("ugoira:{frames}")),
                secondary_fallback_urls: vec![],
            });
        }
        for (index, (url, thumbnail_url)) in urls.into_iter().enumerate() {
            if !media.is_empty() && is_ugoira {
                break;
            }
            let mut headers = BTreeMap::new();
            headers.insert("Referer".into(), "https://www.pixiv.net/".into());
            media.push(MediaItem {
                kind: MediaKind::Photo,
                source_url: url.clone(),
                fallback_urls: vec![],
                thumbnail_url,
                filename: filename_from_url(&url, &format!("pixiv-{id}-p{index}.jpg")),
                mime_type: None,
                duration_secs: None,
                width: None,
                height: None,
                size: None,
                headers,
                cache_key: format!("pixiv:{id}:p{index}"),
                requires_download: true,
                secondary_url: None,
                secondary_fallback_urls: vec![],
            });
        }
        if media.is_empty() {
            return Err(ProviderError::Unavailable(request.url.clone()));
        }
        let tags = body
            .pointer("/tags/tags")
            .or_else(|| body.pointer("/tags"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| {
                        v.as_str()
                            .or_else(|| v.get("tag").and_then(Value::as_str))
                            .or_else(|| v.get("name").and_then(Value::as_str))
                    })
                    .map(|s| format!("#{s}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let sensitive = get_u64(body, "/xRestrict")
            .or_else(|| get_u64(body, "/x_restrict"))
            .is_some_and(|value| value > 0)
            || tags.split_whitespace().any(|tag| {
                matches!(
                    tag.trim_start_matches('#').to_ascii_lowercase().as_str(),
                    "r-18" | "r18" | "r-18g" | "r18g" | "nsfw" | "成人向"
                )
            });
        let description = get_str(body, "/description")
            .or_else(|| get_str(body, "/caption"))
            .unwrap_or_default();
        Ok(ParsedContent {
            platform: Platform::Pixiv,
            kind: ContentKind::Artwork,
            id,
            canonical_url: canonical,
            author: Author {
                id: author_id.into(),
                name: author_name.into(),
                url: if author_id.is_empty() {
                    None
                } else {
                    Some(format!("https://www.pixiv.net/users/{author_id}"))
                },
                avatar_url: None,
            },
            title: get_str(body, "/title").unwrap_or_default().into(),
            text: format!("{description}\n{tags}").trim().to_string(),
            sensitive,
            stats: Stats {
                likes: get_u64(body, "/likeCount").or_else(|| get_u64(body, "/bookmarkCount")),
                views: get_u64(body, "/viewCount"),
                ..Default::default()
            },
            media,
            collection_items: Vec::new(),
        })
    }
}
