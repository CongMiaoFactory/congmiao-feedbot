use std::{env, path::PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaSpoilerMode {
    Auto,
    Always,
    Off,
}

impl MediaSpoilerMode {
    fn from_env() -> Self {
        match env::var("MEDIA_SPOILER_MODE")
            .unwrap_or_else(|_| "auto".into())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "always" | "on" | "true" | "1" => Self::Always,
            "off" | "false" | "0" => Self::Off,
            _ => Self::Auto,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub telegram_token: String,
    pub telegram_api_url: Option<String>,
    pub database_url: String,
    pub redis_url: Option<String>,
    pub fxtwitter_api_base: String,
    pub pixiv_web_api_base: String,
    pub netease_api_base: String,
    pub netease_cookie: Option<String>,
    pub youtube_api_key: Option<String>,
    pub youtube_cookies_file: Option<PathBuf>,
    pub pixiv_refresh_token: Option<String>,
    pub bilibili_cookie: Option<String>,
    pub bilibili_passport_base: String,
    pub bilibili_api_base: String,
    pub bilibili_live_api_base: String,
    pub bilibili_www_base: String,
    pub ffmpeg_path: String,
    pub yt_dlp_path: String,
    pub temp_dir: PathBuf,
    pub upload_workers: usize,
    pub max_queue_size: usize,
    pub request_limit_count: u64,
    pub request_limit_ttl: u64,
    pub local_bot_api: bool,
    pub webhook_url: Option<String>,
    pub webhook_host: String,
    pub webhook_port: u16,
    pub admin_user_id: Option<u64>,
    pub media_spoiler_mode: MediaSpoilerMode,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let telegram_token = env::var("TELEGRAM_BOT_TOKEN")
            .or_else(|_| env::var("TOKEN"))
            .context("必须设置 TELEGRAM_BOT_TOKEN（或兼容变量 TOKEN）")?;
        Ok(Self {
            telegram_token,
            telegram_api_url: value("TELEGRAM_API_URL"),
            database_url: env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://data/feedbot.db?mode=rwc".to_string()),
            redis_url: value("REDIS_URL"),
            fxtwitter_api_base: env::var("FXTWITTER_API_BASE")
                .unwrap_or_else(|_| "https://api.fxtwitter.com".to_string()),
            pixiv_web_api_base: env::var("PIXIV_WEB_API_BASE")
                .or_else(|_| env::var("PHIXIV_API_BASE"))
                .unwrap_or_else(|_| "https://www.pixiv.net".to_string()),
            netease_api_base: env::var("NETEASE_API_BASE")
                .unwrap_or_else(|_| "http://netease-api:3000".to_string()),
            netease_cookie: value("NETEASE_COOKIE"),
            youtube_api_key: value("YOUTUBE_API_KEY"),
            youtube_cookies_file: value("YOUTUBE_COOKIES_FILE").map(PathBuf::from),
            pixiv_refresh_token: value("PIXIV_REFRESH_TOKEN"),
            bilibili_cookie: value("BILIBILI_COOKIE"),
            bilibili_passport_base: env::var("BILIBILI_PASSPORT_BASE")
                .unwrap_or_else(|_| "https://passport.bilibili.com".into()),
            bilibili_api_base: env::var("BILIBILI_API_BASE")
                .unwrap_or_else(|_| "https://api.bilibili.com".into()),
            bilibili_live_api_base: env::var("BILIBILI_LIVE_API_BASE")
                .unwrap_or_else(|_| "https://api.live.bilibili.com".into()),
            bilibili_www_base: env::var("BILIBILI_WWW_BASE")
                .unwrap_or_else(|_| "https://www.bilibili.com".into()),
            ffmpeg_path: env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".to_string()),
            yt_dlp_path: env::var("YT_DLP_PATH").unwrap_or_else(|_| "yt-dlp".to_string()),
            temp_dir: PathBuf::from(env::var("TEMP_DIR").unwrap_or_else(|_| "tmp".to_string())),
            upload_workers: parse("UPLOAD_WORKERS", 4),
            max_queue_size: parse("MAX_QUEUE_SIZE", 200),
            request_limit_count: parse("REQUEST_LIMIT_COUNT", 0),
            request_limit_ttl: parse("REQUEST_LIMIT_TTL", 86_400),
            local_bot_api: parse_bool("LOCAL_MODE", false),
            webhook_url: value("WEBHOOK_URL"),
            webhook_host: env::var("WEBHOOK_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            webhook_port: parse("WEBHOOK_PORT", 8080),
            admin_user_id: value("ADMIN_USER_ID").and_then(|v| v.parse().ok()),
            media_spoiler_mode: MediaSpoilerMode::from_env(),
        })
    }
}

fn value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn parse<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn parse_bool(name: &str, default: bool) -> bool {
    env::var(name)
        .ok()
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}
