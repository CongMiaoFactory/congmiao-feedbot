use std::{
    fmt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use reqwest::{
    Client, StatusCode,
    header::{ACCEPT_ENCODING, CONTENT_RANGE, RANGE},
};
use serde::Deserialize;
use tokio::{
    fs::{File, OpenOptions},
    io::AsyncWriteExt,
    process::Command,
    sync::Semaphore,
    time::{Duration, sleep},
};
use tracing::{info, warn};

use crate::{
    Config,
    model::{MediaItem, MediaKind, ParsedContent},
};

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

const TELEGRAM_PHOTO_MAX_SIZE: u64 = 10 * 1024 * 1024;
const TELEGRAM_PHOTO_PREVIEW_TARGET_SIZE: u64 = 9 * 1024 * 1024;
const TELEGRAM_PHOTO_PREVIEW_MAX_EDGE: u32 = 4096;
const TELEGRAM_PHOTO_PREVIEW_MAX_RATIO: u32 = 19;

#[derive(Debug)]
struct MediaTooLarge {
    max_size: u64,
}

impl fmt::Display for MediaTooLarge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "媒体超过 {}MB 上传限制",
            self.max_size / 1024 / 1024
        )
    }
}

impl std::error::Error for MediaTooLarge {}

/// 判断错误是否属于无需刷新上游地址的永久失败，例如媒体超过 Telegram 上传限制。
pub fn is_permanent_prepare_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<MediaTooLarge>().is_some()
        || error
            .chain()
            .any(|source| source.downcast_ref::<MediaTooLarge>().is_some())
}

#[derive(Debug)]
pub struct PreparedMedia {
    pub item: MediaItem,
    pub path: PathBuf,
    pub is_preview: bool,
}

#[derive(Debug, Default)]
struct VideoMetadata {
    width: Option<u32>,
    height: Option<u32>,
    duration_secs: Option<u64>,
    format_name: String,
    video_codec: String,
    pixel_format: String,
    audio_codec: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: ProbeFormat,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    pix_fmt: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    duration: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ProbeFormat {
    format_name: Option<String>,
    duration: Option<String>,
}

#[derive(Clone)]
pub struct MediaProcessor {
    client: Client,
    ffmpeg: String,
    ffprobe: String,
    yt_dlp: String,
    youtube_cookies: Option<PathBuf>,
    temp_dir: PathBuf,
    semaphore: Arc<Semaphore>,
    max_size: u64,
    photo_source_max_size: u64,
    download_retries: usize,
}

impl MediaProcessor {
    pub fn new(client: Client, config: &Config) -> Self {
        Self {
            client,
            ffmpeg: config.ffmpeg_path.clone(),
            ffprobe: config.ffprobe_path.clone(),
            yt_dlp: config.yt_dlp_path.clone(),
            youtube_cookies: config.youtube_cookies_file.clone(),
            temp_dir: config.temp_dir.clone(),
            semaphore: Arc::new(Semaphore::new(config.upload_workers.max(1))),
            max_size: if config.local_bot_api {
                2_000_000_000
            } else {
                50 * 1024 * 1024
            },
            photo_source_max_size: config
                .media_photo_source_max_size_mb
                .saturating_mul(1024 * 1024),
            download_retries: config.media_download_retries,
        }
    }
    pub fn max_size(&self) -> u64 {
        self.max_size
    }

