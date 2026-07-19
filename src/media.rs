use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use reqwest::Client;
use tokio::{fs::File, io::AsyncWriteExt, process::Command, sync::Semaphore};

use crate::{
    Config,
    model::{MediaItem, MediaKind, ParsedContent},
};

#[derive(Debug)]
pub struct PreparedMedia {
    pub item: MediaItem,
    pub path: PathBuf,
}

#[derive(Clone)]
pub struct MediaProcessor {
    client: Client,
    ffmpeg: String,
    yt_dlp: String,
    youtube_cookies: Option<PathBuf>,
    temp_dir: PathBuf,
    semaphore: Arc<Semaphore>,
    max_size: u64,
}

impl MediaProcessor {
    pub fn new(client: Client, config: &Config) -> Self {
        Self {
            client,
            ffmpeg: config.ffmpeg_path.clone(),
            yt_dlp: config.yt_dlp_path.clone(),
            youtube_cookies: config.youtube_cookies_file.clone(),
            temp_dir: config.temp_dir.clone(),
            semaphore: Arc::new(Semaphore::new(config.upload_workers.max(1))),
            max_size: if config.local_bot_api {
                2_000_000_000
            } else {
                50 * 1024 * 1024
            },
        }
    }
    pub fn max_size(&self) -> u64 {
        self.max_size
    }

    pub async fn prepare(
        &self,
        content: &ParsedContent,
        item: &MediaItem,
        quality: u32,
    ) -> Result<PreparedMedia> {
        let _permit = self.semaphore.acquire().await?;
        tokio::fs::create_dir_all(&self.temp_dir).await?;
        let safe = item.cache_key.replace([':', '/', '\\'], "-");
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
            self.download(&item.source_url, &archive, &item.headers)
                .await?;
            self.convert_ugoira(
                &archive,
                item.secondary_url.as_deref().unwrap_or("ugoira:[]"),
                &output,
            )
            .await?;
            let _ = tokio::fs::remove_file(archive).await;
        } else if content.platform == crate::model::Platform::YouTube {
            self.download_youtube(&content.canonical_url, &output, quality)
                .await?;
        } else {
            let primary = self.temp_dir.join(format!("{safe}.part1"));
            self.download(&item.source_url, &primary, &item.headers)
                .await?;
            if let Some(second_url) = &item.secondary_url {
                let secondary = self.temp_dir.join(format!("{safe}.part2"));
                self.download(second_url, &secondary, &item.headers).await?;
                self.merge(&primary, &secondary, &output).await?;
                let _ = tokio::fs::remove_file(secondary).await;
                let _ = tokio::fs::remove_file(primary).await;
            } else {
                tokio::fs::rename(&primary, &output).await?;
            }
        }
        let item_limit = if item.kind == MediaKind::Photo && self.max_size < 2_000_000_000 {
            10 * 1024 * 1024
        } else {
            self.max_size
        };
        if tokio::fs::metadata(&output).await?.len() > item_limit
            && matches!(item.kind, MediaKind::Video | MediaKind::Animation)
        {
            self.compress_video(&output).await?;
        }
        if tokio::fs::metadata(&output).await?.len() > item_limit && item.kind == MediaKind::Photo {
            self.compress_image(&output).await?;
        }
        let size = tokio::fs::metadata(&output).await?.len();
        if size > item_limit {
            let _ = tokio::fs::remove_file(&output).await;
            bail!("媒体文件 {} MB 超过 Telegram 限制", size / 1024 / 1024);
        }
        Ok(PreparedMedia {
            item: item.clone(),
            path: output,
        })
    }

    async fn download(
        &self,
        url: &str,
        target: &Path,
        headers: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        let mut request = self.client.get(url);
        for (k, v) in headers {
            request = request.header(k, v);
        }
        let response = request.send().await?.error_for_status()?;
        let mut file = File::create(target).await?;
        let mut stream = response.bytes_stream();
        let mut total = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            total += chunk.len() as u64;
            if total > 2_000_000_000 {
                bail!("拒绝下载超过 2GB 的文件");
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
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
            .arg("-y")
            .arg("-i")
            .arg(video)
            .arg("-i")
            .arg(audio)
            .args(["-c", "copy", "-movflags", "+faststart"])
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
            .arg("-y")
            .arg("-i")
            .arg(path)
            .args([
                "-vf",
                "scale='min(1280,iw)':-2",
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "28",
                "-c:a",
                "aac",
                "-b:a",
                "96k",
                "-movflags",
                "+faststart",
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
