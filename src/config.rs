use feedbot_bot::TelegraphSettings;
use serde::Deserialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use url::Url;

/// Settings come from `config/default.toml` (every key except the bot token
/// must be listed there; it is the single place defaults live) overridden by
/// `FEEDBOT__<SECTION>__<KEY>` environment variables.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub telegram: TelegramConfig,
    pub limits: LimitsConfig,
    pub output: OutputConfig,
    pub telegraph: TelegraphConfig,
    pub database: DatabaseConfig,
    pub ffmpeg: FfmpegConfig,
    pub ytdlp: YtdlpConfig,
    pub http: HttpConfig,
    pub feed: FeedConfig,
    pub log: LogConfig,
    pub bilibili: BilibiliConfig,
    pub netease: NeteaseConfig,
    pub x: XConfig,
    pub secrets: SecretsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    /// Secret: only from `FEEDBOT__TELEGRAM__TOKEN`, never in the file.
    #[serde(default)]
    pub token: String,
    pub api_url: String,
    pub admin_ids: Vec<i64>,
    pub mode: TelegramMode,
    pub polling: PollingConfig,
    pub webhook: WebhookConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelegramMode {
    Polling,
    Webhook,
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
    pub max_attempts: u32,
    /// Jobs processed at the same time; further jobs wait in the queue.
    pub max_jobs: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputConfig {
    pub album_max: u32,
    pub summary_threshold: u32,
    pub max_upload_bytes: u64,
    pub max_photo_bytes: u64,
    /// What to do with more than `summary_threshold` items.
    pub overflow: OverflowMode,
    /// Hard cap for one downloaded file (before transcoding).
    pub max_download_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverflowMode {
    /// Send every item in albums, then a summary line.
    Chunk,
    /// Publish hotlinkable photo sets as one Telegraph page; else `Chunk`.
    Telegraph,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegraphConfig {
    pub api_base: String,
    /// Secret. Empty: a new account is created on first use per process.
    pub access_token: String,
    pub author_name: String,
    /// `image-host=proxy-host` entries for images that block hotlinking.
    pub image_proxies: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FfmpegConfig {
    pub ffmpeg: String,
    pub ffprobe: String,
    /// Wall-clock limit for one ffmpeg/ffprobe run.
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct YtdlpConfig {
    pub bin: String,
    pub download_timeout_secs: u64,
    pub js_runtime: String,
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
    /// Poll interval of new subscriptions.
    pub sub_interval_secs: u64,
    /// New entries pushed per poll.
    pub push_limit: usize,
    /// Subscriptions polled at the same time.
    pub poll_concurrency: usize,
    pub poll_timeout_secs: u64,
    /// Minimum time between two checks of one stored credential.
    pub credential_check_secs: u64,
    /// Finished jobs older than this are deleted.
    pub job_retention_days: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogConfig {
    pub format: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BilibiliConfig {
    pub passport_base: String,
    pub api_base: String,
    pub live_api_base: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NeteaseConfig {
    pub api_base: String,
}

/// GraphQL query ids of the x.com web client.
#[derive(Debug, Clone, Deserialize)]
pub struct XConfig {
    pub tweet_result_by_rest_id: String,
    pub user_by_screen_name: String,
    pub user_media: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SecretsConfig {
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
                    .with_list_parse_key("telegram.admin_ids")
                    .with_list_parse_key("telegraph.image_proxies"),
            )
            .build()?;
        let cfg: Self = cfg.try_deserialize()?;
        cfg.validate().map_err(config::ConfigError::Message)?;
        Ok(cfg)
    }

    /// Reject values that would stall jobs or hammer upstream APIs instead of
    /// failing later at run time.
    fn validate(&self) -> Result<(), String> {
        let positive = [
            (
                "limits.extract_permits",
                u64::from(self.limits.extract_permits),
            ),
            ("limits.media_permits", u64::from(self.limits.media_permits)),
            (
                "limits.per_user_per_minute",
                u64::from(self.limits.per_user_per_minute),
            ),
            ("limits.max_attempts", u64::from(self.limits.max_attempts)),
            ("limits.max_jobs", self.limits.max_jobs as u64),
            (
                "output.summary_threshold",
                u64::from(self.output.summary_threshold),
            ),
            ("output.max_upload_bytes", self.output.max_upload_bytes),
            ("output.max_photo_bytes", self.output.max_photo_bytes),
            ("output.max_download_bytes", self.output.max_download_bytes),
            ("ffmpeg.timeout_secs", self.ffmpeg.timeout_secs),
            (
                "ytdlp.download_timeout_secs",
                self.ytdlp.download_timeout_secs,
            ),
            ("http.timeout_secs", self.http.timeout_secs),
            ("http.connect_timeout_secs", self.http.connect_timeout_secs),
            ("feed.tick_secs", self.feed.tick_secs),
            (
                "feed.max_fail_count",
                self.feed.max_fail_count.max(0) as u64,
            ),
            ("feed.sub_interval_secs", self.feed.sub_interval_secs),
            ("feed.push_limit", self.feed.push_limit as u64),
            ("feed.poll_concurrency", self.feed.poll_concurrency as u64),
            ("feed.poll_timeout_secs", self.feed.poll_timeout_secs),
            (
                "feed.credential_check_secs",
                self.feed.credential_check_secs,
            ),
        ];
        if let Some((key, _)) = positive.iter().find(|(_, v)| *v == 0) {
            return Err(format!("配置 {key} 必须大于 0"));
        }
        if !(2..=10).contains(&self.output.album_max) {
            return Err(format!(
                "配置 output.album_max 必须在 2–10 之间（Telegram 相册上限），当前 {}",
                self.output.album_max
            ));
        }
        // Checks only run on ticks, so a shorter period means a real API call
        // per stored credential on every tick.
        if self.feed.credential_check_secs < self.feed.tick_secs {
            return Err(format!(
                "配置 feed.credential_check_secs 必须不小于 feed.tick_secs（{}）",
                self.feed.tick_secs
            ));
        }
        if self.feed.sub_interval_secs < self.feed.tick_secs {
            return Err(format!(
                "配置 feed.sub_interval_secs 必须不小于 feed.tick_secs（{}）",
                self.feed.tick_secs
            ));
        }
        Ok(())
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

    /// `telegraph.image_proxies` entries are `image-host=proxy-host`.
    pub fn telegraph_settings(&self) -> anyhow::Result<TelegraphSettings> {
        let image_proxies = self
            .telegraph
            .image_proxies
            .iter()
            .map(|entry| {
                entry
                    .split_once('=')
                    .map(|(from, to)| (from.trim(), to.trim()))
                    .filter(|(from, to)| !from.is_empty() && !to.is_empty())
                    .map(|(from, to)| (from.to_ascii_lowercase(), to.to_ascii_lowercase()))
                    .ok_or_else(|| {
                        anyhow::anyhow!("telegraph.image_proxies 格式应为 host=proxy: {entry}")
                    })
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(TelegraphSettings {
            api_base: self.telegraph.api_base.clone(),
            access_token: self.telegraph.access_token.clone(),
            author_name: self.telegraph.author_name.clone(),
            image_proxies,
        })
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
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("创建 {} 失败: {e}", parent.display()))?;
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
    fn default_file_lists_every_setting() {
        let cfg = config::Config::builder()
            .add_source(config::File::with_name("config/default").required(true))
            .build()
            .unwrap();
        let cfg: AppConfig = cfg.try_deserialize().unwrap();
        assert!(cfg.telegram.token.is_empty());
        assert_eq!(cfg.telegram.mode, TelegramMode::Polling);
        assert_eq!(cfg.output.overflow, OverflowMode::Chunk);
        assert!(cfg.telegraph.access_token.is_empty());
    }

    fn defaults() -> AppConfig {
        config::Config::builder()
            .add_source(config::File::with_name("config/default").required(true))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap()
    }

    #[test]
    fn validation_rejects_stalling_values() {
        assert_eq!(defaults().validate(), Ok(()));

        let mut cfg = defaults();
        cfg.limits.max_attempts = 0;
        assert!(cfg.validate().unwrap_err().contains("limits.max_attempts"));

        let mut cfg = defaults();
        cfg.output.album_max = 11;
        assert!(cfg.validate().unwrap_err().contains("output.album_max"));

        let mut cfg = defaults();
        cfg.feed.credential_check_secs = cfg.feed.tick_secs - 1;
        assert!(
            cfg.validate()
                .unwrap_err()
                .contains("feed.credential_check_secs")
        );

        let mut cfg = defaults();
        cfg.feed.max_fail_count = -1;
        assert!(cfg.validate().unwrap_err().contains("feed.max_fail_count"));
    }

    #[test]
    fn telegraph_proxies_parse() {
        let mut cfg: AppConfig = config::Config::builder()
            .add_source(config::File::with_name("config/default").required(true))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap();
        cfg.telegraph.image_proxies = vec!["I.pximg.net = i.pixiv.re".into()];
        let settings = cfg.telegraph_settings().unwrap();
        assert_eq!(
            settings
                .image_proxies
                .get("i.pximg.net")
                .map(String::as_str),
            Some("i.pixiv.re")
        );
        for bad in ["nope", "a=", "=b"] {
            cfg.telegraph.image_proxies = vec![bad.into()];
            assert!(cfg.telegraph_settings().is_err(), "{bad}");
        }
    }

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