    pub async fn prepare_thumbnail(&self, url: &str, cache_key: &str) -> Result<PathBuf> {
        let _permit = self.semaphore.acquire().await?;
        tokio::fs::create_dir_all(&self.temp_dir).await?;
        let safe = unique_temp_key(cache_key);
        let source = self.temp_dir.join(format!("{safe}-cover.part"));
        let output = self.temp_dir.join(format!("{safe}-cover.jpg"));
        self.download(
            url,
            &source,
            &Default::default(),
            self.max_size.min(50 * 1024 * 1024),
        )
        .await?;
        let result = Command::new(&self.ffmpeg)
            .arg("-y")
            .arg("-i")
            .arg(&source)
            .args([
                "-vf",
                "scale=320:320:force_original_aspect_ratio=decrease",
                "-frames:v",
                "1",
                "-q:v",
                "5",
            ])
            .arg(&output)
            .output()
            .await?;
        let _ = tokio::fs::remove_file(source).await;
        if !result.status.success() {
            bail!(
                "FFmpeg 封面处理失败: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        ensure_thumbnail_size(&output).await?;
        Ok(output)
    }

    /// 从本地视频抽一帧作为 Telegram 缩略图，避免再下载远程封面。
    pub async fn prepare_video_thumbnail(
        &self,
        video_path: &Path,
        cache_key: &str,
    ) -> Result<PathBuf> {
        let _permit = self.semaphore.acquire().await?;
        tokio::fs::create_dir_all(&self.temp_dir).await?;
        let safe = unique_temp_key(cache_key);
        let output = self.temp_dir.join(format!("{safe}-vcover.jpg"));
        // 优先取 1 秒处画面；极短视频再回退到开头。
        for seek in ["00:00:01.000", "00:00:00.000"] {
            let result = Command::new(&self.ffmpeg)
                .args(["-y", "-ss", seek, "-i"])
                .arg(video_path)
                .args([
                    "-vf",
                    "scale=320:320:force_original_aspect_ratio=decrease",
                    "-frames:v",
                    "1",
                    "-q:v",
                    "5",
                ])
                .arg(&output)
                .output()
                .await?;
            if result.status.success()
                && tokio::fs::metadata(&output)
                    .await
                    .map(|meta| meta.len() > 0)
                    .unwrap_or(false)
            {
                ensure_thumbnail_size(&output).await?;
                return Ok(output);
            }
        }
        let _ = tokio::fs::remove_file(&output).await;
        bail!("无法从本地视频提取缩略图")
    }

    pub async fn cleanup_path(&self, path: impl AsRef<Path>) {
        let _ = tokio::fs::remove_file(path).await;
    }

    pub async fn prepare(
        &self,
        content: &ParsedContent,
        item: &MediaItem,
        quality: u32,
    ) -> Result<PreparedMedia> {
        self.prepare_with_photo_mode(content, item, quality, false)
            .await
    }

    pub async fn prepare_original(
        &self,
        content: &ParsedContent,
        item: &MediaItem,
        quality: u32,
    ) -> Result<PreparedMedia> {
        self.prepare_with_photo_mode(content, item, quality, true)
            .await
    }

    async fn prepare_with_photo_mode(
        &self,
        content: &ParsedContent,
        item: &MediaItem,
        quality: u32,
        preserve_photo_original: bool,
    ) -> Result<PreparedMedia> {
        let _permit = self.semaphore.acquire().await?;
        tokio::fs::create_dir_all(&self.temp_dir).await?;
        let safe = unique_temp_key(&item.cache_key);
        let output = self
            .temp_dir
            .join(format!("{safe}-{}", sanitize(&item.filename)));
        let prepared = self
            .prepare_into(
                content,
                item,
                quality,
                &safe,
                &output,
                preserve_photo_original,
            )
            .await;
        if prepared.is_err() {
            // 下载或转码失败时不留半成品，temp 目录不随错误累积垃圾。
            let _ = tokio::fs::remove_file(&output).await;
            let _ = tokio::fs::remove_file(output.with_extension("telegram-preview.jpg")).await;
        }
        prepared
    }

    async fn prepare_into(
        &self,
        content: &ParsedContent,
        item: &MediaItem,
        quality: u32,
        safe: &str,
        output: &Path,
        preserve_photo_original: bool,
    ) -> Result<PreparedMedia> {
        let download_limit = match item.kind {
            MediaKind::Video | MediaKind::Animation => 2_000_000_000,
            MediaKind::Photo if !preserve_photo_original => self.photo_source_max_size,
            _ => self.max_size,
        };
        let mut used_photo_thumbnail = false;
        if item.kind == MediaKind::Animation
            && item
                .secondary_url
                .as_deref()
                .is_some_and(|value| value.starts_with("ugoira:"))
        {
            let archive = self.temp_dir.join(format!("{safe}.zip"));
            self.download_with_fallbacks(
                &item.source_url,
                &item.fallback_urls,
                &archive,
                &item.headers,
                download_limit,
            )
            .await?;
            let conversion = self
                .convert_ugoira(
                    &archive,
                    item.secondary_url.as_deref().unwrap_or("ugoira:[]"),
                    output,
                )
                .await;
            let _ = tokio::fs::remove_file(archive).await;
            conversion?;
        } else if content.platform == crate::model::Platform::YouTube {
            self.download_youtube(&content.canonical_url, output, quality)
                .await?;
        } else {
            let primary = self.temp_dir.join(format!("{safe}.part1"));
            let primary_download = self
                .download_with_fallbacks(
                    &item.source_url,
                    &item.fallback_urls,
                    &primary,
                    &item.headers,
                    download_limit,
                )
                .await;
            if let Err(error) = primary_download {
                let can_use_thumbnail = item.kind == MediaKind::Photo
                    && !preserve_photo_original
                    && is_permanent_prepare_error(&error)
                    && item
                        .thumbnail_url
                        .as_deref()
                        .is_some_and(|url| url != item.source_url);
                if can_use_thumbnail {
                    let thumbnail_url = item.thumbnail_url.as_deref().expect("checked above");
                    warn!(
                        error = %error,
                        "原图超过预览下载上限，改用上游预览图"
                    );
                    self.download(
                        thumbnail_url,
                        &primary,
                        &item.headers,
                        self.max_size.min(self.photo_source_max_size),
                    )
                    .await?;
                    used_photo_thumbnail = true;
                } else {
                    return Err(error);
                }
            }
            if let Some(second_url) = &item.secondary_url {
                let secondary = self.temp_dir.join(format!("{safe}.part2"));
                let preparation = async {
                    self.download_with_fallbacks(
                        second_url,
                        &item.secondary_fallback_urls,
                        &secondary,
                        &item.headers,
                        download_limit,
                    )
                    .await?;
                    self.merge(&primary, &secondary, output).await
                }
                .await;
                let _ = tokio::fs::remove_file(&secondary).await;
                let _ = tokio::fs::remove_file(&primary).await;
                preparation?;
            } else if let Err(error) = tokio::fs::rename(&primary, output).await {
                let _ = tokio::fs::remove_file(&primary).await;
                return Err(error.into());
            }
        }
        let mut prepared_item = item.clone();
        let mut prepared_path = output.to_path_buf();
        let mut is_preview = false;
        let mut video_metadata = if item.kind == MediaKind::Video {
            let mut metadata = self.probe_video(output).await?;
            if !video_is_telegram_compatible(&metadata) {
                self.normalize_video(output).await?;
                metadata = self.probe_video(output).await?;
            }
            Some(metadata)
        } else {
            None
        };
        if item.kind == MediaKind::Photo && !preserve_photo_original {
            let (width, height) = photo_dimensions(output)?;
            let source_size = tokio::fs::metadata(output).await?.len();
            if source_size > TELEGRAM_PHOTO_MAX_SIZE
                || !telegram_photo_dimensions_valid(width, height)
            {
                let (preview_path, preview_width, preview_height) =
                    self.render_photo_preview(output, width, height).await?;
                prepared_path = preview_path;
                prepared_item.width = Some(preview_width);
                prepared_item.height = Some(preview_height);
                prepared_item.filename = preview_filename(&item.filename);
                prepared_item.mime_type = Some("image/jpeg".into());
                prepared_item.cache_key = format!("{}:telegram-photo-preview-v1", item.cache_key);
                is_preview = true;
            } else {
                prepared_item.width = Some(width);
                prepared_item.height = Some(height);
                if used_photo_thumbnail {
                    prepared_item.cache_key =
                        format!("{}:telegram-photo-preview-v1", item.cache_key);
                    is_preview = true;
                }
            }
            let (final_width, final_height) = photo_dimensions(&prepared_path)?;
            let final_size = tokio::fs::metadata(&prepared_path).await?.len();
            if final_size > TELEGRAM_PHOTO_MAX_SIZE
                || !telegram_photo_dimensions_valid(final_width, final_height)
            {
                bail!(
                    "图片预览仍不符合 Telegram 限制：{}x{}，{}MB",
                    final_width,
                    final_height,
                    final_size / 1024 / 1024
                );
            }
        }
        let item_limit = if item.kind == MediaKind::Photo && !preserve_photo_original {
            TELEGRAM_PHOTO_MAX_SIZE
        } else {
            self.max_size
        };
        if tokio::fs::metadata(&prepared_path).await?.len() > item_limit
            && matches!(item.kind, MediaKind::Video | MediaKind::Animation)
        {
            self.compress_video(&prepared_path).await?;
            if item.kind == MediaKind::Video {
                video_metadata = Some(self.probe_video(&prepared_path).await?);
            }
        }
        let size = tokio::fs::metadata(&prepared_path).await?.len();
        if size > item_limit {
            bail!("媒体文件 {} MB 超过 Telegram 限制", size / 1024 / 1024);
        }
        if let Some(metadata) = video_metadata {
            prepared_item.width = metadata.width.or(prepared_item.width);
            prepared_item.height = metadata.height.or(prepared_item.height);
            prepared_item.duration_secs = metadata.duration_secs.or(prepared_item.duration_secs);
        }
        Ok(PreparedMedia {
            item: prepared_item,
            path: prepared_path,
            is_preview,
        })
    }

    async fn download_with_fallbacks(
        &self,
        primary_url: &str,
        fallback_urls: &[String],
        target: &Path,
        headers: &std::collections::BTreeMap<String, String>,
        download_limit: u64,
    ) -> Result<()> {
        let mut last_error = None;
        for url in std::iter::once(primary_url).chain(fallback_urls.iter().map(String::as_str)) {
            match self.download(url, target, headers, download_limit).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if error.downcast_ref::<MediaTooLarge>().is_some() {
                        return Err(error);
                    }
                    warn!(host = %url_host(url), error = %error, "媒体地址下载失败，尝试备用地址");
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("没有可用的媒体下载地址")))
    }

    async fn download(
        &self,
        url: &str,
        target: &Path,
        headers: &std::collections::BTreeMap<String, String>,
        download_limit: u64,
    ) -> Result<()> {
        let attempts = self.download_retries.saturating_add(1);
        let mut last_error = None;
        for attempt in 1..=attempts {
            match self
                .download_attempt(url, target, headers, attempt, download_limit)
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if error.downcast_ref::<MediaTooLarge>().is_some() {
                        let _ = tokio::fs::remove_file(target).await;
                        return Err(error);
                    }
                    let downloaded = tokio::fs::metadata(target)
                        .await
                        .map(|metadata| metadata.len())
                        .unwrap_or_default();
                    warn!(
                        host = %url_host(url),
                        attempt,
                        attempts,
                        downloaded,
                        error = %error,
                        "媒体下载尝试失败"
                    );
                    last_error = Some(error);
                    if attempt < attempts {
                        sleep(Duration::from_secs(1_u64 << (attempt - 1).min(3))).await;
                    }
                }
            }
        }
        let _ = tokio::fs::remove_file(target).await;
        Err(last_error.unwrap_or_else(|| anyhow!("媒体下载失败")))
    }

