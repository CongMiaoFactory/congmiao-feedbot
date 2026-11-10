//! Extractor contract, request/response types, and HTTP client trait.

use crate::auth::{AuthProvider, Credential};
use crate::media::Ytdlp;
use crate::options::{OptionError, OptionSpec, OptionValues, merge_layers};
use bytes::Bytes;
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone)]
pub struct RequesterInfo {
    pub user_id: i64,
    pub chat_id: i64,
}

#[derive(Debug, Clone)]
pub struct ExtractRequest {
    pub url: Url,
    pub options: OptionValues,
    pub credential: Option<Credential>,
    pub requester: RequesterInfo,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Engagement {
    pub views: Option<u64>,
    pub likes: Option<u64>,
    pub shares: Option<u64>,
    pub comments: Option<u64>,
}

impl Engagement {
    pub fn is_empty(&self) -> bool {
        self.views.is_none()
            && self.likes.is_none()
            && self.shares.is_none()
            && self.comments.is_none()
    }

    pub fn packed(
        views: Option<u64>,
        likes: Option<u64>,
        shares: Option<u64>,
        comments: Option<u64>,
    ) -> Option<Self> {
        let e = Self {
            views,
            likes,
            shares,
            comments,
        };
        (!e.is_empty()).then_some(e)
    }
}

#[derive(Debug, Clone)]
pub struct Extracted {
    pub extractor_id: String,
    pub canonical_url: Url,
    pub title: Option<String>,
    pub author: Option<String>,
    pub author_url: Option<Url>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub nsfw: bool,
    pub ai: bool,
    pub engagement: Option<Engagement>,
    pub items: Vec<MediaItem>,
}

#[derive(Debug, Clone)]
pub struct MediaItem {
    pub id: String,
    pub kind: MediaKind,
    pub source: MediaSource,
    pub filename: Option<String>,
    /// Optional cover downloaded for `send_audio` / video thumbnails.
    pub cover: Option<Url>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Photo,
    Video,
    Animation,
    Audio,
    Document,
}

/// One frame inside a Pixiv ugoira zip, with its display duration.
#[derive(Debug, Clone)]
pub struct UgoiraFrame {
    pub file: String,
    pub delay_ms: u32,
}

#[derive(Debug, Clone)]
pub enum MediaSource {
    Direct {
        url: Url,
        headers: HeaderMap,
    },
    Separated {
        video: Url,
        audio: Url,
        headers: HeaderMap,
    },
    UgoiraZip {
        zip: Url,
        headers: HeaderMap,
        frames: Vec<UgoiraFrame>,
    },
    YtDlp {
        url: Url,
        audio_only: bool,
        max_height: Option<u32>,
        thumbnail: Option<Url>,
        cookies: Option<String>,
    },
}

/// One item of a subscribable source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedEntry {
    /// Stable per-source identity used for de-duplication.
    pub key: String,
    pub url: Url,
}

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("没有匹配的解析器")]
    NoExtractor,
    #[error("需要登录后才能解析此链接，请私聊发送 /login {0}")]
    AuthRequired(String),
    #[error("网络错误: {0}")]
    Network(String),
    #[error("解析失败: {0}")]
    Failed(String),
    #[error("参数错误: {0}")]
    Options(#[from] OptionError),
    #[error("不支持的链接")]
    Unsupported,
}

impl ExtractError {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Network(_))
    }
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub url: Url,
    pub headers: HeaderMap,
    pub method: HttpMethod,
    /// Request body; only sent for `Post`.
    pub body: Bytes,
    /// When false, a 3xx response is returned as-is instead of being followed.
    pub follow_redirects: bool,
}

impl HttpRequest {
    pub fn get(url: Url, headers: HeaderMap) -> Self {
        Self {
            url,
            headers,
            method: HttpMethod::Get,
            body: Bytes::new(),
            follow_redirects: true,
        }
    }

    /// `headers` should carry the body's `Content-Type`.
    pub fn post(url: Url, headers: HeaderMap, body: impl Into<Bytes>) -> Self {
        Self {
            method: HttpMethod::Post,
            body: body.into(),
            ..Self::get(url, headers)
        }
    }

    pub fn head(url: Url, headers: HeaderMap) -> Self {
        Self {
            method: HttpMethod::Head,
            ..Self::get(url, headers)
        }
    }

