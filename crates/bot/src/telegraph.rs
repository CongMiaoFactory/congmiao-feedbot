//! Telegraph overflow: large photo sets are published as one telegra.ph page
//! instead of many albums.
//!
//! Telegraph no longer accepts uploads, so pages hotlink the original image
//! URLs. Images that need request headers (`Referer`, `Cookie`) only work
//! through a configured proxy host; otherwise the job falls back to albums.

use crate::publisher::{ChunkedAlbums, OverflowCtx, OverflowSink};
use feedbot_core::extract::{Extracted, MediaItem, MediaKind, MediaSource};
use feedbot_core::{CachedPage, HttpClient, HttpRequest, MediaCacheRepo, PublishError};
use http::HeaderMap;
use http::header::{CONTENT_TYPE, COOKIE, HeaderValue, REFERER};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tracing::{info, warn};
use url::Url;

/// Telegraph rejects page content above 64 KiB.
const MAX_CONTENT_BYTES: usize = 64 * 1024;
const MAX_TITLE_CHARS: usize = 256;
const MAX_AUTHOR_CHARS: usize = 128;
const SHORT_NAME: &str = "CongMiaoFeedBot";

#[derive(Debug, Clone)]
pub struct TelegraphSettings {
    pub api_base: String,
    /// Empty: an account is created on first use and kept for this process.
    pub access_token: String,
    /// Shown when the post has no author.
    pub author_name: String,
    /// Lowercase image host → proxy host, e.g. `i.pximg.net` → `i.pixiv.re`.
    pub image_proxies: HashMap<String, String>,
}

struct Telegraph {
    http: Arc<dyn HttpClient>,
    settings: TelegraphSettings,
    token: OnceCell<String>,
}

impl Telegraph {
    fn new(http: Arc<dyn HttpClient>, mut settings: TelegraphSettings) -> Self {
        settings.api_base = settings.api_base.trim_end_matches('/').to_string();
        // The token lives only in the cell.
        let configured = std::mem::take(&mut settings.access_token);
        let token = OnceCell::new();
        if !configured.trim().is_empty() {
            let _ = token.set(configured.trim().to_string());
        }
        Self {
            http,
            settings,
            token,
        }
    }

    /// Hotlinkable URL of every item, or `None` when any item cannot be shown
    /// on a Telegraph page (not a photo, or needs headers and has no proxy).
    fn image_urls(&self, items: &[MediaItem]) -> Option<Vec<Url>> {
        items
            .iter()
            .map(|item| {
                let MediaSource::Direct { url, headers } = &item.source else {
                    return None;
                };
                if item.kind != MediaKind::Photo {
                    return None;
                }
                self.hotlink(url, headers)
            })
            .collect()
    }

    fn hotlink(&self, url: &Url, headers: &HeaderMap) -> Option<Url> {
        let host = url.host_str()?.to_ascii_lowercase();
        if let Some(proxy) = self.settings.image_proxies.get(&host) {
            let mut proxied = url.clone();
            proxied.set_host(Some(proxy)).ok()?;
            return Some(proxied);
        }
        let needs_headers = headers.contains_key(REFERER) || headers.contains_key(COOKIE);
        (!needs_headers).then(|| url.clone())
    }

    async fn create_page(
        &self,
        extracted: &Extracted,
        images: &[Url],
    ) -> Result<Url, PublishError> {
        let content = page_content(extracted, images).to_string();
        if content.len() > MAX_CONTENT_BYTES {
            return Err(PublishError::Failed(format!(
                "Telegraph 页面内容过大（{} 字节）",
                content.len()
            )));
        }
        let token = self.token().await?;
        let title = clip(&page_title(extracted), MAX_TITLE_CHARS);
        let author = clip(
            extracted
                .author
                .as_deref()
                .filter(|a| !a.is_empty())
                .unwrap_or(&self.settings.author_name),
            MAX_AUTHOR_CHARS,
        );
        let mut form = vec![
            ("access_token", token.as_str()),
            ("title", title.as_str()),
            ("author_name", author.as_str()),
            ("content", content.as_str()),
        ];
        let author_url = extracted.author_url.as_ref().map(Url::to_string);
        if let Some(u) = &author_url {
            form.push(("author_url", u));
        }
        let result = self.call("createPage", &form).await?;
        result
            .get("url")
            .and_then(Value::as_str)
            .and_then(|u| Url::parse(u).ok())
            .ok_or_else(|| PublishError::Failed("Telegraph 未返回页面地址".into()))
    }

