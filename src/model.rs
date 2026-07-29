use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    X,
    YouTube,
    Pixiv,
    Bilibili,
    Netease,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X => "x",
            Self::YouTube => "youtube",
            Self::Pixiv => "pixiv",
            Self::Bilibili => "bilibili",
            Self::Netease => "netease",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Post,
    Video,
    Artwork,
    Audio,
    Album,
    Playlist,
    Live,
    Article,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Author {
    pub id: String,
    pub name: String,
    pub url: Option<String>,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Photo,
    Video,
    Audio,
    Animation,
    Document,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItem {
    pub kind: MediaKind,
    pub source_url: String,
    #[serde(default)]
    pub fallback_urls: Vec<String>,
    pub thumbnail_url: Option<String>,
    pub filename: String,
    pub mime_type: Option<String>,
    pub duration_secs: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub size: Option<u64>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub cache_key: String,
    #[serde(default)]
    pub requires_download: bool,
    pub secondary_url: Option<String>,
    #[serde(default)]
    pub secondary_fallback_urls: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stats {
    pub likes: Option<u64>,
    pub reposts: Option<u64>,
    pub replies: Option<u64>,
    pub views: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedContent {
    pub platform: Platform,
    pub kind: ContentKind,
    pub id: String,
    pub canonical_url: String,
    pub author: Author,
    pub title: String,
    pub text: String,
    /// Whether the upstream marks this content as adult/sensitive.
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub stats: Stats,
    #[serde(default)]
    pub media: Vec<MediaItem>,
    #[serde(default)]
    pub collection_items: Vec<String>,
}

/// 未指定 `/video` 清晰度时的默认上限：最高不超过 480p。
pub const DEFAULT_VIDEO_QUALITY: u32 = 480;

#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    pub quality: Option<u32>,
    pub file_mode: bool,
    pub cover_only: bool,
    pub force_spoiler: bool,
    /// 1-based 页码；用于 Pixiv 等多图内容只发送指定页。
    pub page: Option<u32>,
    /// 回复 Bot 媒体消息时，由持久化映射指定要发送的原媒体。
    pub media_cache_key: Option<String>,
}

impl ParseOptions {
    pub fn quality_or_default(&self) -> u32 {
        self.quality.unwrap_or(DEFAULT_VIDEO_QUALITY)
    }
}

#[derive(Debug, Clone)]
pub struct ParseRequest {
    pub url: String,
    pub options: ParseOptions,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("不支持的链接: {0}")]
    Unsupported(String),
    #[error("链接格式不正确: {0}")]
    InvalidUrl(String),
    #[error("内容不存在、已删除或不可公开访问: {0}")]
    Unavailable(String),
    #[error("需要登录凭证: {0}")]
    Authentication(String),
    #[error("上游请求受限，请稍后重试: {0}")]
    RateLimited(String),
    #[error("上游服务错误: {0}")]
    Upstream(String),
    #[error("响应解析错误: {0}")]
    InvalidResponse(String),
    #[error("媒体处理错误: {0}")]
    Media(String),
}

pub type ProviderResult<T> = Result<T, ProviderError>;
