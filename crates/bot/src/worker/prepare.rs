//! Turning extracted media items into local files ready for upload.

use crate::app::{App, MediaTools};
use feedbot_core::extract::{MediaItem, MediaKind, MediaSource};
use feedbot_core::{MediaError, MediaInfo, TranscodeSpec, YtdlpSpec};
use http::HeaderMap;
use std::path::{Path, PathBuf};
use tokio::task::JoinSet;
use tracing::warn;

pub(super) async fn prepare_all(
    app: &App,
    items: Vec<MediaItem>,
    dir: &Path,
) -> anyhow::Result<(Vec<PreparedMedia>, Vec<String>)> {
    let total = items.len();
    let mut set = JoinSet::new();
    for (idx, item) in items.into_iter().enumerate() {
        let tools = app.pipeline.media.clone();
        let dir = dir.to_path_buf();
        set.spawn(async move {
            let _permit = tools.limit.clone().acquire_owned().await?;
            let outcome = prepare_one(idx, item, &tools, &dir).await;
            anyhow::Ok((idx, outcome))
        });
    }

    let mut prepared = Vec::new();
    let mut skipped = Vec::new();
    let mut first_error: Option<(usize, anyhow::Error)> = None;
    while let Some(joined) = set.join_next().await {
        let (idx, outcome) = joined.map_err(|e| anyhow::anyhow!("download task: {e}"))??;
        match outcome {
            Ok(PrepareOutcome::Ready(item)) => prepared.push(item),
            Ok(PrepareOutcome::Skip(reason)) => skipped.push((idx, reason)),
            Err(e) => {
                warn!(idx, error = %e, "media item failed");
                skipped.push((idx, format!("第 {} 项失败：{e}", idx + 1)));
                if first_error.as_ref().is_none_or(|(i, _)| idx < *i) {
                    first_error = Some((idx, e));
                }
            }
        }
    }
    // If nothing could be prepared, surface the error so the job is retried
    // (download errors) or reported, instead of sending an empty result.
    if prepared.is_empty()
        && total > 0
        && let Some((_, e)) = first_error
    {
        return Err(e);
    }
    prepared.sort_by_key(|p| p.idx);
    skipped.sort_by_key(|(idx, _)| *idx);
    Ok((prepared, skipped.into_iter().map(|(_, r)| r).collect()))
}

enum PrepareOutcome {
    Ready(PreparedMedia),
    Skip(String),
}

pub(super) struct PreparedMedia {
    pub(super) idx: usize,
    pub(super) kind: MediaKind,
    pub(super) path: PathBuf,
    pub(super) filename: Option<String>,
    pub(super) probe: Option<MediaInfo>,
    pub(super) thumbnail: Option<PathBuf>,
}

async fn prepare_one(
    idx: usize,
    item: MediaItem,
    tools: &MediaTools,
    dir: &Path,
) -> anyhow::Result<PrepareOutcome> {
    let MediaTools {
        downloader,
        processor: media,
        ytdlp,
        max_upload_bytes,
        max_photo_bytes,
        ..
    } = tools;
    let mut thumb_url = item.cover.clone();
    let (mut path, mut filename, mut kind) = match item.source {
        MediaSource::Direct { url, headers } => {
            let path = dir.join(safe_name(item.filename.as_deref(), idx));
            downloader.to_file(&url, &headers, &path).await?;
            (path, item.filename, item.kind)
        }
        MediaSource::Separated {
            video,
            audio,
            headers,
        } => {
            let vpath = dir.join(format!("{idx}_v.bin"));
            let apath = dir.join(format!("{idx}_a.bin"));
            let out = dir.join(format!("{idx}_out.mp4"));
            tokio::try_join!(
                downloader.to_file(&video, &headers, &vpath),
                downloader.to_file(&audio, &headers, &apath),
            )?;
            media.remux(&[vpath, apath], &out).await?;
            (out, Some("video.mp4".into()), MediaKind::Video)
        }
        MediaSource::UgoiraZip {
            zip,
            headers,
            frames,
        } => {
            let zip_path = dir.join(format!("{idx}_ugoira.zip"));
            let work = dir.join(format!("{idx}_ugoira"));
            tokio::fs::create_dir_all(&work)
                .await
                .map_err(|e| MediaError::Io(e.to_string()))?;
            let out = dir.join(format!("{idx}_ugoira.mp4"));
            downloader.to_file(&zip, &headers, &zip_path).await?;
            media.ugoira_to_mp4(&zip_path, &frames, &work, &out).await?;
            (out, Some("ugoira.mp4".into()), MediaKind::Animation)
        }
        MediaSource::YtDlp {
            url,
            audio_only,
            max_height,
            thumbnail,
            cookies,
        } => {
            let (ext, kind) = if audio_only {
                ("m4a", MediaKind::Audio)
            } else {
                ("mp4", MediaKind::Video)
            };
            let dest = dir.join(format!("{idx}.{ext}"));
            ytdlp
                .download(
                    &url,
                    &dest,
                    &YtdlpSpec {
                        audio_only,
                        max_height,
                        cookies,
                        max_bytes: Some(*max_upload_bytes),
                    },
                )
                .await?;
            if thumbnail.is_some() {
                thumb_url = thumbnail;
            }
            let name = if audio_only { "audio.m4a" } else { "video.mp4" };
            (dest, Some(name.into()), kind)
        }
    };

    let mut thumbnail = None;
    if let Some(url) = thumb_url {
        let cover = dir.join(format!("{idx}_cover.jpg"));
        if downloader
            .to_file(&url, &HeaderMap::new(), &cover)
            .await
            .is_ok()
        {
            thumbnail = Some(cover);
        }
    }

    let mut size = file_size(&path).await?;
    if kind == MediaKind::Photo && size > *max_photo_bytes {
        kind = MediaKind::Document;
    }

    if matches!(kind, MediaKind::Video | MediaKind::Animation) && size > *max_upload_bytes {
        let tc = dir.join(format!("{idx}_tc.mp4"));
        let spec = TranscodeSpec {
            max_bytes: max_upload_bytes.saturating_sub(1024 * 1024).max(1),
            ..TranscodeSpec::default()
        };
        if let Err(e) = media.transcode(&path, &tc, &spec).await {
            return Ok(PrepareOutcome::Skip(format!(
                "第 {} 项转码失败：{e}",
                idx + 1
            )));
        }
        path = tc;
        filename = Some("video.mp4".into());
        kind = MediaKind::Video;
        size = file_size(&path).await.unwrap_or(size);
    }

    if size > *max_upload_bytes {
        return Ok(PrepareOutcome::Skip(format!(
            "第 {} 项超过上传上限（{size} 字节）",
            idx + 1
        )));
    }

    let probe = if matches!(kind, MediaKind::Video | MediaKind::Animation) {
        media.probe(&path).await.ok()
    } else {
        None
    };
    if thumbnail.is_none() && kind == MediaKind::Video {
        let thumb = dir.join(format!("{idx}_thumb.jpg"));
        if media.thumbnail(&path, &thumb).await.is_ok() {
            thumbnail = Some(thumb);
        }
    }

    Ok(PrepareOutcome::Ready(PreparedMedia {
        idx,
        kind,
        path,
        filename,
        probe,
        thumbnail,
    }))
}

async fn file_size(path: &Path) -> Result<u64, MediaError> {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .map_err(|e| MediaError::Io(e.to_string()))
}

fn safe_name(name: Option<&str>, idx: usize) -> String {
    name.and_then(|n| Path::new(n).file_name())
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| format!("{idx}_{s}"))
        .unwrap_or_else(|| format!("item_{idx}"))
}
