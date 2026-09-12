//! NetEase Cloud Music via ncm-api sidecar.

use crate::qr::render_qr_png;
use async_trait::async_trait;
use feedbot_core::auth::{
    AuthChallenge, AuthError, AuthInput, AuthMethod, AuthProgress, AuthProvider, AuthSession,
    Credential, CredentialStatus,
};
use feedbot_core::extract::{
    Engagement, ExtractCtx, ExtractError, ExtractRequest, Extracted, Extractor, HttpMethod,
    HttpRequest, MediaItem, MediaKind, MediaSource,
};
use feedbot_core::options::{OptionDef, OptionDefault, OptionKind, OptionSpec, OptionValue};
use http::HeaderMap;
use serde_json::Value;
use std::time::{Duration, SystemTime};
use time::OffsetDateTime;
use url::Url;

const METHODS: &[AuthMethod] = &[AuthMethod::QrCode];

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
            desc: "MV 清晰度",
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

#[derive(Debug, Clone)]
pub struct NeteaseEndpoints {
    pub api_base: String,
}

impl Default for NeteaseEndpoints {
    fn default() -> Self {
        Self {
            api_base: "http://127.0.0.1:3000".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NeteaseKind {
    Song,
    Album,
    Playlist,
    Mv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NcmPoll {
    Waiting,
    Scanned,
    Success,
    Expired,
    Other(i64),
}

pub struct NeteaseExtractor {
    auth: NeteaseAuth,
    api_base: String,
}

impl NeteaseExtractor {
    pub fn new(endpoints: NeteaseEndpoints) -> Self {
        let base = trim_slash(&endpoints.api_base);
        Self {
            auth: NeteaseAuth::new(base.clone()),
            api_base: base,
        }
    }
}

impl Default for NeteaseExtractor {
    fn default() -> Self {
        Self::new(NeteaseEndpoints::default())
    }
}

#[async_trait]
impl Extractor for NeteaseExtractor {
    fn id(&self) -> &'static str {
        "netease"
    }
    fn display_name(&self) -> &'static str {
        "网易云音乐"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["ncm", "163", "网易云"]
    }
    fn priority(&self) -> i32 {
        40
    }
    fn matches(&self, url: &Url) -> bool {
        let host = url.host_str().unwrap_or("");
        if matches!(
            host.to_ascii_lowercase().as_str(),
            "163cn.tv" | "y.music.163.com"
        ) {
            return true;
        }
        is_netease_host(host) && parse_kind_and_id(url).is_some()
    }
    fn options(&self) -> &'static OptionSpec {
        &SPEC
    }
    fn auth(&self) -> Option<&dyn AuthProvider> {
        Some(&self.auth)
    }
    fn extract_kinds(&self) -> &'static [&'static str] {
        &["歌曲", "MV", "专辑", "歌单"]
    }
    fn url_examples(&self) -> &'static [&'static str] {
        &[
            "music.163.com/song?id=",
            "music.163.com/mv?id=",
            "music.163.com/album?id=",
            "music.163.com/playlist?id=",
        ]
    }

    async fn extract(
        &self,
        ctx: &ExtractCtx,
        req: &ExtractRequest,
    ) -> Result<Extracted, ExtractError> {
        let url = expand_netease(ctx, &req.url).await?;
        let (kind, id) = parse_kind_and_id(&url).ok_or(ExtractError::Unsupported)?;
        let cookie = cookie_from_req(req);
        match kind {
            NeteaseKind::Song => self.extract_song(ctx, &id, cookie.as_deref()).await,
            NeteaseKind::Mv => {
                self.extract_mv(ctx, &id, cookie.as_deref(), &req.options)
                    .await
            }
            NeteaseKind::Album => self.extract_collection(ctx, &id, true, cookie.as_deref()).await,
            NeteaseKind::Playlist => {
                self.extract_collection(ctx, &id, false, cookie.as_deref())
                    .await
            }
        }
    }
}

