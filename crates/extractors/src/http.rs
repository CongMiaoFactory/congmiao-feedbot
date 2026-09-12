use bytes::Bytes;
use feedbot_core::{ExtractError, HttpClient, HttpMethod, HttpRequest, HttpResponse};
use http::HeaderMap;
use reqwest::redirect::Policy;
use std::time::Duration;
use url::Url;

pub struct ReqwestClient {
    inner: reqwest::Client,
}

impl ReqwestClient {
    pub fn new(
        user_agent: &str,
        timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<Self, ExtractError> {
        let inner = reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(timeout)
            .connect_timeout(connect_timeout)
            .redirect(Policy::limited(10))
            .cookie_store(true)
            .build()
            .map_err(|e| ExtractError::Network(e.to_string()))?;
        Ok(Self { inner })
    }

    pub fn inner(&self) -> &reqwest::Client {
        &self.inner
    }
}

#[async_trait::async_trait]
impl HttpClient for ReqwestClient {
    async fn fetch(&self, req: HttpRequest) -> Result<HttpResponse, ExtractError> {
        let mut builder = match req.method {
            HttpMethod::Get => self.inner.get(req.url.clone()),
            HttpMethod::Head => self.inner.head(req.url.clone()),
        };
        for (name, value) in req.headers.iter() {
            builder = builder.header(name, value);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| ExtractError::Network(e.to_string()))?;
        let status = response.status().as_u16();
        let final_url = response.url().clone();
        let mut headers = HeaderMap::new();
        for (k, v) in response.headers().iter() {
            headers.append(k, v.clone());
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| ExtractError::Network(e.to_string()))?;
        Ok(HttpResponse {
            status,
            headers,
            body,
            final_url,
        })
    }
}

pub async fn expand_shortlink(http: &dyn HttpClient, url: &Url) -> Result<Url, ExtractError> {
    if !is_shortener(url) {
        return Ok(url.clone());
    }
    let resp = http
        .fetch(HttpRequest {
            url: url.clone(),
            headers: HeaderMap::new(),
            method: HttpMethod::Head,
        })
        .await?;
    Ok(resp.final_url)
}

fn is_shortener(url: &Url) -> bool {
    matches!(
        url.host_str().unwrap_or(""),
        "t.co"
            | "b23.tv"
            | "b23.wtf"
            | "bit.ly"
            | "tinyurl.com"
            | "xhslink.com"
            | "youtu.be"
            | "163cn.tv"
    )
}

pub async fn download(
    client: &reqwest::Client,
    url: &Url,
    headers: &HeaderMap,
) -> Result<Bytes, ExtractError> {
    let mut builder = client.get(url.clone());
    for (name, value) in headers.iter() {
        builder = builder.header(name, value);
    }
    let resp = builder
        .send()
        .await
        .map_err(|e| ExtractError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(ExtractError::Network(format!("HTTP {}", resp.status())));
    }
    resp.bytes()
        .await
        .map_err(|e| ExtractError::Network(e.to_string()))
}

/// Shared download helper used by the media crate without depending on extractors.
pub fn reqwest_to_http_headers(headers: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in headers.iter() {
        out.append(k, v.clone());
    }
    out
}

#[allow(dead_code)]
fn _bytes(b: Bytes) -> Bytes {
    b
}
