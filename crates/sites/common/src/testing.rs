//! Test doubles for whole `extract` / `feed` runs: canned upstream responses
//! by host and path, and a request builder.

use bytes::Bytes;
use feedbot_core::auth::{AuthMethod, Credential};
use feedbot_core::extract::{
    ExtractCtx, ExtractError, ExtractRequest, HttpClient, HttpRequest, HttpResponse, RequesterInfo,
};
use feedbot_core::options::OptionValues;
use feedbot_core::{MediaError, Ytdlp, YtdlpSpec};
use http::HeaderMap;
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};
use url::Url;

use crate::NoYtdlp;

/// Answers each request with the first route whose `host/path` equals the
/// request's; records every request. An unknown route is a test failure.
#[derive(Default)]
pub struct FakeHttp {
    routes: Vec<(String, u16, Bytes)>,
    seen: Mutex<Vec<HttpRequest>>,
}

impl FakeHttp {
    pub fn new() -> Self {
        Self::default()
    }

    /// `route` is `host/path`, e.g. `www.pixiv.net/ajax/illust/1`.
    pub fn json(self, route: &str, body: Value) -> Self {
        self.status(route, 200, body.to_string())
    }

    pub fn status(mut self, route: &str, status: u16, body: impl Into<Bytes>) -> Self {
        self.routes.push((route.into(), status, body.into()));
        self
    }

    /// Requested URLs in order.
    pub fn urls(&self) -> Vec<Url> {
        let seen = self.seen.lock().unwrap();
        seen.iter().map(|r| r.url.clone()).collect()
    }

    /// The last request to `route`.
    pub fn last(&self, route: &str) -> HttpRequest {
        let seen = self.seen.lock().unwrap();
        seen.iter()
            .rev()
            .find(|r| route_of(&r.url) == route)
            .cloned()
            .unwrap_or_else(|| panic!("no request to {route}"))
    }

    pub fn ctx(self: &Arc<Self>) -> ExtractCtx {
        ExtractCtx::new(self.clone(), Arc::new(NoYtdlp))
    }
}

/// An `HttpClient` for extractors whose test never sends a request.
pub fn no_http() -> Arc<dyn HttpClient> {
    Arc::new(FakeHttp::new())
}

fn route_of(url: &Url) -> String {
    format!("{}{}", url.host_str().unwrap_or_default(), url.path())
}

#[async_trait::async_trait]
impl HttpClient for FakeHttp {
    async fn fetch(&self, req: HttpRequest) -> Result<HttpResponse, ExtractError> {
        let route = route_of(&req.url);
        let (_, status, body) = self
            .routes
            .iter()
            .find(|(r, ..)| *r == route)
            .unwrap_or_else(|| panic!("unexpected request: {}", req.url));
        let resp = HttpResponse {
            status: *status,
            headers: HeaderMap::new(),
            body: body.clone(),
            final_url: req.url.clone(),
        };
        self.seen.lock().unwrap().push(req);
        Ok(resp)
    }
}

/// `Ytdlp` whose probes return a canned result and record `(url, cookies)`;
/// downloads fail.
pub struct FakeYtdlp {
    probe: Result<Value, String>,
    calls: Mutex<Vec<(Url, Option<String>)>>,
}

impl FakeYtdlp {
    pub fn new(probe: Value) -> Self {
        Self::with(Ok(probe))
    }

    /// Probes fail with `MediaError::Ytdlp(stderr)`.
    pub fn failing(stderr: &str) -> Self {
        Self::with(Err(stderr.into()))
    }

    fn with(probe: Result<Value, String>) -> Self {
        Self {
            probe,
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<(Url, Option<String>)> {
        self.calls.lock().unwrap().clone()
    }

    pub fn ctx(self: &Arc<Self>) -> ExtractCtx {
        ExtractCtx::new(Arc::new(FakeHttp::new()), self.clone())
    }
}

#[async_trait::async_trait]
impl Ytdlp for FakeYtdlp {
    async fn probe_json(&self, url: &Url, cookies: Option<&str>) -> Result<Value, MediaError> {
        self.calls
            .lock()
            .unwrap()
            .push((url.clone(), cookies.map(str::to_string)));
        self.probe.clone().map_err(MediaError::Ytdlp)
    }

    async fn probe_playlist(
        &self,
        url: &Url,
        cookies: Option<&str>,
        _limit: u32,
    ) -> Result<Value, MediaError> {
        self.probe_json(url, cookies).await
    }

    async fn download(
        &self,
        _url: &Url,
        _dest: &Path,
        _spec: &YtdlpSpec,
    ) -> Result<(), MediaError> {
        Err(MediaError::Ytdlp("fake".into()))
    }
}

/// Request for `url` with `options`, logged in with `cookie` when given.
pub fn request(
    extractor_id: &str,
    url: &str,
    options: OptionValues,
    cookie: Option<&str>,
) -> ExtractRequest {
    ExtractRequest {
        url: Url::parse(url).unwrap(),
        options,
        credential: cookie.map(|c| Credential {
            extractor_id: extractor_id.into(),
            method: AuthMethod::Cookie,
            payload: c.as_bytes().to_vec(),
            expires_at: None,
        }),
        requester: RequesterInfo {
            user_id: 1,
            chat_id: 1,
        },
    }
}