impl NeteaseExtractor {
    async fn sidecar_get(
        &self,
        ctx: &ExtractCtx,
        path: &str,
        params: &[(&str, &str)],
        cookie: Option<&str>,
    ) -> Result<Value, ExtractError> {
        let mut url = Url::parse(&format!("{}{path}", self.api_base))
            .map_err(|_| sidecar_err())?;
        {
            let mut q = url.query_pairs_mut();
            for (k, v) in params {
                q.append_pair(k, v);
            }
            if let Some(c) = cookie.filter(|s| !s.is_empty()) {
                q.append_pair("cookie", c);
            }
        }
        let resp = ctx
            .http
            .fetch(HttpRequest {
                url,
                headers: HeaderMap::new(),
                method: HttpMethod::Get,
            })
            .await
            .map_err(map_extract_sidecar)?;
        if resp.status == 0 || resp.status >= 500 {
            return Err(sidecar_err());
        }
        if !(200..300).contains(&resp.status) {
            return Err(ExtractError::Network(format!("HTTP {}", resp.status)));
        }
        serde_json::from_slice(&resp.body).map_err(|e| ExtractError::Failed(e.to_string()))
    }

    async fn extract_song(
        &self,
        ctx: &ExtractCtx,
        id: &str,
        cookie: Option<&str>,
    ) -> Result<Extracted, ExtractError> {
        let detail = self
            .sidecar_get(ctx, "/song/detail", &[("ids", id)], cookie)
            .await?;
        let audio = self
            .sidecar_get(ctx, "/song/url/v1", &[("id", id), ("level", "standard")], cookie)
            .await?;
        require_login(&detail, &audio)?;
        song_from_api(id, &detail, &audio)
    }

    async fn extract_mv(
        &self,
        ctx: &ExtractCtx,
        id: &str,
        cookie: Option<&str>,
        options: &feedbot_core::options::OptionValues,
    ) -> Result<Extracted, ExtractError> {
        let quality = match options.get("q") {
            Some(OptionValue::Enum(q)) => q.as_str(),
            _ => "1080",
        };
        let detail = self
            .sidecar_get(ctx, "/mv/detail", &[("mvid", id)], cookie)
            .await?;
        let urls = self
            .sidecar_get(ctx, "/mv/url", &[("id", id), ("r", quality)], cookie)
            .await?;
        require_login(&detail, &urls)?;
        mv_from_api(id, &detail, &urls)
    }

    async fn extract_collection(
        &self,
        ctx: &ExtractCtx,
        id: &str,
        album: bool,
        cookie: Option<&str>,
    ) -> Result<Extracted, ExtractError> {
        let (path, name_ptr, cover_ptr, tracks_ptr, canonical) = if album {
            (
                "/album",
                "/album/name",
                "/album/picUrl",
                "/songs",
                format!("https://music.163.com/#/album?id={id}"),
            )
        } else {
            (
                "/playlist/detail",
                "/playlist/name",
                "/playlist/coverImgUrl",
                "/playlist/tracks",
                format!("https://music.163.com/#/playlist?id={id}"),
            )
        };
        let root = self
            .sidecar_get(ctx, path, &[("id", id)], cookie)
            .await?;
        require_login(&root, &Value::Null)?;
        collection_from_api(id, &root, name_ptr, cover_ptr, tracks_ptr, &canonical)
    }
}

