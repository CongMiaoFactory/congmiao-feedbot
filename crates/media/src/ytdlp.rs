use feedbot_core::{MediaError, Ytdlp, YtdlpSpec};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use url::Url;

const GOOGLE_COOKIE_NAMES: &[&str] = &[
    "SID",
    "HSID",
    "SSID",
    "APISID",
    "SAPISID",
    "__Secure-1PSID",
    "__Secure-3PSID",
    "__Secure-1PAPISID",
    "__Secure-3PAPISID",
    "NID",
];

#[derive(Clone)]
pub struct YtdlpCli {
    bin: PathBuf,
}

impl YtdlpCli {
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        Self { bin: bin.into() }
    }

    fn map_spawn(err: std::io::Error) -> MediaError {
        if err.kind() == std::io::ErrorKind::NotFound {
            MediaError::MissingYtdlp
        } else {
            MediaError::Ytdlp(err.to_string())
        }
    }

    async fn run(
        &self,
        args: &[String],
        timeout: Option<std::time::Duration>,
    ) -> Result<String, MediaError> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = match timeout {
            Some(limit) => tokio::time::timeout(limit, cmd.output())
                .await
                .map_err(|_| MediaError::Ytdlp("yt-dlp 超时".into()))?
                .map_err(Self::map_spawn)?,
            None => cmd.output().await.map_err(Self::map_spawn)?,
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let msg = if stderr.is_empty() { stdout } else { stderr };
            return Err(MediaError::Ytdlp(msg));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

pub fn looks_like_netscape(raw: &str) -> bool {
    let trimmed = raw.trim_start();
    trimmed.starts_with("# Netscape")
        || trimmed.starts_with("# HTTP Cookie File")
        || trimmed.lines().any(|line| {
            let line = line.trim();
            !line.is_empty() && !line.starts_with('#') && line.matches('\t').count() >= 6
        })
}

pub fn cookies_to_netscape(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if looks_like_netscape(trimmed) {
        if trimmed.starts_with('#') {
            return trimmed.to_string();
        }
        return format!("# Netscape HTTP Cookie File\n{trimmed}");
    }
    let mut body = String::from("# Netscape HTTP Cookie File\n");
    for part in trimmed.split([';', '\n', '\r']) {
        let part = part.trim().trim_start_matches("Cookie:").trim();
        if part.is_empty() {
            continue;
        }
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            continue;
        }
        let domain = if GOOGLE_COOKIE_NAMES.contains(&name) {
            ".google.com"
        } else {
            ".youtube.com"
        };
        let secure = name.starts_with("__Secure-") || name == "LOGIN_INFO";
        let flag = if secure { "TRUE" } else { "FALSE" };
        body.push_str(&format!("{domain}\tTRUE\t/\t{flag}\t0\t{name}\t{value}\n"));
    }
    body
}

fn write_cookies_file(raw: &str) -> Result<tempfile::TempPath, MediaError> {
    let mut file = tempfile::NamedTempFile::new().map_err(|e| MediaError::Io(e.to_string()))?;
    file.write_all(cookies_to_netscape(raw).as_bytes())
        .map_err(|e| MediaError::Io(e.to_string()))?;
    file.flush().map_err(|e| MediaError::Io(e.to_string()))?;
    Ok(file.into_temp_path())
}

fn json_from_stdout(stdout: &str) -> Result<Value, MediaError> {
    let trimmed = stdout.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Ok(v);
    }
    let Some(start) = trimmed.find('{') else {
        return Err(MediaError::Ytdlp("yt-dlp 未返回 JSON".into()));
    };
    serde_json::from_str(&trimmed[start..])
        .map_err(|e| MediaError::Ytdlp(format!("解析 yt-dlp JSON 失败: {e}")))
}

