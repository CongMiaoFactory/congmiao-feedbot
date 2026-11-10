//! YouTube / YouTube Music extractor via yt-dlp.

use async_trait::async_trait;
use feedbot_core::MediaError;
use feedbot_core::auth::{
    AuthChallenge, AuthError, AuthInput, AuthMethod, AuthProgress, AuthProvider, AuthSession,
    Credential, CredentialStatus,
};
use feedbot_core::extract::{
    Engagement, ExtractCtx, ExtractError, ExtractRequest, Extracted, Extractor, FeedEntry,
    MediaItem, MediaKind, MediaSource,
};
use feedbot_core::media::Ytdlp;
use feedbot_core::options::{OptionDef, OptionDefault, OptionKind, OptionSpec, OptionValue};
use feedbot_core::{Present, SiteMeta};
use http::HeaderMap;
use serde_json::Value;
use std::sync::Arc;
use url::Url;

use feedbot_site_common::{
    credential_text, json_count, parse_url, rewrite_host, strip_cookie_header,
};

const VERIFY_URL: &str = "https://www.youtube.com/watch?v=jNQXAC9IVRw";
/// Newest uploads considered per subscription poll.
const FEED_LIMIT: u32 = 20;

const SPEC: OptionSpec = OptionSpec {
    options: &[
        OptionDef {
            name: "q",
            aliases: &["quality"],
            kind: OptionKind::Enum {
                variants: &["1080", "720", "480"],
            },
            default: Some(OptionDefault::Enum("1080")),
            affects_output: true,
            desc: "视频最高高度。YouTube Music 始终取 bestaudio",
        },
        OptionDef {
            name: "sp",
            aliases: &["spoiler"],
            kind: OptionKind::Flag,
            default: None,
            affects_output: false,
            desc: "Telegram 遮罩。默认关，+sp 打开，-sp 关闭",
        },
    ],
};

pub struct YoutubeExtractor {
    auth: YoutubeAuth,
}

impl YoutubeExtractor {
    pub fn new(ytdlp: Arc<dyn Ytdlp>) -> Self {
        Self {
            auth: YoutubeAuth { ytdlp },
        }
    }
}

const META: SiteMeta = SiteMeta {
    id: "youtube",
    name: "YouTube",
    aliases: &["yt", "ytb", "ytbm"],
    priority: 60,
    kinds: &["视频", "音频", "直播封面"],
    examples: &[
        "/watch?v={id}",
        "youtu.be/{id}",
        "music.youtube.com/watch?v={id}",
        "/live/{id}",
    ],
    feed_examples: &[
        "youtube.com/@{handle}",
        "/channel/{id}",
        "/playlist?list={id}",
    ],
};

#[async_trait]
impl Extractor for YoutubeExtractor {
    fn meta(&self) -> SiteMeta {
        META
    }
    fn matches(&self, url: &Url) -> bool {
        is_youtube_host(url) && parse_video_id(url).is_some()
    }
    fn rewrite_url(&self, url: &Url) -> Option<Url> {
        rewrite(url)
    }
    fn present(&self) -> Present {
        Present::Clip { origin: "原片" }
    }
    fn options(&self) -> &'static OptionSpec {
        &SPEC
    }
    fn auth(&self) -> Option<&dyn AuthProvider> {
        Some(&self.auth)
    }
    fn feed_matches(&self, url: &Url) -> bool {
        is_youtube_host(url) && feed_url(url).is_some()
    }

    async fn feed(
        &self,
        ctx: &ExtractCtx,
        req: &ExtractRequest,
    ) -> Result<Vec<FeedEntry>, ExtractError> {
        let url = feed_url(&req.url).ok_or(ExtractError::Unsupported)?;
        let cookies = cookie_from_req(req);
        let json = ctx
            .ytdlp
            .probe_playlist(&url, cookies.as_deref(), FEED_LIMIT)
            .await
            .map_err(map_ytdlp)?;
        feed_entries(&json)
    }

    async fn extract(
        &self,
        ctx: &ExtractCtx,
        req: &ExtractRequest,
    ) -> Result<Extracted, ExtractError> {
        let id = parse_video_id(&req.url).ok_or(ExtractError::Unsupported)?;
        let cookies = cookie_from_req(req);
        let json = ctx
            .ytdlp
            .probe_json(&req.url, cookies.as_deref())
            .await
            .map_err(map_ytdlp)?;
        extracted_from_probe(&req.url, &id, cookies, json, &req.options)
    }
}