pub(crate) fn song_from_api(id: &str, detail: &Value, audio: &Value) -> Result<Extracted, ExtractError> {
    let song = detail
        .pointer("/songs/0")
        .ok_or_else(|| ExtractError::Failed("歌曲不存在".into()))?;
    let media_url = json_str(audio, "/data/0/url")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ExtractError::Failed("歌曲无可用播放地址".into()))?;
    let url = Url::parse(media_url).map_err(|e| ExtractError::Failed(e.to_string()))?;
    let artists = artists_of(song);
    let cover = json_str(song, "/al/picUrl").and_then(|s| Url::parse(s).ok());
    let audio_format = json_str(audio, "/data/0/type").unwrap_or("mp3");
    Ok(Extracted {
        extractor_id: "netease".into(),
        canonical_url: Url::parse(&format!("https://music.163.com/#/song?id={id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?,
        title: json_str(song, "/name").map(str::to_string),
        author: (!artists.is_empty()).then_some(artists),
        author_url: None,
        description: json_str(song, "/al/name").map(str::to_string),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: None,
        items: vec![MediaItem {
            id: id.into(),
            kind: MediaKind::Audio,
            source: MediaSource::Direct {
                url,
                headers: HeaderMap::new(),
            },
            filename: Some(song_filename(
                json_str(song, "/name").unwrap_or(""),
                id,
                audio_format,
            )),
            cover,
        }],
    })
}

pub(crate) fn mv_from_api(id: &str, detail: &Value, urls: &Value) -> Result<Extracted, ExtractError> {
    let mv = detail.get("data").unwrap_or(detail);
    let media_url = json_str(urls, "/data/url")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ExtractError::Failed("MV 无可用播放地址".into()))?;
    let url = Url::parse(media_url).map_err(|e| ExtractError::Failed(e.to_string()))?;
    let cover = json_str(mv, "/cover").and_then(|s| Url::parse(s).ok());
    Ok(Extracted {
        extractor_id: "netease".into(),
        canonical_url: Url::parse(&format!("https://music.163.com/#/mv?id={id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?,
        title: json_str(mv, "/name").map(str::to_string),
        author: json_str(mv, "/artistName").map(str::to_string),
        author_url: None,
        description: json_str(mv, "/desc")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: Engagement::packed(json_u64(mv, "/playCount"), None, None, None),
        items: vec![MediaItem {
            id: id.into(),
            kind: MediaKind::Video,
            source: MediaSource::Direct {
                url,
                headers: HeaderMap::new(),
            },
            filename: Some(format!("netease-mv-{id}.mp4")),
            cover,
        }],
    })
}

pub(crate) fn collection_from_api(
    id: &str,
    root: &Value,
    name_ptr: &str,
    cover_ptr: &str,
    tracks_ptr: &str,
    canonical: &str,
) -> Result<Extracted, ExtractError> {
    let tracks: Vec<String> = root
        .pointer(tracks_ptr)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .take(30)
                .map(|s| {
                    let artists = artists_of(s);
                    format!(
                        "{} — {}",
                        s.get("name").and_then(Value::as_str).unwrap_or(""),
                        artists
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let cover = json_str(root, cover_ptr).and_then(|s| Url::parse(s).ok());
    let items = match cover {
        Some(url) => vec![MediaItem {
            id: id.into(),
            kind: MediaKind::Photo,
            source: MediaSource::Direct {
                url,
                headers: HeaderMap::new(),
            },
            filename: Some(format!("netease-collection-{id}.jpg")),
            cover: None,
        }],
        None => Vec::new(),
    };
    Ok(Extracted {
        extractor_id: "netease".into(),
        canonical_url: Url::parse(canonical).map_err(|e| ExtractError::Failed(e.to_string()))?,
        title: json_str(root, name_ptr).map(str::to_string),
        author: None,
        author_url: None,
        description: (!tracks.is_empty()).then_some(tracks.join("\n")),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: None,
        items,
    })
}

fn artists_of(song: &Value) -> String {
    song.get("ar")
        .or_else(|| song.get("artists"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" / ")
        })
        .unwrap_or_default()
}

fn song_filename(title: &str, id: &str, format: &str) -> String {
    let mut safe: String = title
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
            {
                '_'
            } else {
                ch
            }
        })
        .take(120)
        .collect();
    safe = safe.trim_matches([' ', '.']).to_string();
    if safe.is_empty() {
        safe = format!("netease-{id}");
    }
    let extension = match format.to_ascii_lowercase().as_str() {
        "mp3" | "flac" | "m4a" | "aac" | "ogg" | "opus" => format.to_ascii_lowercase(),
        _ => "mp3".into(),
    };
    format!("{safe}.{extension}")
}

fn require_login(a: &Value, b: &Value) -> Result<(), ExtractError> {
    for v in [a, b] {
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = json_str(v, "/message").unwrap_or("");
        if code == 301
            || code == 401
            || msg.contains("登录")
            || msg.to_ascii_lowercase().contains("login")
        {
            return Err(ExtractError::AuthRequired("netease".into()));
        }
    }
    Ok(())
}

pub(crate) fn parse_kind_and_id(url: &Url) -> Option<(NeteaseKind, String)> {
    let hay = format!(
        "{}?{}#{}",
        url.path(),
        url.query().unwrap_or(""),
        url.fragment().unwrap_or("")
    );
    let lower = hay.to_ascii_lowercase();
    let id = url
        .query_pairs()
        .find(|(k, _)| k == "id")
        .map(|(_, v)| v.into_owned())
        .or_else(|| fragment_id(url.fragment().unwrap_or("")))
        .or_else(|| path_id(url.path()))?;
    let kind = if lower.contains("playlist") {
        NeteaseKind::Playlist
    } else if lower.contains("album") {
        NeteaseKind::Album
    } else if lower.contains("/mv") || lower.contains("m=mv") || lower.contains("#/mv") {
        NeteaseKind::Mv
    } else {
        NeteaseKind::Song
    };
    Some((kind, id))
}

fn fragment_id(fragment: &str) -> Option<String> {
    fragment
        .split(['?', '&'])
        .find_map(|part| part.strip_prefix("id=").map(str::to_string))
        .filter(|s| s.chars().all(|c| c.is_ascii_digit()))
}

fn path_id(path: &str) -> Option<String> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    for pair in segs.windows(2) {
        if matches!(pair[0], "song" | "album" | "playlist" | "mv")
            && pair[1].chars().all(|c| c.is_ascii_digit())
        {
            return Some(pair[1].to_string());
        }
    }
    None
}

fn is_netease_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "music.163.com" | "y.music.163.com" | "163cn.tv" | "www.music.163.com"
    )
}