#[async_trait::async_trait]
impl Ytdlp for YtdlpCli {
    async fn probe_json(&self, url: &Url, cookies: Option<&str>) -> Result<Value, MediaError> {
        let cookie_file = cookies
            .filter(|s| !s.trim().is_empty())
            .map(write_cookies_file)
            .transpose()?;
        let mut args: Vec<String> = [
            "--ignore-config",
            "--no-warnings",
            "--no-playlist",
            "--skip-download",
            "--retries",
            "1",
            "--extractor-retries",
            "1",
            "--socket-timeout",
            "15",
            "-J",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        if let Some(path) = cookie_file.as_ref() {
            args.push("--cookies".into());
            args.push(
                path.to_str()
                    .ok_or_else(|| MediaError::Io("cookies 路径无效".into()))?
                    .to_string(),
            );
        }
        args.push(url.as_str().to_string());
        let stdout = self
            .run(&args, Some(std::time::Duration::from_secs(45)))
            .await?;
        drop(cookie_file);
        json_from_stdout(&stdout)
    }

    async fn download(&self, url: &Url, dest: &Path, spec: &YtdlpSpec) -> Result<(), MediaError> {
        let cookie_file = spec
            .cookies
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(write_cookies_file)
            .transpose()?;
        let parent = dest.parent().unwrap_or(Path::new("."));
        let stem = dest.file_stem().and_then(|s| s.to_str()).unwrap_or("media");
        let tmpl = parent.join(format!("{stem}.%(ext)s"));
        let tmpl_s = tmpl
            .to_str()
            .ok_or_else(|| MediaError::Io("下载路径无效".into()))?
            .to_string();
        let height = spec.max_height.unwrap_or(1080);
        let format = if spec.audio_only {
            "bestaudio/best".to_string()
        } else {
            format!("bv*[height<={height}]+ba/b[height<={height}]/b")
        };
        let mut args: Vec<String> = vec![
            "--ignore-config".into(),
            "--no-warnings".into(),
            "--no-playlist".into(),
            "--newline".into(),
            "-f".into(),
            format,
            "-o".into(),
            tmpl_s,
        ];
        if spec.audio_only {
            args.extend(["-x".into(), "--audio-format".into(), "m4a".into()]);
        } else {
            args.extend(["--merge-output-format".into(), "mp4".into()]);
        }
        if let Some(path) = cookie_file.as_ref() {
            args.push("--cookies".into());
            args.push(
                path.to_str()
                    .ok_or_else(|| MediaError::Io("cookies 路径无效".into()))?
                    .to_string(),
            );
        }
        args.push(url.as_str().to_string());
        self.run(&args, None).await?;
        drop(cookie_file);
        if dest.exists() {
            return Ok(());
        }
        let found = std::fs::read_dir(parent)
            .map_err(|e| MediaError::Io(e.to_string()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s == stem)
            });
        let Some(found) = found else {
            return Err(MediaError::Ytdlp("yt-dlp 未写出文件".into()));
        };
        if found != dest {
            std::fs::rename(&found, dest).map_err(|e| MediaError::Io(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_header_to_netscape() {
        let n = cookies_to_netscape("SID=abc; LOGIN_INFO=xyz; PREF=f1");
        assert!(n.contains("# Netscape"));
        assert!(n.contains(".google.com"));
        assert!(n.contains("SID\tabc"));
        assert!(n.contains(".youtube.com"));
        assert!(n.contains("LOGIN_INFO\txyz"));
    }

    #[test]
    fn netscape_passthrough() {
        let raw = "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSID\tabc\n";
        assert_eq!(cookies_to_netscape(raw), raw.trim());
    }

    #[test]
    fn cookie_prefix_and_empty() {
        let n = cookies_to_netscape("Cookie: SID=abc; LOGIN_INFO=xyz");
        assert!(n.contains("SID\tabc"));
        assert!(n.contains("LOGIN_INFO\txyz"));
        assert!(cookies_to_netscape("   ").is_empty());
        assert!(looks_like_netscape(
            "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tSID\tabc\n"
        ));
    }
}
