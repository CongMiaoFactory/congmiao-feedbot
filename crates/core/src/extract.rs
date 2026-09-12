//! Extractor contract, request/response types, and HTTP client trait.

use crate::auth::{AuthProvider, Credential};
use crate::media::Ytdlp;
use crate::options::{OptionError, OptionSpec, OptionValues};
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

#[derive(Debug, Clone)]
pub enum ManifestKind {
    Hls,
    Dash,
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
    Segmented {
        manifest: Url,
        kind: ManifestKind,
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
}

#[derive(Debug, Clone, Copy)]
pub enum HttpMethod {
    Get,
    Head,
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

#[async_trait::async_trait]
pub trait Extractor: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }
    fn priority(&self) -> i32 {
        0
    }
    fn matches(&self, url: &Url) -> bool;

    fn options(&self) -> &'static OptionSpec {
        &OptionSpec::EMPTY
    }
    fn validate(&self, _opts: &mut OptionValues) -> Result<(), OptionError> {
        Ok(())
    }
    fn auth(&self) -> Option<&dyn AuthProvider> {
        None
    }

    /// Human-readable extractable work types, e.g. `插画`, `动图 ugoira`.
    fn extract_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    /// Short URL shapes this extractor accepts.
    fn url_examples(&self) -> &'static [&'static str] {
        &[]
    }

    async fn extract(
        &self,
        ctx: &ExtractCtx,
        req: &ExtractRequest,
    ) -> Result<Extracted, ExtractError>;
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
