//! Media processing contract (probe / remux / thumbnail / transcode).

use crate::extract::UgoiraFrame;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub duration: Option<Duration>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct TranscodeSpec {
    pub max_bytes: u64,
    pub max_height: u32,
    pub video_bitrate: &'static str,
    pub audio_bitrate: &'static str,
}

impl Default for TranscodeSpec {
    fn default() -> Self {
        Self {
            max_bytes: 49 * 1024 * 1024,
            max_height: 1080,
            video_bitrate: "2500k",
            audio_bitrate: "128k",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct YtdlpSpec {
    pub audio_only: bool,
    pub max_height: Option<u32>,
    pub cookies: Option<String>,
}

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("ffmpeg 未安装或不在 PATH 中")]
    MissingBinary,
    #[error("ffmpeg 失败: {0}")]
    Ffmpeg(String),
    #[error("yt-dlp 未安装或不在 PATH 中")]
    MissingYtdlp,
    #[error("yt-dlp 失败: {0}")]
    Ytdlp(String),
    #[error("下载失败: {0}")]
    Download(String),
    #[error("IO 错误: {0}")]
    Io(String),
}

#[async_trait::async_trait]
pub trait Ytdlp: Send + Sync {
    async fn probe_json(&self, url: &Url, cookies: Option<&str>) -> Result<Value, MediaError>;
    async fn download(&self, url: &Url, dest: &Path, spec: &YtdlpSpec) -> Result<(), MediaError>;
}

#[async_trait::async_trait]
pub trait MediaProcessor: Send + Sync {
    async fn probe(&self, path: &Path) -> Result<MediaInfo, MediaError>;
    async fn remux(&self, inputs: &[PathBuf], output: &Path) -> Result<(), MediaError>;
    async fn thumbnail(&self, input: &Path, output: &Path) -> Result<(), MediaError>;
    async fn transcode(
        &self,
        input: &Path,
        output: &Path,
        spec: &TranscodeSpec,
    ) -> Result<(), MediaError>;
    async fn ugoira_to_mp4(
        &self,
        zip: &Path,
        frames: &[UgoiraFrame],
        work_dir: &Path,
        output: &Path,
    ) -> Result<(), MediaError>;
}