fn extracted_from_probe(
    page_url: &Url,
    id: &str,
    cookies: Option<String>,
    json: Value,
    options: &feedbot_core::options::OptionValues,
) -> Result<Extracted, ExtractError> {
    let live_status = json
        .get("live_status")
        .and_then(Value::as_str)
        .unwrap_or("");
    if live_status == "is_upcoming" {
        return Err(ExtractError::Failed("尚未开播".into()));
    }
    let is_live = json
        .get("is_live")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || live_status == "is_live";
    let music = is_music_host(page_url);
    let title = json
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let author = json
        .get("uploader")
        .or_else(|| json.get("channel"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let author_url = json
        .get("uploader_url")
        .or_else(|| json.get("channel_url"))
        .and_then(Value::as_str)
        .and_then(|s| Url::parse(s).ok());
    let description = json
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let nsfw = json
        .get("age_limit")
        .and_then(Value::as_u64)
        .is_some_and(|n| n >= 18);
    let thumb = thumbnail_url(&json, id);
    let engagement = Engagement::packed(
        json_count(json.get("view_count")),
        json_count(json.get("like_count")),
        None,
        json_count(json.get("comment_count")),
    );
    let canonical_url = if music {
        Url::parse(&format!("https://music.youtube.com/watch?v={id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?
    } else {
        Url::parse(&format!("https://youtube.com/watch?v={id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?
    };
    let max_height = match options.get("q") {
        Some(OptionValue::Enum(q)) => q.parse().ok(),
        _ => Some(1080),
    };
    let item = if is_live {
        let url = thumb.ok_or_else(|| ExtractError::Failed("直播没有封面".into()))?;
        MediaItem {
            id: id.to_string(),
            kind: MediaKind::Photo,
            source: MediaSource::Direct {
                url,
                headers: HeaderMap::new(),
            },
            filename: Some("live.jpg".into()),
            cover: None,
        }
    } else if music {
        MediaItem {
            id: id.to_string(),
            kind: MediaKind::Audio,
            source: MediaSource::YtDlp {
                url: page_url.clone(),
                audio_only: true,
                max_height: None,
                thumbnail: thumb,
                cookies,
            },
            filename: Some("audio.m4a".into()),
            cover: None,
        }
    } else {
        MediaItem {
            id: id.to_string(),
            kind: MediaKind::Video,
            source: MediaSource::YtDlp {
                url: page_url.clone(),
                audio_only: false,
                max_height,
                thumbnail: thumb,
                cookies,
            },
            filename: Some("video.mp4".into()),
            cover: None,
        }
    };
    Ok(Extracted {
        extractor_id: "youtube".into(),
        canonical_url,
        title,
        author,
        author_url,
        description,
        tags: Vec::new(),
        nsfw,
        ai: false,
        engagement,
        items: vec![item],
    })
}

fn thumbnail_url(json: &Value, id: &str) -> Option<Url> {
    json.get("thumbnail")
        .and_then(Value::as_str)
        .and_then(|s| Url::parse(s).ok())
        .or_else(|| {
            json.pointer("/thumbnails/0/url")
                .and_then(Value::as_str)
                .and_then(|s| Url::parse(s).ok())
        })
        .or_else(|| Url::parse(&format!("https://i.ytimg.com/vi/{id}/maxresdefault.jpg")).ok())
}

/// `youtu.be/{id}` → `youtube.com/watch?v={id}`, plus host aliases.
fn rewrite(url: &Url) -> Option<Url> {
    let host = url.host_str()?.to_ascii_lowercase();
    if host == "youtu.be" || host == "www.youtu.be" {
        let id = url.path_segments()?.next().unwrap_or("").to_string();
        let mut next = url.clone();
        next.set_host(Some("youtube.com")).ok()?;
        next.set_path("/watch");
        if id.is_empty() {
            next.set_query(None);
        } else {
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            query.append_pair("v", &id);
            query.extend_pairs(url.query_pairs().filter(|(k, _)| k != "v"));
            next.set_query(Some(&query.finish()));
        }
        return Some(next);
    }
    rewrite_host(url, &["www.youtube.com", "m.youtube.com"], "youtube.com")
        .or_else(|| rewrite_host(url, &["www.music.youtube.com"], "music.youtube.com"))
}

fn is_youtube_host(url: &Url) -> bool {
    matches!(
        url.host_str().unwrap_or(""),
        "youtube.com"
            | "www.youtube.com"
            | "m.youtube.com"
            | "youtu.be"
            | "www.youtu.be"
            | "music.youtube.com"
            | "www.music.youtube.com"
    )
}

fn is_music_host(url: &Url) -> bool {
    matches!(
        url.host_str().unwrap_or(""),
        "music.youtube.com" | "www.music.youtube.com"
    )
}

fn parse_video_id(url: &Url) -> Option<String> {
    let host = url.host_str().unwrap_or("");
    let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segs
        .first()
        .is_some_and(|s| matches!(*s, "shorts" | "playlist" | "channel"))
    {
        return None;
    }
    if segs.first().is_some_and(|s| s.starts_with('@')) && segs.get(1) != Some(&"live") {
        return None;
    }
    if host == "youtu.be" || host == "www.youtu.be" {
        return segs
            .first()
            .copied()
            .filter(|s| is_video_id(s))
            .map(str::to_string);
    }
    if let Some((_, v)) = url.query_pairs().find(|(k, _)| k == "v") {
        let id = v.into_owned();
        if is_video_id(&id) && segs.first() != Some(&"shorts") {
            return Some(id);
        }
    }
    for pair in segs.windows(2) {
        if matches!(pair[0], "embed" | "live" | "v") && is_video_id(pair[1]) {
            return Some(pair[1].to_string());
        }
    }
    None
}

fn is_video_id(s: &str) -> bool {
    let s = s.split(['?', '&', '/']).next().unwrap_or(s);
    s.len() == 11
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn cookie_from_req(req: &ExtractRequest) -> Option<String> {
    Some(normalize_cookie(credential_text(req)?)).filter(|c| !c.is_empty())
}

fn normalize_cookie(raw: &str) -> String {
    strip_cookie_header(raw).to_string()
}

/// Canonical listing URL for a channel or playlist. Bare channel URLs are
/// pointed at their `/videos` tab so yt-dlp lists uploads instead of tabs.
fn feed_url(url: &Url) -> Option<Url> {
    if is_music_host(url) {
        return None;
    }
    let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segs.as_slice() {
        ["playlist"] => {
            let list = url.query_pairs().find(|(k, _)| k == "list")?.1;
            Url::parse(&format!("https://www.youtube.com/playlist?list={list}")).ok()
        }
        [handle, ..] if handle.starts_with('@') && handle.len() > 1 => {
            Url::parse(&format!("https://www.youtube.com/{handle}/videos")).ok()
        }
        ["channel" | "c" | "user", name, ..] => {
            let kind = segs[0];
            Url::parse(&format!("https://www.youtube.com/{kind}/{name}/videos")).ok()
        }
        _ => None,
    }
}

fn feed_entries(json: &Value) -> Result<Vec<FeedEntry>, ExtractError> {
    let entries = json
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| ExtractError::Failed("yt-dlp 未返回播放列表".into()))?;
    entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str))
        .filter(|id| is_video_id(id))
        .map(|id| {
            Ok(FeedEntry {
                key: id.to_string(),
                url: parse_url(&format!("https://www.youtube.com/watch?v={id}"))?,
            })
        })
        .collect()
}

fn has_session_cookie(raw: &str) -> bool {
    const NAMES: &[&str] = &[
        "LOGIN_INFO",
        "SID",
        "SAPISID",
        "__Secure-1PSID",
        "__Secure-3PSID",
    ];
    NAMES.iter().any(|name| cookie_has(raw, name))
}

fn cookie_has(raw: &str, name: &str) -> bool {
    for line in raw.split(['\n', '\r', ';']) {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == name
            && !v.trim().is_empty()
        {
            return true;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() >= 7 && cols[5] == name && !cols[6].is_empty() {
            return true;
        }
    }
    false
}

fn is_bot_wall(msg: &str) -> bool {
    let t = msg.to_ascii_lowercase();
    t.contains("sign in to confirm")
        || t.contains("not a bot")
        || t.contains("use --cookies")
        || t.contains("please log in")
        || t.contains("confirm you're not a bot")
        || t.contains("confirm you’re not a bot")
}

fn map_ytdlp(err: MediaError) -> ExtractError {
    match err {
        MediaError::MissingYtdlp => ExtractError::Failed("未安装 yt-dlp".into()),
        MediaError::Ytdlp(msg) if is_bot_wall(&msg) => ExtractError::AuthRequired("youtube".into()),
        MediaError::Ytdlp(msg) => ExtractError::Failed(truncate_err(&msg)),
        other => ExtractError::Failed(other.to_string()),
    }
}

fn truncate_err(msg: &str) -> String {
    let line = msg.lines().find(|l| !l.trim().is_empty()).unwrap_or(msg);
    let line = line.trim();
    if line.chars().count() <= 180 {
        line.to_string()
    } else {
        line.chars().take(180).collect()
    }
}

struct YoutubeAuth {
    ytdlp: Arc<dyn Ytdlp>,
}

const METHODS: &[AuthMethod] = &[AuthMethod::Cookie];

#[async_trait]
impl AuthProvider for YoutubeAuth {
    fn methods(&self) -> &[AuthMethod] {
        METHODS
    }

    async fn begin(
        &self,
        method: AuthMethod,
        _session: &mut AuthSession,
    ) -> Result<AuthChallenge, AuthError> {
        if method != AuthMethod::Cookie {
            return Err(AuthError::UnsupportedMethod);
        }
        Ok(AuthChallenge::AskText {
            prompt: "请发送 YouTube Cookie。在已登录 youtube.com 的浏览器：F12 → Application → Cookies，或复制请求头 Cookie。也接受 Netscape cookies.txt。\n至少包含 LOGIN_INFO、SID、SAPISID 或 __Secure-1PSID / __Secure-3PSID 之一。含凭据的消息会在处理后删除。".into(),
            secret: true,
        })
    }

    async fn advance(
        &self,
        session: &mut AuthSession,
        input: AuthInput,
    ) -> Result<AuthProgress, AuthError> {
        match input {
            AuthInput::Cancel => Err(AuthError::Failed("已取消".into())),
            AuthInput::Poll => Err(AuthError::Failed("YouTube 登录不需要轮询".into())),
            AuthInput::Text(text) => {
                let cookie = normalize_cookie(&text);
                if !has_session_cookie(&cookie) {
                    return Err(AuthError::Failed(
                        "cookie 缺少 LOGIN_INFO / SID / SAPISID / __Secure-1PSID。请复制 Value，不是名称。".into(),
                    ));
                }
                self.verify_cookie(&cookie).await?;
                session.method = Some(AuthMethod::Cookie);
                session.step = 1;
                Ok(AuthProgress::Completed(Credential {
                    extractor_id: "youtube".into(),
                    method: AuthMethod::Cookie,
                    payload: cookie.into_bytes(),
                    expires_at: None,
                }))
            }
        }
    }

    async fn check(&self, credential: &Credential) -> Result<CredentialStatus, AuthError> {
        let Ok(raw) = std::str::from_utf8(&credential.payload) else {
            return Ok(CredentialStatus::Invalid);
        };
        let cookie = normalize_cookie(raw);
        match self.verify_cookie(&cookie).await {
            Ok(()) => Ok(CredentialStatus::Valid),
            Err(AuthError::Failed(msg))
                if msg.contains("无效") || msg.contains("缺少") || msg.contains("非法") =>
            {
                Ok(CredentialStatus::Invalid)
            }
            Err(e) => Err(e),
        }
    }

    async fn refresh(&self, _credential: &Credential) -> Result<Option<Credential>, AuthError> {
        Ok(None)
    }
}

impl YoutubeAuth {
    async fn verify_cookie(&self, cookie: &str) -> Result<(), AuthError> {
        if !has_session_cookie(cookie) {
            return Err(AuthError::Failed("cookie 缺少会话字段".into()));
        }
        let url = Url::parse(VERIFY_URL).map_err(|e| AuthError::Failed(e.to_string()))?;
        match self.ytdlp.probe_json(&url, Some(cookie)).await {
            Ok(json) => {
                if json.get("id").and_then(Value::as_str).is_some()
                    || json.get("title").and_then(Value::as_str).is_some()
                {
                    Ok(())
                } else {
                    Err(AuthError::Failed("cookie 无效或已过期".into()))
                }
            }
            Err(MediaError::MissingYtdlp) => Err(AuthError::Failed("未安装 yt-dlp".into())),
            Err(MediaError::Ytdlp(msg)) if msg.contains("超时") => Err(AuthError::Failed(
                "校验超时，请重试或发送「取消」退出".into(),
            )),
            Err(MediaError::Ytdlp(msg)) if is_bot_wall(&msg) => {
                Err(AuthError::Failed("cookie 无效或已过期".into()))
            }
            Err(MediaError::Ytdlp(msg)) => Err(AuthError::Failed(truncate_err(&msg))),
            Err(e) => Err(AuthError::Failed(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feedbot_core::options::{OptionValues, merge_layers};

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn parse_watch_embed_live_and_music() {
        assert_eq!(
            parse_video_id(&url("https://www.youtube.com/watch?v=jNQXAC9IVRw")),
            Some("jNQXAC9IVRw".into())
        );
        assert_eq!(
            parse_video_id(&url("https://youtu.be/jNQXAC9IVRw")),
            Some("jNQXAC9IVRw".into())
        );
        assert_eq!(
            parse_video_id(&url("https://youtube.com/embed/jNQXAC9IVRw")),
            Some("jNQXAC9IVRw".into())
        );
        assert_eq!(
            parse_video_id(&url("https://youtube.com/live/jNQXAC9IVRw")),
            Some("jNQXAC9IVRw".into())
        );
        assert_eq!(
            parse_video_id(&url("https://music.youtube.com/watch?v=jNQXAC9IVRw")),
            Some("jNQXAC9IVRw".into())
        );
        assert!(
            YoutubeExtractor::new(Arc::new(feedbot_site_common::NoYtdlp))
                .matches(&url("https://youtube.com/watch?v=jNQXAC9IVRw"))
        );
        assert!(
            !YoutubeExtractor::new(Arc::new(feedbot_site_common::NoYtdlp))
                .matches(&url("https://youtube.com/shorts/jNQXAC9IVRw"))
        );
        assert!(
            !YoutubeExtractor::new(Arc::new(feedbot_site_common::NoYtdlp))
                .matches(&url("https://youtube.com/playlist?list=PLxx"))
        );
        assert!(
            !YoutubeExtractor::new(Arc::new(feedbot_site_common::NoYtdlp))
                .matches(&url("https://youtube.com/@foo"))
        );
    }

    #[test]
    fn live_probe_is_photo_music_is_audio() {
        let opts = merge_layers(
            YoutubeExtractor::new(Arc::new(feedbot_site_common::NoYtdlp)).options(),
            &OptionValues::new(),
            &[],
        )
        .unwrap();
        let live = extracted_from_probe(
            &url("https://youtube.com/watch?v=jNQXAC9IVRw"),
            "jNQXAC9IVRw",
            None,
            serde_json::json!({
                "title": "live now",
                "is_live": true,
                "live_status": "is_live",
                "thumbnail": "https://i.ytimg.com/vi/jNQXAC9IVRw/maxresdefault.jpg"
            }),
            &opts,
        )
        .unwrap();
        assert_eq!(live.items[0].kind, MediaKind::Photo);
        assert!(matches!(live.items[0].source, MediaSource::Direct { .. }));

        let music = extracted_from_probe(
            &url("https://music.youtube.com/watch?v=jNQXAC9IVRw"),
            "jNQXAC9IVRw",
            Some("SID=abc".into()),
            serde_json::json!({
                "title": "song",
                "uploader": "A",
                "view_count": 12,
                "like_count": 3
            }),
            &opts,
        )
        .unwrap();
        assert_eq!(music.items[0].kind, MediaKind::Audio);
        match &music.items[0].source {
            MediaSource::YtDlp {
                audio_only,
                cookies,
                ..
            } => {
                assert!(*audio_only);
                assert_eq!(cookies.as_deref(), Some("SID=abc"));
            }
            other => panic!("expected YtDlp, got {other:?}"),
        }
        assert_eq!(music.engagement.as_ref().unwrap().views, Some(12));
        assert_eq!(music.canonical_url.host_str(), Some("music.youtube.com"));
    }

    #[test]
    fn upcoming_errors_and_missing_counts_are_none() {
        let opts = merge_layers(
            YoutubeExtractor::new(Arc::new(feedbot_site_common::NoYtdlp)).options(),
            &OptionValues::new(),
            &[],
        )
        .unwrap();
        let err = extracted_from_probe(
            &url("https://youtube.com/watch?v=jNQXAC9IVRw"),
            "jNQXAC9IVRw",
            None,
            serde_json::json!({ "live_status": "is_upcoming" }),
            &opts,
        )
        .unwrap_err();
        assert!(matches!(err, ExtractError::Failed(m) if m.contains("尚未开播")));

        let vod = extracted_from_probe(
            &url("https://youtube.com/watch?v=jNQXAC9IVRw"),
            "jNQXAC9IVRw",
            None,
            serde_json::json!({ "title": "vod" }),
            &opts,
        )
        .unwrap();
        assert!(vod.engagement.is_none());
        assert!(matches!(
            vod.items[0].source,
            MediaSource::YtDlp {
                audio_only: false,
                ..
            }
        ));
    }

    #[test]
    fn cookie_session_and_normalize() {
        assert!(has_session_cookie("SID=abc; PREF=1"));
        assert!(has_session_cookie(
            "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tLOGIN_INFO\txyz\n"
        ));
        assert!(!has_session_cookie("PREF=1; VISITOR_INFO1_LIVE=x"));
        assert_eq!(normalize_cookie("Cookie: SID=abc"), "SID=abc");
        assert!(is_bot_wall("Sign in to confirm you’re not a bot"));
        assert!(has_session_cookie("__Secure-1PSID=x"));
        assert!(has_session_cookie("__Secure-3PSID=y"));
    }

    #[test]
    fn age_limit_marks_nsfw_and_aliases_resolve() {
        let opts = merge_layers(
            YoutubeExtractor::new(Arc::new(feedbot_site_common::NoYtdlp)).options(),
            &OptionValues::new(),
            &[],
        )
        .unwrap();
        let nsfw = extracted_from_probe(
            &url("https://youtube.com/watch?v=jNQXAC9IVRw"),
            "jNQXAC9IVRw",
            None,
            serde_json::json!({ "title": "gated", "age_limit": 18 }),
            &opts,
        )
        .unwrap();
        assert!(nsfw.nsfw);
        let sfw = extracted_from_probe(
            &url("https://youtube.com/watch?v=jNQXAC9IVRw"),
            "jNQXAC9IVRw",
            None,
            serde_json::json!({ "title": "ok", "age_limit": 0 }),
            &opts,
        )
        .unwrap();
        assert!(!sfw.nsfw);
    }

    mod flow {
        use super::*;
        use feedbot_site_common::testing::{FakeYtdlp, request};
        use serde_json::json;

        fn options(ext: &YoutubeExtractor, tokens: &[&str]) -> OptionValues {
            let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
            ext.resolve_options(&OptionValues::new(), &tokens).unwrap()
        }

        #[tokio::test]
        async fn video_passes_quality_and_cookies_to_ytdlp() {
            let ytdlp = Arc::new(FakeYtdlp::new(json!({
                "title": " 标题 ", "uploader": "ch", "view_count": 5,
                "thumbnail": "https://i.ytimg.com/vi/jNQXAC9IVRw/maxresdefault.jpg"
            })));
            let ext = YoutubeExtractor::new(ytdlp.clone());
            let req = request(
                "youtube",
                "https://youtu.be/jNQXAC9IVRw",
                options(&ext, &["+q:720"]),
                Some("Cookie: SID=a; LOGIN_INFO=b"),
            );
            let extracted = ext.extract(&ytdlp.ctx(), &req).await.unwrap();
            assert_eq!(extracted.title.as_deref(), Some("标题"));
            assert_eq!(
                extracted.canonical_url.as_str(),
                "https://youtube.com/watch?v=jNQXAC9IVRw"
            );
            let MediaSource::YtDlp {
                max_height,
                cookies,
                audio_only,
                ..
            } = &extracted.items[0].source
            else {
                panic!("{:?}", extracted.items[0].source);
            };
            assert_eq!((*max_height, *audio_only), (Some(720), false));
            assert_eq!(cookies.as_deref(), Some("SID=a; LOGIN_INFO=b"));
            assert_eq!(ytdlp.calls()[0].1.as_deref(), Some("SID=a; LOGIN_INFO=b"));
        }

        #[tokio::test]
        async fn bot_wall_asks_for_login() {
            let ytdlp = Arc::new(FakeYtdlp::failing(
                "ERROR: [youtube] jNQXAC9IVRw: Sign in to confirm you're not a bot",
            ));
            let ext = YoutubeExtractor::new(ytdlp.clone());
            let req = request(
                "youtube",
                "https://www.youtube.com/watch?v=jNQXAC9IVRw",
                options(&ext, &[]),
                None,
            );
            let err = ext.extract(&ytdlp.ctx(), &req).await.unwrap_err();
            assert!(matches!(err, ExtractError::AuthRequired(_)), "{err:?}");

            let ytdlp = Arc::new(FakeYtdlp::failing("ERROR: Video unavailable\nmore"));
            let ext = YoutubeExtractor::new(ytdlp.clone());
            let err = ext.extract(&ytdlp.ctx(), &req).await.unwrap_err();
            assert_eq!(
                err.to_string(),
                ExtractError::Failed("ERROR: Video unavailable".into()).to_string()
            );
        }

        #[tokio::test]
        async fn channel_feed_lists_uploads_tab() {
            let ytdlp = Arc::new(FakeYtdlp::new(json!({ "entries": [
                { "id": "jNQXAC9IVRw" }, { "id": "UCnot-a-video-id" }, { "id": "dQw4w9WgXcQ" }
            ] })));
            let ext = YoutubeExtractor::new(ytdlp.clone());
            let req = request(
                "youtube",
                "https://www.youtube.com/@jawed",
                OptionValues::new(),
                None,
            );
            let entries = ext.feed(&ytdlp.ctx(), &req).await.unwrap();
            let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
            assert_eq!(keys, ["jNQXAC9IVRw", "dQw4w9WgXcQ"]);
            assert_eq!(
                ytdlp.calls()[0].0.as_str(),
                "https://www.youtube.com/@jawed/videos"
            );
        }
    }
}