    pub fn without_redirects(mut self) -> Self {
        self.follow_redirects = false;
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub enum HttpMethod {
    Get,
    Head,
    Post,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub final_url: Url,
}

#[async_trait::async_trait]
pub trait HttpClient: Send + Sync {
    async fn fetch(&self, req: HttpRequest) -> Result<HttpResponse, ExtractError>;
}

#[derive(Clone)]
pub struct ExtractCtx {
    pub http: Arc<dyn HttpClient>,
    pub ytdlp: Arc<dyn Ytdlp>,
}

impl ExtractCtx {
    pub fn new(http: Arc<dyn HttpClient>, ytdlp: Arc<dyn Ytdlp>) -> Self {
        Self { http, ytdlp }
    }
}

/// Identity and help text of one site, in one place; command pages read it.
#[derive(Debug, Clone, Copy)]
pub struct SiteMeta {
    /// Stable id stored in jobs, subscriptions, and credentials.
    pub id: &'static str,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// Higher wins when several sites match one URL.
    pub priority: i32,
    /// Extractable work types, e.g. `插画`, `动图 ugoira`.
    pub kinds: &'static [&'static str],
    /// Short URL shapes `extract` accepts.
    pub examples: &'static [&'static str],
    /// Short URL shapes `/sub` accepts.
    pub feed_examples: &'static [&'static str],
}

#[async_trait::async_trait]
pub trait Extractor: Send + Sync {
    fn meta(&self) -> SiteMeta;

    fn id(&self) -> &'static str {
        self.meta().id
    }
    fn display_name(&self) -> &'static str {
        self.meta().name
    }
    fn aliases(&self) -> &'static [&'static str] {
        self.meta().aliases
    }
    fn priority(&self) -> i32 {
        self.meta().priority
    }
    fn extract_kinds(&self) -> &'static [&'static str] {
        self.meta().kinds
    }
    fn url_examples(&self) -> &'static [&'static str] {
        self.meta().examples
    }
    fn feed_examples(&self) -> &'static [&'static str] {
        self.meta().feed_examples
    }

    fn matches(&self, url: &Url) -> bool;

    /// Canonical form of a URL this site owns (host aliases, offline-known
    /// short links); `None` when the URL is not this site's.
    fn rewrite_url(&self, _url: &Url) -> Option<Url> {
        None
    }

    /// Caption layout of this site's results.
    fn present(&self) -> crate::present::Present {
        crate::present::Present::Default
    }

    fn options(&self) -> &'static OptionSpec {
        &OptionSpec::EMPTY
    }
    fn validate(&self, _opts: &mut OptionValues) -> Result<(), OptionError> {
        Ok(())
    }

    /// Options of one request: spec defaults < `saved` chat/user layers <
    /// inline `tokens`, then [`Extractor::validate`].
    fn resolve_options(
        &self,
        saved: &OptionValues,
        tokens: &[String],
    ) -> Result<OptionValues, OptionError> {
        let mut options = merge_layers(self.options(), saved, tokens)?;
        self.validate(&mut options)?;
        Ok(options)
    }
    fn auth(&self) -> Option<&dyn AuthProvider> {
        None
    }

    /// Whether `url` names a subscribable source (user page, channel, playlist…).
    fn feed_matches(&self, _url: &Url) -> bool {
        false
    }

    /// Whether `feed` needs a stored credential; `/sub` refuses without one.
    fn feed_requires_login(&self) -> bool {
        false
    }

    /// Latest entries of a subscribable source, newest first. Every entry URL
    /// must be extractable by this extractor.
    async fn feed(
        &self,
        _ctx: &ExtractCtx,
        _req: &ExtractRequest,
    ) -> Result<Vec<FeedEntry>, ExtractError> {
        Err(ExtractError::Unsupported)
    }

    async fn extract(
        &self,
        ctx: &ExtractCtx,
        req: &ExtractRequest,
    ) -> Result<Extracted, ExtractError>;
}

/// Exact lookup of an extractor by its stable [`Extractor::id`], as stored in
/// jobs, subscriptions, and credentials. No alias or fuzzy matching.
pub trait ExtractorLookup: Send + Sync {
    fn by_id(&self, id: &str) -> Option<Arc<dyn Extractor>>;
}

/// Chat/user/global credential lookup scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    User,
    Chat,
    Global,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Chat => "chat",
            Self::Global => "global",
        }
    }
}