async fn expand_netease(ctx: &ExtractCtx, url: &Url) -> Result<Url, ExtractError> {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if host == "y.music.163.com" || host == "163cn.tv" {
        let resp = ctx
            .http
            .fetch(HttpRequest {
                url: url.clone(),
                headers: HeaderMap::new(),
                method: HttpMethod::Get,
            })
            .await
            .map_err(map_extract_sidecar)?;
        Ok(resp.final_url)
    } else {
        Ok(url.clone())
    }
}

fn cookie_from_req(req: &ExtractRequest) -> Option<String> {
    let raw = std::str::from_utf8(&req.credential.as_ref()?.payload).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn sidecar_err() -> ExtractError {
    ExtractError::Failed("未连接网易云 API".into())
}

fn map_extract_sidecar(err: ExtractError) -> ExtractError {
    match err {
        ExtractError::Network(msg)
            if msg.contains("Connection refused")
                || msg.contains("connection refused")
                || msg.contains("Connect")
                || msg.contains("tcp connect error")
                || msg.contains("timed out")
                || msg.contains("error sending request") =>
        {
            sidecar_err()
        }
        other => other,
    }
}

fn map_auth_sidecar(err: reqwest::Error) -> AuthError {
    if err.is_connect() || err.is_timeout() {
        AuthError::Failed("未连接网易云 API".into())
    } else {
        AuthError::Failed(err.to_string())
    }
}

fn timestamp_ms() -> String {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

fn trim_slash(s: &str) -> String {
    s.trim().trim_end_matches('/').to_string()
}

fn json_str<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str)
}

fn json_u64(v: &Value, pointer: &str) -> Option<u64> {
    v.pointer(pointer).and_then(Value::as_u64).or_else(|| {
        v.pointer(pointer)
            .and_then(Value::as_i64)
            .and_then(|n| u64::try_from(n).ok())
    })
}

pub(crate) fn ncm_poll_code(code: Option<i64>) -> NcmPoll {
    match code {
        Some(800) => NcmPoll::Expired,
        Some(801) => NcmPoll::Waiting,
        Some(802) => NcmPoll::Scanned,
        Some(803) => NcmPoll::Success,
        Some(other) => NcmPoll::Other(other),
        None => NcmPoll::Other(-1),
    }
}

struct NeteaseAuth {
    client: reqwest::Client,
    api_base: String,
}

impl NeteaseAuth {
    fn new(api_base: String) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(format!(
                "Mozilla/5.0 (CongmiaoFeedBot/{} QR Login)",
                env!("CARGO_PKG_VERSION")
            ))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client, api_base }
    }

    async fn check_qr(&self, key: &str, no_cookie: bool) -> Result<reqwest::Response, AuthError> {
        let ts = timestamp_ms();
        let mut req = self
            .client
            .get(format!("{}/login/qr/check", self.api_base))
            .query(&[("key", key), ("timestamp", ts.as_str())]);
        if no_cookie {
            req = req.query(&[("noCookie", "true")]);
        }
        req.send().await.map_err(map_auth_sidecar)
    }
}

#[async_trait]
impl AuthProvider for NeteaseAuth {
    fn methods(&self) -> &[AuthMethod] {
        METHODS
    }

