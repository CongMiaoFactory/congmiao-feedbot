use feedbot_core::{MediaError, MediaInfo, MediaProcessor, TranscodeSpec, UgoiraFrame};
use serde::Deserialize;
use std::fs::File;
use std::io::{copy, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use zip::ZipArchive;

#[derive(Clone)]
pub struct FfmpegCli {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

impl FfmpegCli {
    pub fn new(ffmpeg: impl Into<PathBuf>, ffprobe: impl Into<PathBuf>) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            ffprobe: ffprobe.into(),
        }
    }

    async fn run(bin: &Path, args: &[&str]) -> Result<String, MediaError> {
        let output = Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    MediaError::MissingBinary
                } else {
                    MediaError::Ffmpeg(e.to_string())
                }
            })?;
        if !output.status.success() {
            return Err(MediaError::Ffmpeg(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Concat demuxer list. The last frame is repeated because ffmpeg ignores the
/// duration of the final `file` entry.
pub(crate) fn build_concat_list(frames: &[UgoiraFrame]) -> String {
    let mut out = String::from("ffconcat version 1.0\n");
    for frame in frames {
        let name = concat_file_name(&frame.file);
        out.push_str("file ");
        out.push_str(&name);
        out.push('\n');
        out.push_str(&format!("duration {:.3}\n", frame.delay_ms as f64 / 1000.0));
    }
    if let Some(last) = frames.last() {
        out.push_str("file ");
        out.push_str(&concat_file_name(&last.file));
        out.push('\n');
    }
    out
}

fn concat_file_name(file: &str) -> String {
    Path::new(file)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(file)
        .to_string()
}

fn extract_zip_safe(zip_path: &Path, dest: &Path) -> Result<(), MediaError> {
    std::fs::create_dir_all(dest).map_err(|e| MediaError::Io(e.to_string()))?;
    let file = File::open(zip_path).map_err(|e| MediaError::Io(e.to_string()))?;
    let mut archive = ZipArchive::new(file).map_err(|e| MediaError::Io(e.to_string()))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| MediaError::Io(e.to_string()))?;
        if entry.is_dir() {
            continue;
        }
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        if rel.components().count() != 1 {
            continue;
        }
        let out_path = dest.join(rel);
        let mut outfile = File::create(&out_path).map_err(|e| MediaError::Io(e.to_string()))?;
        copy(&mut entry, &mut outfile).map_err(|e| MediaError::Io(e.to_string()))?;
        outfile.flush().map_err(|e| MediaError::Io(e.to_string()))?;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ProbeJson {
    format: Option<ProbeFormat>,
    streams: Option<Vec<ProbeStream>>,
}

#[derive(Debug, Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
    size: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    duration: Option<String>,
}

#[async_trait::async_trait]
impl MediaProcessor for FfmpegCli {
    async fn probe(&self, path: &Path) -> Result<MediaInfo, MediaError> {
        let path_str = path.to_string_lossy();
        let json = Self::run(
            &self.ffprobe,
            &[
                "-v",
                "quiet",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
                path_str.as_ref(),
            ],
        )
        .await?;
        let parsed: ProbeJson =
            serde_json::from_str(&json).map_err(|e| MediaError::Ffmpeg(e.to_string()))?;
        let mut info = MediaInfo {
            duration: parsed
                .format
                .as_ref()
                .and_then(|f| f.duration.as_ref())
                .and_then(|d| d.parse::<f64>().ok())
                .map(Duration::from_secs_f64),
            width: None,
            height: None,
            video_codec: None,
            audio_codec: None,
            size_bytes: parsed
                .format
                .as_ref()
                .and_then(|f| f.size.as_ref())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        };
        for stream in parsed.streams.unwrap_or_default() {
            match stream.codec_type.as_deref() {
                Some("video") => {
                    info.width = stream.width;
                    info.height = stream.height;
                    info.video_codec = stream.codec_name;
                    if info.duration.is_none() {
                        info.duration = stream
                            .duration
                            .as_ref()
                            .and_then(|d| d.parse::<f64>().ok())
                            .map(Duration::from_secs_f64);
                    }
                }
                Some("audio") => info.audio_codec = stream.codec_name,
                _ => {}
            }
        }
        Ok(info)
    }

    async fn remux(&self, inputs: &[PathBuf], output: &Path) -> Result<(), MediaError> {
        if inputs.is_empty() {
            return Err(MediaError::Ffmpeg("remux 没有输入文件".into()));
        }
        let out = output.to_string_lossy();
        let mut args: Vec<String> = Vec::new();
        for input in inputs {
            args.push("-i".into());
            args.push(input.to_string_lossy().into_owned());
        }
        args.extend([
            "-c".into(),
            "copy".into(),
            "-movflags".into(),
            "+faststart".into(),
            "-y".into(),
            out.into_owned(),
        ]);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        Self::run(&self.ffmpeg, &arg_refs).await?;
        Ok(())
    }

    async fn thumbnail(&self, input: &Path, output: &Path) -> Result<(), MediaError> {
        let inp = input.to_string_lossy();
        let out = output.to_string_lossy();
        Self::run(
            &self.ffmpeg,
            &[
                "-ss",
                "0",
                "-i",
                inp.as_ref(),
                "-vframes",
                "1",
                "-vf",
                "scale='min(320,iw)':-2",
                "-q:v",
                "3",
                "-y",
                out.as_ref(),
            ],
        )
        .await?;
        Ok(())
    }

    async fn transcode(
        &self,
        input: &Path,
        output: &Path,
        spec: &TranscodeSpec,
    ) -> Result<(), MediaError> {
        let inp = input.to_string_lossy();
        let out = output.to_string_lossy();
        let vf = format!("scale=-2:'min({},ih)'", spec.max_height);
        Self::run(
            &self.ffmpeg,
            &[
                "-i",
                inp.as_ref(),
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-b:v",
                spec.video_bitrate,
                "-c:a",
                "aac",
                "-b:a",
                spec.audio_bitrate,
                "-vf",
                &vf,
                "-movflags",
                "+faststart",
                "-y",
                out.as_ref(),
            ],
        )
        .await?;
        Ok(())
    }

    async fn ugoira_to_mp4(
        &self,
        zip: &Path,
        frames: &[UgoiraFrame],
        work_dir: &Path,
        output: &Path,
    ) -> Result<(), MediaError> {
        if frames.is_empty() {
            return Err(MediaError::Ffmpeg("ugoira 没有帧".into()));
        }
        let zip = zip.to_path_buf();
        let work = work_dir.to_path_buf();
        tokio::task::spawn_blocking(move || extract_zip_safe(&zip, &work))
            .await
            .map_err(|e| MediaError::Io(e.to_string()))??;

        let list_path = work_dir.join("list.ffconcat");
        tokio::fs::write(&list_path, build_concat_list(frames))
            .await
            .map_err(|e| MediaError::Io(e.to_string()))?;

        let list = list_path.to_string_lossy().into_owned();
        let out = output.to_string_lossy().into_owned();
        match encode_ugoira(&self.ffmpeg, &list, &out, true).await {
            Err(MediaError::Ffmpeg(msg))
                if msg.to_ascii_lowercase().contains("unrecognized option") =>
            {
                encode_ugoira(&self.ffmpeg, &list, &out, false).await
            }
            other => other,
        }
    }
}

async fn encode_ugoira(
    bin: &Path,
    list: &str,
    output: &str,
    fps_mode: bool,
) -> Result<(), MediaError> {
    let timing = if fps_mode { "-fps_mode" } else { "-vsync" };
    FfmpegCli::run(
        bin,
        &[
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
            list,
            timing,
            "vfr",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            "scale=trunc(iw/2)*2:trunc(ih/2)*2",
            "-an",
            "-movflags",
            "+faststart",
            "-y",
            output,
        ],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_list_repeats_last_frame_and_converts_ms() {
        let frames = [
            UgoiraFrame {
                file: "000000.jpg".into(),
                delay_ms: 70,
            },
            UgoiraFrame {
                file: "subdir/000001.jpg".into(),
                delay_ms: 100,
            },
        ];
        let list = build_concat_list(&frames);
        assert_eq!(
            list,
            "ffconcat version 1.0\n\
             file 000000.jpg\n\
             duration 0.070\n\
             file 000001.jpg\n\
             duration 0.100\n\
             file 000001.jpg\n"
        );
    }

    #[test]
    fn concat_list_single_frame() {
        let frames = [UgoiraFrame {
            file: "000000.jpg".into(),
            delay_ms: 50,
        }];
        let list = build_concat_list(&frames);
        assert_eq!(
            list,
            "ffconcat version 1.0\n\
             file 000000.jpg\n\
             duration 0.050\n\
             file 000000.jpg\n"
        );
    }
}
