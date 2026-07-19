mod bilibili;
mod netease;
mod pixiv;
mod x;
mod youtube;

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::join_all;
use reqwest::Client;

use crate::{Config, model::*};

pub use bilibili::BilibiliProvider;
pub use netease::NeteaseProvider;
pub use pixiv::PixivProvider;
pub use x::XProvider;
pub use youtube::YouTubeProvider;

#[async_trait]
pub trait Provider: Send + Sync {
    fn platform(&self) -> Platform;
    fn can_handle(&self, url: &str) -> bool;
    async fn parse(&self, request: &ParseRequest) -> ProviderResult<ParsedContent>;
}

#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: Vec<Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new(config: &Config) -> ProviderResult<Self> {
        let client = Client::builder()
            .user_agent("Mozilla/5.0 (compatible; CongmiaoFeedBot/0.1; +https://github.com/congmiao-factory)")
            .redirect(reqwest::redirect::Policy::limited(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ProviderError::Upstream(e.to_string()))?;
        let mut registry = Self::default();
        registry.register(XProvider::new(client.clone(), config));
        registry.register(YouTubeProvider::new(client.clone(), config));
        registry.register(PixivProvider::new(client.clone(), config));
        registry.register(BilibiliProvider::new(client.clone(), config));
        registry.register(NeteaseProvider::new(client, config));
        Ok(registry)
    }

    pub fn register<P: Provider + 'static>(&mut self, provider: P) {
        self.providers.push(Arc::new(provider));
    }

    pub fn find(&self, url: &str) -> Option<Arc<dyn Provider>> {
        self.providers.iter().find(|p| p.can_handle(url)).cloned()
    }

    pub async fn parse_one(&self, request: ParseRequest) -> ProviderResult<ParsedContent> {
        self.find(&request.url)
            .ok_or_else(|| ProviderError::Unsupported(request.url.clone()))?
            .parse(&request)
            .await
    }

    pub async fn parse_many(
        &self,
        requests: Vec<ParseRequest>,
    ) -> Vec<ProviderResult<ParsedContent>> {
        join_all(requests.into_iter().map(|r| self.parse_one(r))).await
    }
}

pub(crate) fn get_str<'a>(value: &'a serde_json::Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(serde_json::Value::as_str)
}

pub(crate) fn get_u64(value: &serde_json::Value, pointer: &str) -> Option<u64> {
    value.pointer(pointer).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

pub(crate) fn filename_from_url(url: &str, fallback: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.path_segments()?.next_back().map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

pub(crate) async fn json_response(
    response: reqwest::Response,
    source: &str,
) -> ProviderResult<serde_json::Value> {
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(ProviderError::Authentication(source.to_string()));
    }
    if status.as_u16() == 404 {
        return Err(ProviderError::Unavailable(source.to_string()));
    }
    if status.as_u16() == 429 {
        return Err(ProviderError::RateLimited(source.to_string()));
    }
    if !status.is_success() {
        return Err(ProviderError::Upstream(format!("{source}: HTTP {status}")));
    }
    response
        .json()
        .await
        .map_err(|e| ProviderError::InvalidResponse(format!("{source}: {e}")))
}