    async fn begin(
        &self,
        method: AuthMethod,
        session: &mut AuthSession,
    ) -> Result<AuthChallenge, AuthError> {
        if method != AuthMethod::QrCode {
            return Err(AuthError::UnsupportedMethod);
        }
        let ts = timestamp_ms();
        let root: Value = self
            .client
            .get(format!("{}/login/qr/key", self.api_base))
            .query(&[("timestamp", ts.as_str())])
            .send()
            .await
            .map_err(map_auth_sidecar)?
            .error_for_status()
            .map_err(|_| AuthError::Failed("未连接网易云 API".into()))?
            .json()
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        let key = json_str(&root, "/data/unikey")
            .ok_or_else(|| AuthError::Failed("网易云响应缺少 unikey".into()))?
            .to_string();
        let created: Value = self
            .client
            .get(format!("{}/login/qr/create", self.api_base))
            .query(&[
                ("key", key.as_str()),
                ("qrimg", "true"),
                ("timestamp", ts.as_str()),
            ])
            .send()
            .await
            .map_err(map_auth_sidecar)?
            .error_for_status()
            .map_err(|_| AuthError::Failed("未连接网易云 API".into()))?
            .json()
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        let qrurl = json_str(&created, "/data/qrurl")
            .ok_or_else(|| AuthError::Failed("网易云响应缺少二维码地址".into()))?;
        session.method = Some(AuthMethod::QrCode);
        session.step = 0;
        session.blob = serde_json::json!({ "unikey": key });
        Ok(AuthChallenge::ShowQr {
            png: render_qr_png(qrurl)?,
            expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(180),
            poll_interval: Duration::from_secs(3),
        })
    }

    async fn advance(
        &self,
        session: &mut AuthSession,
        input: AuthInput,
    ) -> Result<AuthProgress, AuthError> {
        match input {
            AuthInput::Cancel => Err(AuthError::Failed("已取消".into())),
            AuthInput::Text(_) => Err(AuthError::Failed("请扫码登录".into())),
            AuthInput::Poll => {
                let key = session
                    .blob
                    .get("unikey")
                    .and_then(Value::as_str)
                    .ok_or(AuthError::SessionExpired)?
                    .to_string();
                let mut response = self.check_qr(&key, false).await?;
                if response.status().as_u16() == 502 {
                    response = self.check_qr(&key, true).await?;
                }
                if response.status().is_server_error() {
                    return Err(AuthError::Failed("未连接网易云 API".into()));
                }
                let root: Value = response
                    .error_for_status()
                    .map_err(|e| AuthError::Failed(e.to_string()))?
                    .json()
                    .await
                    .map_err(|e| AuthError::Failed(e.to_string()))?;
                match ncm_poll_code(root.get("code").and_then(Value::as_i64)) {
                    NcmPoll::Waiting => Ok(AuthProgress::Pending),
                    NcmPoll::Scanned => {
                        if session.step == 0 {
                            session.step = 1;
                            Ok(AuthProgress::Hint("已扫码，请在客户端确认".into()))
                        } else {
                            Ok(AuthProgress::Pending)
                        }
                    }
                    NcmPoll::Expired => Err(AuthError::SessionExpired),
                    NcmPoll::Success => {
                        let cookie = root
                            .get("cookie")
                            .and_then(Value::as_str)
                            .filter(|v| !v.is_empty())
                            .ok_or_else(|| {
                                AuthError::Failed("网易云扫码成功但响应中没有 Cookie".into())
                            })?;
                        Ok(AuthProgress::Completed(Credential {
                            extractor_id: "netease".into(),
                            method: AuthMethod::QrCode,
                            payload: cookie.as_bytes().to_vec(),
                            expires_at: None,
                        }))
                    }
                    NcmPoll::Other(code) => Err(AuthError::Failed(format!(
                        "网易云扫码状态 {code}: {}",
                        root.get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("未知错误")
                    ))),
                }
            }
        }
    }

    async fn check(&self, credential: &Credential) -> Result<CredentialStatus, AuthError> {
        let Ok(raw) = std::str::from_utf8(&credential.payload) else {
            return Ok(CredentialStatus::Invalid);
        };
        let cookie = raw.trim();
        if cookie.is_empty() {
            return Ok(CredentialStatus::Invalid);
        }
        let ts = timestamp_ms();
        let resp = self
            .client
            .get(format!("{}/login/status", self.api_base))
            .query(&[("cookie", cookie), ("timestamp", ts.as_str())])
            .send()
            .await
            .map_err(map_auth_sidecar)?;
        if resp.status().is_server_error() {
            return Err(AuthError::Failed("未连接网易云 API".into()));
        }
        let root: Value = resp
            .json()
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        let code = root
            .pointer("/data/code")
            .or_else(|| root.get("code"))
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        if code == 200 || root.pointer("/data/profile").is_some() {
            Ok(CredentialStatus::Valid)
        } else {
            Ok(CredentialStatus::Invalid)
        }
    }

