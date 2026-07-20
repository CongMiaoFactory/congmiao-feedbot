use std::{
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

#[derive(Debug)]
pub struct PreparedMedia {
    pub item: MediaItem,
    pub path: PathBuf,
    pub force_document: bool,
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
        self.download(url, &source, &Default::default()).await?;
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
        if tokio::fs::metadata(&output).await?.len() > 200 * 1024 {
            let _ = tokio::fs::remove_file(&output).await;
            bail!("音频封面超过 Telegram 200KB 限制");
        }
        Ok(output)
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
        let _permit = self.semaphore.acquire().await?;
        tokio::fs::create_dir_all(&self.temp_dir).await?;
        let safe = unique_temp_key(&item.cache_key);
        let output = self
            .temp_dir
            .join(format!("{safe}-{}", sanitize(&item.filename)));
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
            )
            .await?;
            let conversion = self
                .convert_ugoira(
                    &archive,
                    item.secondary_url.as_deref().unwrap_or("ugoira:[]"),
                    &output,
                )
                .await;
            let _ = tokio::fs::remove_file(archive).await;
            conversion?;
        } else if content.platform == crate::model::Platform::YouTube {
            self.download_youtube(&content.canonical_url, &output, quality)
                .await?;
        } else {
            let primary = self.temp_dir.join(format!("{safe}.part1"));
            self.download_with_fallbacks(
                &item.source_url,
                &item.fallback_urls,
                &primary,
                &item.headers,
            )
            .await?;
            if let Some(second_url) = &item.secondary_url {
                let secondary = self.temp_dir.join(format!("{safe}.part2"));
                let preparation = async {
                    self.download(second_url, &secondary, &item.headers).await?;
                    self.merge(&primary, &secondary, &output).await
                }
                .await;
                let _ = tokio::fs::remove_file(&secondary).await;
                let _ = tokio::fs::remove_file(&primary).await;
                preparation?;
            } else if let Err(error) = tokio::fs::rename(&primary, &output).await {
                let _ = tokio::fs::remove_file(&primary).await;
                return Err(error.into());
            }
        }
        let mut prepared_item = item.clone();
        let mut video_metadata = if item.kind == MediaKind::Video {
            let mut metadata = self.probe_video(&output).await?;
            if !video_is_telegram_compatible(&metadata) {
                self.normalize_video(&output).await?;
                metadata = self.probe_video(&output).await?;
            }
            Some(metadata)
        } else {
            None
        };
        let force_document = if item.kind == MediaKind::Photo {
            let path = output.clone();
            tokio::task::spawn_blocking(move || photo_exceeds_telegram_dimensions(&path))
                .await?
                .unwrap_or(false)
        } else {
            false
        };
        let item_limit =
            if item.kind == MediaKind::Photo && !force_document && self.max_size < 2_000_000_000 {
                10 * 1024 * 1024
            } else {
                self.max_size
            };
        if tokio::fs::metadata(&output).await?.len() > item_limit
            && matches!(item.kind, MediaKind::Video | MediaKind::Animation)
        {
            self.compress_video(&output).await?;
            if item.kind == MediaKind::Video {
                video_metadata = Some(self.probe_video(&output).await?);
            }
        }
        if tokio::fs::metadata(&output).await?.len() > item_limit && item.kind == MediaKind::Photo {
            self.compress_image(&output).await?;
        }
        let size = tokio::fs::metadata(&output).await?.len();
        if size > item_limit {
            let _ = tokio::fs::remove_file(&output).await;
            bail!("媒体文件 {} MB 超过 Telegram 限制", size / 1024 / 1024);
        }
        if let Some(metadata) = video_metadata {
            prepared_item.width = metadata.width.or(prepared_item.width);
            prepared_item.height = metadata.height.or(prepared_item.height);
            prepared_item.duration_secs = metadata.duration_secs.or(prepared_item.duration_secs);
        }
        Ok(PreparedMedia {
            item: prepared_item,
            path: output,
            force_document,
        })
    }

    async fn download_with_fallbacks(
        &self,
        primary_url: &str,
        fallback_urls: &[String],
        target: &Path,
        headers: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        let mut last_error = None;
        for url in std::iter::once(primary_url).chain(fallback_urls.iter().map(String::as_str)) {
            match self.download(url, target, headers).await {
                Ok(()) => return Ok(()),
                Err(error) => {
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
    ) -> Result<()> {
        let attempts = self.download_retries.saturating_add(1);
        let mut last_error = None;
        for attempt in 1..=attempts {
            match self.download_attempt(url, target, headers, attempt).await {
                Ok(()) => return Ok(()),
                Err(error) => {
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
        if expected_total.is_some_and(|total| total > 2_000_000_000) {
            bail!("拒绝下载超过 2GB 的文件");
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
            if total > 2_000_000_000 {
                bail!("拒绝下载超过 2GB 的文件");
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
        let format = format!(
            "bestvideo[height<={quality}][vcodec^=avc1]+bestaudio[acodec^=mp4a]/best[height<={quality}][ext=mp4]/best[height<={quality}]"
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
                "-vf",
                "scale='min(1280,iw)':-2",
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
        tokio::fs::rename(compressed, path).await?;
        Ok(())
    }
    async fn compress_image(&self, path: &Path) -> Result<()> {
        let compressed = path.with_extension("compressed.jpg");
        let out = Command::new(&self.ffmpeg)
            .arg("-y")
            .arg("-i")
            .arg(path)
            .args(["-vf", "scale='min(4096,iw)':-2", "-q:v", "4"])
            .arg(&compressed)
            .output()
            .await?;
        if !out.status.success() {
            bail!(
                "FFmpeg 图片压缩失败: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        tokio::fs::remove_file(path).await?;
        tokio::fs::rename(compressed, path).await?;
        Ok(())
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
                let display = path.display().to_string().replace('\\', "/");
                list.push_str(&format!("file '{display}'\nduration {delay:.3}\n"));
            }
            if let Some(last) = items
                .last()
                .and_then(|value| value.get("file"))
                .and_then(serde_json::Value::as_str)
            {
                let path = directory.join(last).canonicalize()?;
                let display = path.display().to_string().replace('\\', "/");
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

pub fn telegram_photo_dimensions_valid(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && u64::from(width) + u64::from(height) <= 10_000
        && u64::from(width.max(height)) <= u64::from(width.min(height)) * 20
}

fn photo_exceeds_telegram_dimensions(path: &Path) -> Result<bool> {
    let (width, height) = image::ImageReader::open(path)?
        .with_guessed_format()?
        .into_dimensions()?;
    Ok(!telegram_photo_dimensions_valid(width, height))
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