    async fn token(&self) -> Result<&String, PublishError> {
        self.token
            .get_or_try_init(|| async {
                let result = self
                    .call(
                        "createAccount",
                        &[
                            ("short_name", SHORT_NAME),
                            (
                                "author_name",
                                &clip(&self.settings.author_name, MAX_AUTHOR_CHARS),
                            ),
                        ],
                    )
                    .await?;
                let token = result
                    .get("access_token")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| PublishError::Failed("Telegraph 未返回 access_token".into()))?;
                info!("created Telegraph account for this process");
                Ok(token.to_string())
            })
            .await
    }

    /// POST a form to `{api_base}/{method}` and unwrap `{ok, result, error}`.
    async fn call(&self, method: &str, form: &[(&str, &str)]) -> Result<Value, PublishError> {
        let url = Url::parse(&format!("{}/{method}", self.settings.api_base))
            .map_err(|e| PublishError::Failed(e.to_string()))?;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        let resp = self
            .http
            .fetch(HttpRequest::post(url, headers, body))
            .await
            .map_err(|e| PublishError::Failed(e.to_string()))?;
        let value: Value = serde_json::from_slice(&resp.body)
            .map_err(|e| PublishError::Failed(format!("Telegraph 响应无法解析: {e}")))?;
        unwrap_response(value)
    }
}

fn unwrap_response(value: Value) -> Result<Value, PublishError> {
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        return value
            .get("result")
            .cloned()
            .ok_or_else(|| PublishError::Failed("Telegraph 响应缺少 result".into()));
    }
    let error = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("未知错误");
    Err(PublishError::Failed(format!("Telegraph: {error}")))
}

fn page_title(extracted: &Extracted) -> String {
    extracted
        .title
        .as_deref()
        .or(extracted.author.as_deref())
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.trim().to_string())
        .unwrap_or_else(|| extracted.canonical_url.to_string())
}

/// Description paragraphs, one figure per image, then a link to the source.
/// Text goes in as JSON string nodes, so Telegraph never parses it as HTML.
fn page_content(extracted: &Extracted, images: &[Url]) -> Value {
    let mut nodes = Vec::new();
    if let Some(desc) = extracted.description.as_deref() {
        for line in desc.lines().map(str::trim).filter(|l| !l.is_empty()) {
            nodes.push(json!({ "tag": "p", "children": [line] }));
        }
    }
    if !extracted.tags.is_empty() {
        let tags: Vec<String> = extracted.tags.iter().map(|t| format!("#{t}")).collect();
        nodes.push(json!({ "tag": "p", "children": [tags.join(" ")] }));
    }
    for url in images {
        nodes.push(json!({
            "tag": "figure",
            "children": [{ "tag": "img", "attrs": { "src": url.as_str() } }]
        }));
    }
    nodes.push(json!({
        "tag": "p",
        "children": [{
            "tag": "a",
            "attrs": { "href": extracted.canonical_url.as_str() },
            "children": ["原链接"]
        }]
    }));
    Value::Array(nodes)
}