    async fn refresh(&self, _credential: &Credential) -> Result<Option<Credential>, AuthError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn poll_codes_map() {
        assert_eq!(ncm_poll_code(Some(800)), NcmPoll::Expired);
        assert_eq!(ncm_poll_code(Some(801)), NcmPoll::Waiting);
        assert_eq!(ncm_poll_code(Some(802)), NcmPoll::Scanned);
        assert_eq!(ncm_poll_code(Some(803)), NcmPoll::Success);
    }

    #[test]
    fn url_hit_and_ids() {
        let ext = NeteaseExtractor::default();
        assert!(ext.matches(&url("https://music.163.com/#/song?id=65")));
        assert!(ext.matches(&url("https://y.music.163.com/m/song?id=1")));
        assert!(ext.matches(&url("https://163cn.tv/xx")));
        assert!(!ext.matches(&url("https://youtube.com/watch?v=a")));
        assert!(!ext.matches(&url("https://music.163.com/")));
        let reg = crate::ExtractorRegistry::builtin();
        for name in ["netease", "ncm", "163", "网易云"] {
            assert_eq!(reg.resolve_name(name).unwrap().id(), "netease");
        }
        assert_eq!(
            parse_kind_and_id(&url("https://music.163.com/#/song?id=65")),
            Some((NeteaseKind::Song, "65".into()))
        );
        assert_eq!(
            parse_kind_and_id(&url("https://music.163.com/song?id=65")),
            Some((NeteaseKind::Song, "65".into()))
        );
        assert_eq!(
            parse_kind_and_id(&url("https://music.163.com/#/playlist?id=9")),
            Some((NeteaseKind::Playlist, "9".into()))
        );
        assert_eq!(
            parse_kind_and_id(&url("https://music.163.com/mv?id=3")),
            Some((NeteaseKind::Mv, "3".into()))
        );
        assert_eq!(
            parse_kind_and_id(&url("https://music.163.com/album?id=8")),
            Some((NeteaseKind::Album, "8".into()))
        );
    }

    #[test]
    fn song_fixture_is_audio() {
        let detail = serde_json::json!({
            "songs": [{
                "name": "song",
                "ar": [{ "name": "A" }, { "name": "B" }],
                "al": { "name": "album", "picUrl": "https://p1.music.126.net/c.jpg" }
            }]
        });
        let audio = serde_json::json!({
            "data": [{ "url": "https://m801.music.126.net/a.mp3", "type": "mp3" }]
        });
        let extracted = song_from_api("65", &detail, &audio).unwrap();
        assert_eq!(extracted.items[0].kind, MediaKind::Audio);
        assert_eq!(extracted.author.as_deref(), Some("A / B"));
        assert!(extracted.items[0].cover.is_some());
        let missing = song_from_api("65", &detail, &serde_json::json!({"data":[{"url":null}]}));
        assert!(matches!(missing, Err(ExtractError::Failed(m)) if m.contains("无可用播放地址")));
    }

    #[test]
    fn collection_lists_tracks() {
        let root = serde_json::json!({
            "playlist": {
                "name": "pl",
                "coverImgUrl": "https://p1.music.126.net/c.jpg",
                "tracks": [
                    { "name": "t1", "ar": [{ "name": "a" }] },
                    { "name": "t2", "ar": [{ "name": "b" }] }
                ]
            }
        });
        let extracted = collection_from_api(
            "9",
            &root,
            "/playlist/name",
            "/playlist/coverImgUrl",
            "/playlist/tracks",
            "https://music.163.com/#/playlist?id=9",
        )
        .unwrap();
        assert_eq!(extracted.items[0].kind, MediaKind::Photo);
        assert_eq!(extracted.title.as_deref(), Some("pl"));
        assert!(extracted.description.as_ref().unwrap().contains("t1 — a"));
    }
}
