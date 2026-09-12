use serde::Deserialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub telegram: TelegramConfig,
    pub limits: LimitsConfig,
    #[serde(default)]
    pub output: OutputConfig,
    pub database: DatabaseConfig,
    pub ffmpeg: FfmpegConfig,
    #[serde(default)]
    pub ytdlp: YtdlpConfig,
    pub http: HttpConfig,
    pub feed: FeedConfig,
    pub log: LogConfig,
    #[serde(default)]
    pub bilibili: BilibiliConfig,
    #[serde(default)]
    pub netease: NeteaseConfig,
    pub secrets: SecretsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub token: String,
    pub api_url: String,
    #[serde(default)]
    pub admin_ids: Vec<i64>,
    pub mode: String,
    pub polling: PollingConfig,
    pub webhook: WebhookConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PollingConfig {
    pub timeout_secs: u64,
    pub drop_pending: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebhookConfig {
    pub addr: String,
    pub public_url: String,
    pub secret_token: String,
    pub drop_pending: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitsConfig {
    pub extract_permits: u32,
    pub media_permits: u32,
    pub per_user_per_minute: u32,
    pub progress_edit_interval_secs: u64,
    pub max_attempts: u32,
}

fn default_album_max() -> u32 {
    10
}
fn default_summary_threshold() -> u32 {
    20
}
fn default_max_upload_bytes() -> u64 {
    52_428_800
}
fn default_max_photo_bytes() -> u64 {
    10_485_760
}
fn default_overflow() -> String {
    "chunk".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputConfig {
    #[serde(default = "default_album_max")]
    pub album_max: u32,
    #[serde(default = "default_summary_threshold")]
    pub summary_threshold: u32,
    #[serde(default = "default_max_upload_bytes")]
    pub max_upload_bytes: u64,
    #[serde(default = "default_max_photo_bytes")]
    pub max_photo_bytes: u64,
    #[serde(default = "default_overflow")]
    pub overflow: String,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            album_max: default_album_max(),
            summary_threshold: default_summary_threshold(),
            max_upload_bytes: default_max_upload_bytes(),
            max_photo_bytes: default_max_photo_bytes(),
            overflow: default_overflow(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FfmpegConfig {
    pub ffmpeg: String,
    pub ffprobe: String,
}

fn default_ytdlp_bin() -> String {
    "yt-dlp".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct YtdlpConfig {
    #[serde(default = "default_ytdlp_bin")]
    pub bin: String,
}

impl Default for YtdlpConfig {
    fn default() -> Self {
        Self {
            bin: default_ytdlp_bin(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpConfig {
    pub user_agent: String,
    pub timeout_secs: u64,
    pub connect_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedConfig {
    pub tick_secs: u64,
    pub max_fail_count: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogConfig {
    pub format: String,
}

fn default_bili_passport() -> String {
    "https://passport.bilibili.com".into()
}
fn default_bili_api() -> String {
    "https://api.bilibili.com".into()
}
fn default_bili_live() -> String {
    "https://api.live.bilibili.com".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct BilibiliConfig {
    #[serde(default = "default_bili_passport")]
    pub passport_base: String,
    #[serde(default = "default_bili_api")]
    pub api_base: String,
    #[serde(default = "default_bili_live")]
    pub live_api_base: String,
}

impl Default for BilibiliConfig {
    fn default() -> Self {
        Self {
            passport_base: default_bili_passport(),
            api_base: default_bili_api(),
            live_api_base: default_bili_live(),
        }
    }
}

fn default_ncm_api() -> String {
    "http://127.0.0.1:3000".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct NeteaseConfig {
    #[serde(default = "default_ncm_api")]
    pub api_base: String,
}

impl Default for NeteaseConfig {
    fn default() -> Self {
        Self {
            api_base: default_ncm_api(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SecretsConfig {
    #[serde(default)]
    pub master_key: String,
}

impl AppConfig {
    pub fn load() -> Result<Self, config::ConfigError> {
        let cfg = config::Config::builder()
            .add_source(config::File::with_name("config/default").required(true))
            .add_source(
                config::Environment::with_prefix("FEEDBOT")
                    .separator("__")
                    .try_parsing(true)
                    .list_separator(",")
                    .with_list_parse_key("telegram.admin_ids"),
            )
            .build()?;
        cfg.try_deserialize()
    }

    pub fn token(&self) -> anyhow::Result<&str> {
        if self.telegram.token.is_empty() {
            anyhow::bail!("缺少 FEEDBOT__TELEGRAM__TOKEN");
        }
        Ok(&self.telegram.token)
    }

    /// Prefer `FEEDBOT__SECRETS__MASTER_KEY` / config; otherwise load or create
    /// `{database_dir}/master.key` so credentials stay decryptable across restarts.
    pub fn resolve_master_key(&self) -> anyhow::Result<String> {
        load_or_create_master_key(&self.secrets.master_key, &self.master_key_path())
    }

    fn master_key_path(&self) -> PathBuf {
        match self.database.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.join("master.key"),
            _ => PathBuf::from("master.key"),
        }
    }

    pub fn api_url(&self) -> anyhow::Result<Url> {
        Ok(self.telegram.api_url.parse()?)
    }

    pub fn webhook_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.telegram.webhook.addr.parse()?)
    }

    pub fn http_timeouts(&self) -> (Duration, Duration) {
        (
            Duration::from_secs(self.http.timeout_secs),
            Duration::from_secs(self.http.connect_timeout_secs),
        )
    }
}

fn load_or_create_master_key(configured: &str, path: &Path) -> anyhow::Result<String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        validate_master_key(configured)?;
        return Ok(configured.to_string());
    }
    if path.exists() {
        let key = fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}", path.display()))?;
        let key = key.trim().to_string();
        validate_master_key(&key).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        tracing::info!(path = %path.display(), "loaded secrets master key from file");
        return Ok(key);
    }
    let key = feedbot_store::SecretBox::generate_hex_key();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("创建 {} 失败: {e}", parent.display()))?;
        }
    }
    write_master_key_file(path, &key)?;
    tracing::warn!(
        path = %path.display(),
        "未配置 FEEDBOT__SECRETS__MASTER_KEY，已生成并写入本地文件；请备份此文件，丢失将无法解密已保存的登录凭据"
    );
    Ok(key)
}

fn validate_master_key(key: &str) -> anyhow::Result<()> {
    feedbot_store::SecretBox::from_hex_key(key)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn write_master_key_file(path: &Path, key: &str) -> anyhow::Result<()> {
    let tmp = path.with_extension("key.tmp");
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|e| anyhow::anyhow!("写入 {} 失败: {e}", tmp.display()))?;
        file.write_all(key.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|e| anyhow::anyhow!("写入 {} 失败: {e}", tmp.display()))?;
        file.sync_all()
            .map_err(|e| anyhow::anyhow!("写入 {} 失败: {e}", tmp.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow::anyhow!("设置 {} 权限失败: {e}", tmp.display()))?;
    }
    fs::rename(&tmp, path).map_err(|e| anyhow::anyhow!("保存 {} 失败: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_configured_key_without_writing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master.key");
        let configured = "a".repeat(64);
        let got = load_or_create_master_key(&configured, &path).unwrap();
        assert_eq!(got, configured);
        assert!(!path.exists());
    }

    #[test]
    fn creates_and_reloads_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master.key");
        let first = load_or_create_master_key("", &path).unwrap();
        assert_eq!(first.len(), 64);
        assert!(path.exists());
        let second = load_or_create_master_key("", &path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn rejects_invalid_configured_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master.key");
        assert!(load_or_create_master_key("short", &path).is_err());
        assert!(!path.exists());
    }
}