    async fn download_attempt(
        &self,
        url: &str,
        target: &Path,
        headers: &std::collections::BTreeMap<String, String>,
        attempt: usize,
        download_limit: u64,
    ) -> Result<()> {
        let existing = tokio::fs::metadata(target)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        let mut request = self.client.get(url).header(ACCEPT_ENCODING, "identity");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if existing > 0 {
            request = request.header(RANGE, format!("bytes={existing}-"));
        }
        let response = request.send().await?;
        let status = response.status();
        if status == StatusCode::RANGE_NOT_SATISFIABLE
            && content_range_total(response.headers().get(CONTENT_RANGE)) == Some(existing)
        {
            info!(host = %url_host(url), attempt, downloaded = existing, "媒体续传已完整");
            return Ok(());
        }
        if !status.is_success() {
            return Err(response.error_for_status().unwrap_err().into());
        }

        let resumed = existing > 0 && status == StatusCode::PARTIAL_CONTENT;
        if resumed && content_range_start(response.headers().get(CONTENT_RANGE)) != Some(existing) {
            bail!("服务器返回的 Content-Range 与本地文件不一致");
        }
        let base = if resumed { existing } else { 0 };
        let expected_total =
            content_range_total(response.headers().get(CONTENT_RANGE)).or_else(|| {
                response
                    .content_length()
                    .map(|length| base.saturating_add(length))
            });
        if expected_total.is_some_and(|total| total > download_limit) {
            return Err(MediaTooLarge {
                max_size: download_limit,
            }
            .into());
        }
        let mut file = if resumed {
            OpenOptions::new().append(true).open(target).await?
        } else {
            File::create(target).await?
        };
        let mut total = base;
        info!(
            host = %url_host(url),
            attempt,
            offset = base,
            status = status.as_u16(),
            expected_total,
            "开始媒体下载"
        );
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            total = total.saturating_add(chunk.len() as u64);
            if total > download_limit {
                drop(file);
                let _ = tokio::fs::remove_file(target).await;
                return Err(MediaTooLarge {
                    max_size: download_limit,
                }
                .into());
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        if expected_total.is_some_and(|expected| expected != total) {
            bail!("媒体长度不完整：期望 {expected_total:?} 字节，实际下载 {total} 字节");
        }
        info!(host = %url_host(url), attempt, downloaded = total, "媒体下载完成");
        Ok(())
    }
    async fn probe_video(&self, path: &Path) -> Result<VideoMetadata> {
        let output = Command::new(&self.ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=format_name,duration:stream=codec_type,codec_name,pix_fmt,width,height,duration",
                "-of",
                "json",
            ])
            .arg(path)
            .output()
            .await
            .context("无法启动 ffprobe")?;
        if !output.status.success() {
            bail!(
                "ffprobe 媒体探测失败: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let probe: ProbeOutput = serde_json::from_slice(&output.stdout)?;
        let video = probe
            .streams
            .iter()
            .find(|stream| stream.codec_type.as_deref() == Some("video"))
            .ok_or_else(|| anyhow!("媒体文件缺少视频流"))?;
        let audio_codec = probe
            .streams
            .iter()
            .find(|stream| stream.codec_type.as_deref() == Some("audio"))
            .and_then(|stream| stream.codec_name.clone());
        let duration_secs = video
            .duration
            .as_deref()
            .and_then(parse_duration)
            .or_else(|| probe.format.duration.as_deref().and_then(parse_duration));
        Ok(VideoMetadata {
            width: video.width,
            height: video.height,
            duration_secs,
            format_name: probe.format.format_name.unwrap_or_default(),
            video_codec: video.codec_name.clone().unwrap_or_default(),
            pixel_format: video.pix_fmt.clone().unwrap_or_default(),
            audio_codec,
        })
    }

    async fn normalize_video(&self, path: &Path) -> Result<()> {
        let normalized = path.with_extension("normalized.mp4");
        let output = Command::new(&self.ffmpeg)
            .args(["-y", "-fflags", "+genpts", "-i"])
            .arg(path)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "0:a:0?",
                "-vf",
                "scale=trunc(iw/2)*2:trunc(ih/2)*2",
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "23",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-b:a",
                "128k",
                "-movflags",
                "+faststart",
                "-avoid_negative_ts",
                "make_zero",
            ])
            .arg(&normalized)
            .output()
            .await
            .context("无法启动 FFmpeg")?;
        if !output.status.success() {
            bail!(
                "FFmpeg 视频兼容性转换失败: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        tokio::fs::remove_file(path).await?;
        tokio::fs::rename(normalized, path).await?;
        Ok(())
    }

    async fn download_youtube(&self, url: &str, target: &Path, quality: u32) -> Result<()> {
        // 优先整段 progressive MP4，避免音视频分离后再合并，加快发送链路。
        let format = format!(
            "best[height<={quality}][ext=mp4][vcodec^=avc1][acodec^=mp4a]/bestvideo[height<={quality}][vcodec^=avc1]+bestaudio[acodec^=mp4a]/best[height<={quality}][ext=mp4]/best[height<={quality}]"
        );
        let mut cmd = Command::new(&self.yt_dlp);
        cmd.args([
            "--no-playlist",
            "--no-warnings",
            "--merge-output-format",
            "mp4",
            "-f",
            &format,
            "-o",
        ])
        .arg(target)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
        if let Some(path) = &self.youtube_cookies {
            cmd.arg("--cookies").arg(path);
        }
        let out = cmd.output().await.context("无法启动 yt-dlp")?;
        if !out.status.success() {
            return Err(anyhow!(
                "yt-dlp: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
    async fn merge(&self, video: &Path, audio: &Path, output: &Path) -> Result<()> {
        let out = Command::new(&self.ffmpeg)
            .args(["-y", "-fflags", "+genpts", "-i"])
            .arg(video)
            .arg("-i")
            .arg(audio)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-c",
                "copy",
                "-movflags",
                "+faststart",
                "-avoid_negative_ts",
                "make_zero",
            ])
            .arg(output)
            .output()
            .await
            .context("无法启动 FFmpeg")?;
        if !out.status.success() {
            bail!("FFmpeg 合并失败: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(())
    }
    async fn compress_video(&self, path: &Path) -> Result<()> {
        let compressed = path.with_extension("compressed.mp4");
        let out = Command::new(&self.ffmpeg)
            .args(["-y", "-fflags", "+genpts", "-i"])
            .arg(path)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "0:a:0?",
                // 超限压缩目标贴近默认发送清晰度，减少转码耗时与体积。
                "-vf",
                "scale='min(854,iw)':-2",
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "28",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-b:a",
                "96k",
                "-movflags",
                "+faststart",
                "-avoid_negative_ts",
                "make_zero",
            ])
            .arg(&compressed)
            .output()
            .await?;
        if !out.status.success() {
            bail!("FFmpeg 压缩失败: {}", String::from_utf8_lossy(&out.stderr));
        }
        // Windows 上 rename 覆盖已存在目标会失败，先删原文件。
        let _ = tokio::fs::remove_file(path).await;
        tokio::fs::rename(compressed, path).await?;
        Ok(())
    }
    async fn render_photo_preview(
        &self,
        path: &Path,
        width: u32,
        height: u32,
    ) -> Result<(PathBuf, u32, u32)> {
        let preview = path.with_extension("telegram-preview.jpg");
        let attempts = [
            (TELEGRAM_PHOTO_PREVIEW_MAX_EDGE, 3_u8),
            (3072_u32, 5_u8),
            (2048_u32, 7_u8),
        ];
        let mut last_error = None;
        for (max_edge, quality) in attempts {
            let (scaled_width, scaled_height, canvas_width, canvas_height) =
                photo_preview_dimensions(width, height, max_edge)?;
            let filter = format!(
                "scale={scaled_width}:{scaled_height}:flags=lanczos,pad={canvas_width}:{canvas_height}:(ow-iw)/2:(oh-ih)/2:black"
            );
            let output = Command::new(&self.ffmpeg)
                .arg("-y")
                .arg("-i")
                .arg(path)
                .args([
                    "-map_metadata",
                    "-1",
                    "-vf",
                    &filter,
                    "-frames:v",
                    "1",
                    "-q:v",
                ])
                .arg(quality.to_string())
                .arg(&preview)
                .output()
                .await
                .context("无法启动 FFmpeg 生成图片预览")?;
            if !output.status.success() {
                last_error = Some(anyhow!(
                    "FFmpeg 图片预览失败: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
                continue;
            }
            let size = tokio::fs::metadata(&preview).await?.len();
            let (actual_width, actual_height) = photo_dimensions(&preview)?;
            if size <= TELEGRAM_PHOTO_PREVIEW_TARGET_SIZE
                && telegram_photo_dimensions_valid(actual_width, actual_height)
            {
                tokio::fs::remove_file(path).await?;
                return Ok((preview, actual_width, actual_height));
            }
            last_error = Some(anyhow!(
                "图片预览输出仍超限：{}x{}，{}MB",
                actual_width,
                actual_height,
                size / 1024 / 1024
            ));
        }
        let _ = tokio::fs::remove_file(&preview).await;
        Err(last_error.unwrap_or_else(|| anyhow!("无法生成 Telegram 图片预览")))
    }
    async fn convert_ugoira(&self, archive: &Path, metadata: &str, output: &Path) -> Result<()> {
        let frames: serde_json::Value =
            serde_json::from_str(metadata.strip_prefix("ugoira:").unwrap_or("[]"))?;
        let directory = archive.with_extension("frames");
        tokio::fs::create_dir_all(&directory).await?;
        let archive_owned = archive.to_path_buf();
        let directory_owned = directory.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let file = std::fs::File::open(archive_owned)?;
            let mut zip = zip::ZipArchive::new(file)?;
            zip.extract(directory_owned)?;
            Ok(())
        })
        .await??;

        let list_path = directory.join("concat.txt");
        let mut list = String::new();
        if let Some(items) = frames.as_array() {
            for frame in items {
                let Some(name) = frame.get("file").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let delay = frame
                    .get("delay")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(60) as f64
                    / 1000.0;
                let path = directory.join(name).canonicalize()?;
                let display = concat_escape(&path);
                list.push_str(&format!("file '{display}'\nduration {delay:.3}\n"));
            }
            if let Some(last) = items
                .last()
                .and_then(|value| value.get("file"))
                .and_then(serde_json::Value::as_str)
            {
                let path = directory.join(last).canonicalize()?;
                let display = concat_escape(&path);
                list.push_str(&format!("file '{display}'\n"));
            }
        }
        tokio::fs::write(&list_path, list).await?;
        let result = Command::new(&self.ffmpeg)
            .arg("-y")
            .args(["-f", "concat", "-safe", "0", "-i"])
            .arg(&list_path)
            .args([
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart",
            ])
            .arg(output)
            .output()
            .await?;
        let _ = tokio::fs::remove_dir_all(directory).await;
        if !result.status.success() {
            bail!(
                "FFmpeg ugoira 转换失败: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        Ok(())
    }
    pub async fn cleanup(&self, prepared: PreparedMedia) {
        let _ = tokio::fs::remove_file(prepared.path).await;
    }
}

async fn ensure_thumbnail_size(path: &Path) -> Result<()> {
    if tokio::fs::metadata(path).await?.len() > 200 * 1024 {
        let _ = tokio::fs::remove_file(path).await;
        bail!("媒体封面超过 Telegram 200KB 限制");
    }
    Ok(())
}

/// FFmpeg concat 列表的单引号字符串中，`'` 必须写成 `'\''`。
fn concat_escape(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "/")
        .replace('\'', r"'\''")
}

fn parse_duration(value: &str) -> Option<u64> {
    let seconds = value.parse::<f64>().ok()?;
    (seconds.is_finite() && seconds > 0.0).then(|| seconds.ceil() as u64)
}

fn video_is_telegram_compatible(metadata: &VideoMetadata) -> bool {
    metadata.format_name.split(',').any(|name| name == "mp4")
        && metadata.video_codec == "h264"
        && matches!(metadata.pixel_format.as_str(), "yuv420p" | "yuvj420p")
        && metadata
            .audio_codec
            .as_deref()
            .is_none_or(|codec| codec == "aac")
        && metadata.width.is_some_and(|width| width > 0)
        && metadata.height.is_some_and(|height| height > 0)
        && metadata.duration_secs.is_some_and(|duration| duration > 0)
}

fn photo_dimensions(path: &Path) -> Result<(u32, u32)> {
    Ok(image::ImageReader::open(path)?
        .with_guessed_format()?
        .into_dimensions()?)
}

fn photo_preview_dimensions(
    width: u32,
    height: u32,
    max_edge: u32,
) -> Result<(u32, u32, u32, u32)> {
    if width == 0 || height == 0 || max_edge == 0 {
        bail!("图片尺寸无效：{width}x{height}");
    }
    let longest = width.max(height);
    let (scaled_width, scaled_height) = if longest > max_edge {
        let scaled_width =
            (u64::from(width) * u64::from(max_edge) / u64::from(longest)).max(1) as u32;
        let scaled_height =
            (u64::from(height) * u64::from(max_edge) / u64::from(longest)).max(1) as u32;
        (scaled_width, scaled_height)
    } else {
        (width, height)
    };
    let mut canvas_width = scaled_width;
    let mut canvas_height = scaled_height;
    if u64::from(scaled_height) > u64::from(scaled_width) * TELEGRAM_PHOTO_PREVIEW_MAX_RATIO as u64
    {
        canvas_width = scaled_height.div_ceil(TELEGRAM_PHOTO_PREVIEW_MAX_RATIO);
    } else if u64::from(scaled_width)
        > u64::from(scaled_height) * TELEGRAM_PHOTO_PREVIEW_MAX_RATIO as u64
    {
        canvas_height = scaled_width.div_ceil(TELEGRAM_PHOTO_PREVIEW_MAX_RATIO);
    }
    Ok((scaled_width, scaled_height, canvas_width, canvas_height))
}

fn preview_filename(filename: &str) -> String {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("photo");
    format!("{stem}-preview.jpg")
}

pub fn telegram_photo_dimensions_valid(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && u64::from(width) + u64::from(height) <= 10_000
        && u64::from(width.max(height)) <= u64::from(width.min(height)) * 20
}

fn content_range_start(value: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    let range = value?.to_str().ok()?.strip_prefix("bytes ")?;
    range.split_once('-')?.0.parse().ok()
}

fn content_range_total(value: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    value?.to_str().ok()?.rsplit_once('/')?.1.parse().ok()
}

fn url_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

fn unique_temp_key(cache_key: &str) -> String {
    let safe = cache_key.replace([':', '/', '\\'], "-");
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{safe}-{sequence}")
}

fn sanitize(name: &str) -> String {
    let value: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || ".-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if value.is_empty() {
        "media.bin".into()
    } else {
        value
    }
}