fn clip(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// `output.overflow = "telegraph"`: above `threshold` items, publish one page
/// when every item can be hotlinked, and reuse it for the same source while
/// the item count is unchanged. Anything else, including Telegraph errors,
/// falls back to [`ChunkedAlbums`].
pub struct TelegraphOverflow {
    telegraph: Telegraph,
    cache: Arc<dyn MediaCacheRepo>,
    fallback: ChunkedAlbums,
}

impl TelegraphOverflow {
    pub fn new(
        http: Arc<dyn HttpClient>,
        cache: Arc<dyn MediaCacheRepo>,
        settings: TelegraphSettings,
        threshold: usize,
    ) -> Self {
        Self {
            telegraph: Telegraph::new(http, settings),
            cache,
            fallback: ChunkedAlbums::new(threshold),
        }
    }

    async fn cached(&self, source_key: &str, items: i64) -> Option<Url> {
        match self.cache.get_page(source_key).await {
            Ok(Some(page)) if page.items == items => Url::parse(&page.url).ok(),
            Ok(_) => None,
            Err(e) => {
                warn!(error = %e, "page cache lookup failed");
                None
            }
        }
    }
}

#[async_trait::async_trait]
impl OverflowSink for TelegraphOverflow {
    async fn publish_page(&self, extracted: &Extracted, source_key: &str) -> Option<Url> {
        if extracted.items.len() <= self.fallback.threshold() {
            return None;
        }
        let items = i64::try_from(extracted.items.len()).unwrap_or(i64::MAX);
        if let Some(url) = self.cached(source_key, items).await {
            return Some(url);
        }
        let Some(images) = self.telegraph.image_urls(&extracted.items) else {
            info!(
                items = extracted.items.len(),
                "items cannot be hotlinked; sending albums instead of Telegraph"
            );
            return None;
        };
        match self.telegraph.create_page(extracted, &images).await {
            Ok(url) => {
                let page = CachedPage {
                    url: url.to_string(),
                    items,
                };
                if let Err(e) = self.cache.put_page(source_key, &page).await {
                    warn!(error = %e, "failed to cache page");
                }
                Some(url)
            }
            Err(e) => {
                warn!(error = %e, "Telegraph page failed; sending albums");
                None
            }
        }
    }

    fn summary(&self, ctx: &OverflowCtx<'_>) -> Option<String> {
        self.fallback.summary(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feedbot_core::ExtractError;
    use feedbot_core::extract::HttpResponse;
    use std::sync::Mutex;

    /// Records requests and answers with canned JSON bodies in order.
    struct FakeHttp {
        requests: Mutex<Vec<HttpRequest>>,
        replies: Mutex<Vec<Value>>,
    }

    #[async_trait::async_trait]
    impl HttpClient for FakeHttp {
        async fn fetch(&self, req: HttpRequest) -> Result<HttpResponse, ExtractError> {
            let final_url = req.url.clone();
            self.requests.lock().unwrap().push(req);
            let body = self.replies.lock().unwrap().remove(0);
            Ok(HttpResponse {
                status: 200,
                headers: HeaderMap::new(),
                body: body.to_string().into(),
                final_url,
            })
        }
    }

    fn fake(replies: Vec<Value>) -> Arc<FakeHttp> {
        Arc::new(FakeHttp {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(replies),
        })
    }

    fn settings(token: &str, proxies: &[(&str, &str)]) -> TelegraphSettings {
        TelegraphSettings {
            api_base: "https://api.telegra.ph/".into(),
            access_token: token.into(),
            author_name: "bot".into(),
            image_proxies: proxies
                .iter()
                .map(|(from, to)| (from.to_string(), to.to_string()))
                .collect(),
        }
    }

    fn photo(url: &str, referer: bool) -> MediaItem {
        let mut headers = HeaderMap::new();
        if referer {
            headers.insert(REFERER, HeaderValue::from_static("https://www.pixiv.net/"));
        }
        MediaItem {
            id: url.into(),
            kind: MediaKind::Photo,
            source: MediaSource::Direct {
                url: Url::parse(url).unwrap(),
                headers,
            },
            filename: None,
            cover: None,
        }
    }

    fn extracted(items: Vec<MediaItem>) -> Extracted {
        Extracted {
            extractor_id: "pixiv".into(),
            canonical_url: Url::parse("https://pixiv.net/artworks/1").unwrap(),
            title: Some("title".into()),
            author: Some("artist".into()),
            author_url: Some(Url::parse("https://pixiv.net/users/2").unwrap()),
            description: Some("line one\n\n<b>line two</b>".into()),
            tags: vec!["tag".into()],
            nsfw: false,
            ai: false,
            engagement: None,
            items,
        }
    }

    #[test]
    fn hotlink_rules() {
        let plain = Telegraph::new(fake(vec![]), settings("t", &[]));
        let open = photo("https://pbs.twimg.com/media/a.jpg", false);
        let guarded = photo("https://i.pximg.net/img-original/1_p0.png", true);
        assert!(plain.image_urls(std::slice::from_ref(&open)).is_some());
        assert!(plain.image_urls(&[open.clone(), guarded.clone()]).is_none());

        let proxied = Telegraph::new(
            fake(vec![]),
            settings("t", &[("i.pximg.net", "i.pixiv.re")]),
        );
        let urls = proxied.image_urls(&[guarded, open]).unwrap();
        assert_eq!(urls[0].as_str(), "https://i.pixiv.re/img-original/1_p0.png");
        assert_eq!(urls[1].host_str(), Some("pbs.twimg.com"));

        let mut video = photo("https://pbs.twimg.com/v.mp4", false);
        video.kind = MediaKind::Video;
        assert!(plain.image_urls(&[video]).is_none());
    }

    #[test]
    fn content_keeps_text_literal() {
        let ex = extracted(vec![]);
        let url = Url::parse("https://pbs.twimg.com/media/a.jpg").unwrap();
        let content = page_content(&ex, std::slice::from_ref(&url));
        let nodes = content.as_array().unwrap();
        assert_eq!(nodes[0]["children"][0], "line one");
        assert_eq!(nodes[1]["children"][0], "<b>line two</b>");
        assert_eq!(nodes[2]["children"][0], "#tag");
        assert_eq!(nodes[3]["children"][0]["attrs"]["src"], url.as_str());
        assert_eq!(
            nodes[4]["children"][0]["attrs"]["href"],
            "https://pixiv.net/artworks/1"
        );
    }

    #[tokio::test]
    async fn creates_account_once_then_pages() {
        let http = fake(vec![
            json!({ "ok": true, "result": { "access_token": "tok" } }),
            json!({ "ok": true, "result": { "url": "https://telegra.ph/a-01-01" } }),
            json!({ "ok": true, "result": { "url": "https://telegra.ph/b-01-01" } }),
        ]);
        let telegraph = Telegraph::new(http.clone(), settings("", &[]));
        let ex = extracted(vec![]);
        let img = [Url::parse("https://pbs.twimg.com/media/a.jpg").unwrap()];
        let first = telegraph.create_page(&ex, &img).await.unwrap();
        let second = telegraph.create_page(&ex, &img).await.unwrap();
        assert_eq!(first.as_str(), "https://telegra.ph/a-01-01");
        assert_eq!(second.as_str(), "https://telegra.ph/b-01-01");

        let requests = http.requests.lock().unwrap();
        let paths: Vec<&str> = requests.iter().map(|r| r.url.path()).collect();
        assert_eq!(paths, ["/createAccount", "/createPage", "/createPage"]);
        let form: HashMap<String, String> = url::form_urlencoded::parse(&requests[1].body)
            .into_owned()
            .collect();
        assert_eq!(form["access_token"], "tok");
        assert_eq!(form["title"], "title");
        assert_eq!(form["author_name"], "artist");
        assert_eq!(form["author_url"], "https://pixiv.net/users/2");
    }

    #[tokio::test]
    async fn api_error_is_reported() {
        let http = fake(vec![json!({ "ok": false, "error": "CONTENT_TOO_BIG" })]);
        let telegraph = Telegraph::new(http, settings("tok", &[]));
        let err = telegraph
            .create_page(&extracted(vec![]), &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("CONTENT_TOO_BIG"));
    }

    /// Page part of `MediaCacheRepo`; the album cache is unused here.
    #[derive(Default)]
    struct PageCache(Mutex<HashMap<String, CachedPage>>);

    #[async_trait::async_trait]
    impl MediaCacheRepo for PageCache {
        async fn get_cache_all(
            &self,
            _source_key: &str,
        ) -> Result<Vec<feedbot_core::MediaCacheEntry>, feedbot_core::StoreError> {
            Ok(Vec::new())
        }
        async fn put_many(
            &self,
            _entries: &[feedbot_core::MediaCacheEntry],
        ) -> Result<(), feedbot_core::StoreError> {
            Ok(())
        }
        async fn get_page(
            &self,
            source_key: &str,
        ) -> Result<Option<CachedPage>, feedbot_core::StoreError> {
            Ok(self.0.lock().unwrap().get(source_key).cloned())
        }
        async fn put_page(
            &self,
            source_key: &str,
            page: &CachedPage,
        ) -> Result<(), feedbot_core::StoreError> {
            self.0
                .lock()
                .unwrap()
                .insert(source_key.into(), page.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn overflow_only_above_threshold_and_hotlinkable() {
        let page = json!({ "ok": true, "result": { "url": "https://telegra.ph/p" } });
        let open = || photo("https://pbs.twimg.com/media/a.jpg", false);
        let sink = TelegraphOverflow::new(
            fake(vec![page.clone()]),
            Arc::new(PageCache::default()),
            settings("tok", &[]),
            2,
        );
        assert!(
            sink.publish_page(&extracted(vec![open(), open()]), "k")
                .await
                .is_none()
        );
        let guarded = photo("https://i.pximg.net/a.png", true);
        assert!(
            sink.publish_page(&extracted(vec![open(), open(), guarded]), "k")
                .await
                .is_none()
        );
        let url = sink
            .publish_page(&extracted(vec![open(), open(), open()]), "k")
            .await;
        assert_eq!(url.unwrap().as_str(), "https://telegra.ph/p");
    }

    #[tokio::test]
    async fn pages_are_reused_until_the_item_count_changes() {
        let http = fake(vec![
            json!({ "ok": true, "result": { "url": "https://telegra.ph/a" } }),
            json!({ "ok": true, "result": { "url": "https://telegra.ph/b" } }),
        ]);
        let sink = TelegraphOverflow::new(
            http.clone(),
            Arc::new(PageCache::default()),
            settings("tok", &[]),
            1,
        );
        let open = || photo("https://pbs.twimg.com/media/a.jpg", false);
        let two = extracted(vec![open(), open()]);
        for _ in 0..2 {
            let url = sink.publish_page(&two, "k").await.unwrap();
            assert_eq!(url.as_str(), "https://telegra.ph/a");
        }
        assert_eq!(http.requests.lock().unwrap().len(), 1, "second send reused");

        let three = extracted(vec![open(), open(), open()]);
        let url = sink.publish_page(&three, "k").await.unwrap();
        assert_eq!(url.as_str(), "https://telegra.ph/b");
    }
}
