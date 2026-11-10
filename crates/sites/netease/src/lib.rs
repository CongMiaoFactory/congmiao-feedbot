//! NetEase Cloud Music via ncm-api sidecar.

use async_trait::async_trait;
use feedbot_core::auth::{
    AuthChallenge, AuthError, AuthInput, AuthMethod, AuthProgress, AuthProvider, AuthSession,
    Credential, CredentialStatus,
};
use feedbot_core::extract::{
    Engagement, ExtractCtx, ExtractError, ExtractRequest, Extracted, Extractor, FeedEntry,
    HttpClient, HttpRequest, HttpResponse, MediaItem, MediaKind, MediaSource,
};
use feedbot_core::options::{OptionDef, OptionDefault, OptionKind, OptionSpec, OptionValue};
use feedbot_core::{Present, SiteMeta};
use feedbot_site_common::render_qr_png;
use http::HeaderMap;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use time::OffsetDateTime;
use url::Url;

use feedbot_site_common::{credential_text, json_str, json_u64, parse_url, trim_slash};

/// Upper bound on track ids remembered per playlist/album subscription poll.
const FEED_LIMIT: usize = 1000;

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
    pub fn new(endpoints: NeteaseEndpoints, http: Arc<dyn HttpClient>) -> Self {
        let base = trim_slash(&endpoints.api_base);
        Self {
            auth: NeteaseAuth {
                http,
                api_base: base.clone(),
            },
            api_base: base,
        }
    }
}

const META: SiteMeta = SiteMeta {
    id: "netease",
    name: "网易云音乐",
    aliases: &["ncm", "163", "网易云"],
    priority: 40,
    kinds: &["歌曲", "MV", "专辑", "歌单"],
    examples: &[
        "music.163.com/song?id=",
        "music.163.com/mv?id=",
        "music.163.com/album?id=",
        "music.163.com/playlist?id=",
    ],
    feed_examples: &["music.163.com/playlist?id=", "music.163.com/album?id="],
};

#[async_trait]
impl Extractor for NeteaseExtractor {
    fn meta(&self) -> SiteMeta {
        META
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
        is_netease_host(url.host_str().unwrap_or(""))
            && matches!(
                parse_kind_and_id(url),
                Some((NeteaseKind::Album | NeteaseKind::Playlist, _))
            )
    }

    async fn feed(
        &self,
        ctx: &ExtractCtx,
        req: &ExtractRequest,
    ) -> Result<Vec<FeedEntry>, ExtractError> {
        let cookie = cookie_from_req(req);
        let (path, ids_ptr, fallback_ptr, id) = match parse_kind_and_id(&req.url) {
            Some((NeteaseKind::Playlist, id)) => (
                "/playlist/detail",
                "/playlist/trackIds",
                "/playlist/tracks",
                id,
            ),
            Some((NeteaseKind::Album, id)) => ("/album", "/songs", "/songs", id),
            _ => return Err(ExtractError::Unsupported),
        };
        let root = self
            .sidecar_get(ctx, path, &[("id", &id)], cookie.as_deref())
            .await?;
        require_login(&root, &Value::Null)?;
        let list = root
            .pointer(ids_ptr)
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .or_else(|| root.pointer(fallback_ptr).and_then(Value::as_array))
            .ok_or_else(|| ExtractError::Failed("网易云响应缺少曲目列表".into()))?;
        list.iter()
            .filter_map(|track| json_u64(track, "/id"))
            .take(FEED_LIMIT)
            .map(|song| {
                Ok(FeedEntry {
                    key: song.to_string(),
                    url: parse_url(&format!("https://music.163.com/#/song?id={song}"))?,
                })
            })
            .collect()
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
            NeteaseKind::Album => {
                self.extract_collection(ctx, &id, true, cookie.as_deref())
                    .await
            }
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
        let mut url = Url::parse(&format!("{}{path}", self.api_base)).map_err(|_| sidecar_err())?;
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
            .fetch(HttpRequest::get(url, HeaderMap::new()))
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
            .sidecar_get(
                ctx,
                "/song/url/v1",
                &[("id", id), ("level", "standard")],
                cookie,
            )
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
        let root = self.sidecar_get(ctx, path, &[("id", id)], cookie).await?;
        require_login(&root, &Value::Null)?;
        collection_from_api(id, &root, name_ptr, cover_ptr, tracks_ptr, &canonical)
    }
}

pub(crate) fn song_from_api(
    id: &str,
    detail: &Value,
    audio: &Value,
) -> Result<Extracted, ExtractError> {
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

pub(crate) fn mv_from_api(
    id: &str,
    detail: &Value,
    urls: &Value,
) -> Result<Extracted, ExtractError> {
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
            .fetch(HttpRequest::get(url.clone(), HeaderMap::new()))
            .await
            .map_err(map_extract_sidecar)?;
        Ok(resp.final_url)
    } else {
        Ok(url.clone())
    }
}

fn cookie_from_req(req: &ExtractRequest) -> Option<String> {
    credential_text(req).map(str::to_string)
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

fn map_auth_sidecar(err: ExtractError) -> AuthError {
    match err {
        ExtractError::Network(_) => AuthError::Failed("未连接网易云 API".into()),
        other => AuthError::Failed(other.to_string()),
    }
}

fn timestamp_ms() -> String {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
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
    http: Arc<dyn HttpClient>,
    api_base: String,
}

impl NeteaseAuth {
    async fn sidecar(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<HttpResponse, AuthError> {
        let mut url = Url::parse(&format!("{}{path}", self.api_base))
            .map_err(|_| AuthError::Failed("未连接网易云 API".into()))?;
        url.query_pairs_mut()
            .extend_pairs(params)
            .append_pair("timestamp", &timestamp_ms());
        self.http
            .fetch(HttpRequest::get(url, HeaderMap::new()))
            .await
            .map_err(map_auth_sidecar)
    }

    /// Sidecar GET that must succeed with a JSON body.
    async fn sidecar_json(&self, path: &str, params: &[(&str, &str)]) -> Result<Value, AuthError> {
        let resp = self.sidecar(path, params).await?;
        if !(200..300).contains(&resp.status) {
            return Err(AuthError::Failed("未连接网易云 API".into()));
        }
        serde_json::from_slice(&resp.body).map_err(|e| AuthError::Failed(e.to_string()))
    }

    async fn check_qr(&self, key: &str, no_cookie: bool) -> Result<HttpResponse, AuthError> {
        let mut params = vec![("key", key)];
        if no_cookie {
            params.push(("noCookie", "true"));
        }
        self.sidecar("/login/qr/check", &params).await
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
        let root = self.sidecar_json("/login/qr/key", &[]).await?;
        let key = json_str(&root, "/data/unikey")
            .ok_or_else(|| AuthError::Failed("网易云响应缺少 unikey".into()))?
            .to_string();
        let created = self
            .sidecar_json(
                "/login/qr/create",
                &[("key", key.as_str()), ("qrimg", "true")],
            )
            .await?;
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
                if response.status == 502 {
                    response = self.check_qr(&key, true).await?;
                }
                if response.status >= 500 {
                    return Err(AuthError::Failed("未连接网易云 API".into()));
                }
                if !(200..300).contains(&response.status) {
                    return Err(AuthError::Failed(format!("HTTP {}", response.status)));
                }
                let root: Value = serde_json::from_slice(&response.body)
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
        let resp = self.sidecar("/login/status", &[("cookie", cookie)]).await?;
        if resp.status >= 500 {
            return Err(AuthError::Failed("未连接网易云 API".into()));
        }
        let root: Value =
            serde_json::from_slice(&resp.body).map_err(|e| AuthError::Failed(e.to_string()))?;
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
        let ext = NeteaseExtractor::new(
            NeteaseEndpoints::default(),
            feedbot_site_common::testing::no_http(),
        );
        assert!(ext.matches(&url("https://music.163.com/#/song?id=65")));
        assert!(ext.matches(&url("https://y.music.163.com/m/song?id=1")));
        assert!(ext.matches(&url("https://163cn.tv/xx")));
        assert!(!ext.matches(&url("https://youtube.com/watch?v=a")));
        assert!(!ext.matches(&url("https://music.163.com/")));
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

    mod flow {
        use super::*;
        use feedbot_core::options::OptionValues;
        use feedbot_site_common::testing::{FakeHttp, request};
        use serde_json::json;

        const API: &str = "127.0.0.1";

        fn route(path: &str) -> String {
            format!("{API}{path}")
        }

        fn query(http: &FakeHttp, path: &str) -> Vec<(String, String)> {
            http.last(&route(path))
                .url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        }

        fn pair(k: &str, v: &str) -> (String, String) {
            (k.into(), v.into())
        }

        #[tokio::test]
        async fn song_forwards_login_cookie() {
            let http = Arc::new(
                FakeHttp::new()
                    .json(
                        &route("/song/detail"),
                        json!({ "code": 200, "songs": [{
                        "name": "歌", "ar": [{ "name": "A" }], "al": { "name": "专辑" }
                    }] }),
                    )
                    .json(
                        &route("/song/url/v1"),
                        json!({ "code": 200, "data": [
                        { "url": "https://m801.music.126.net/a.flac", "type": "flac" }
                    ] }),
                    ),
            );
            let ext = NeteaseExtractor::new(Default::default(), http.clone());
            let req = request(
                "netease",
                "https://music.163.com/#/song?id=65",
                OptionValues::new(),
                Some("MUSIC_U=u"),
            );
            let extracted = ext.extract(&http.ctx(), &req).await.unwrap();
            assert_eq!(extracted.title.as_deref(), Some("歌"));
            assert_eq!(extracted.items[0].filename.as_deref(), Some("歌.flac"));
            assert_eq!(
                query(&http, "/song/url/v1"),
                [
                    pair("id", "65"),
                    pair("level", "standard"),
                    pair("cookie", "MUSIC_U=u")
                ]
            );
        }

        #[tokio::test]
        async fn mv_uses_quality_option() {
            let http = Arc::new(
                FakeHttp::new()
                    .json(
                        &route("/mv/detail"),
                        json!({ "code": 200, "data": {
                        "name": "MV", "artistName": "A", "playCount": 9
                    } }),
                    )
                    .json(
                        &route("/mv/url"),
                        json!({ "code": 200, "data": {
                        "url": "https://vodkgeyttp8.vod.126.net/mv.mp4"
                    } }),
                    ),
            );
            let ext = NeteaseExtractor::new(Default::default(), http.clone());
            let options = ext
                .resolve_options(&OptionValues::new(), &["+q:720".into()])
                .unwrap();
            let req = request("netease", "https://music.163.com/mv?id=5", options, None);
            let extracted = ext.extract(&http.ctx(), &req).await.unwrap();
            assert_eq!(extracted.items[0].kind, MediaKind::Video);
            assert_eq!(extracted.engagement.unwrap().views, Some(9));
            assert_eq!(query(&http, "/mv/url"), [pair("id", "5"), pair("r", "720")]);
        }

        #[tokio::test]
        async fn login_and_sidecar_errors() {
            let http = Arc::new(
                FakeHttp::new()
                    .json(&route("/song/detail"), json!({ "code": 200, "songs": [] }))
                    .json(
                        &route("/song/url/v1"),
                        json!({ "code": 301, "message": "需要登录" }),
                    ),
            );
            let ext = NeteaseExtractor::new(Default::default(), http.clone());
            let req = request(
                "netease",
                "https://music.163.com/song?id=65",
                OptionValues::new(),
                None,
            );
            let err = ext.extract(&http.ctx(), &req).await.unwrap_err();
            assert!(matches!(err, ExtractError::AuthRequired(_)), "{err:?}");

            let http = Arc::new(FakeHttp::new().status(&route("/song/detail"), 502, ""));
            let ext = NeteaseExtractor::new(Default::default(), http.clone());
            let err = ext.extract(&http.ctx(), &req).await.unwrap_err();
            assert_eq!(err.to_string(), sidecar_err().to_string());
        }

        #[tokio::test]
        async fn playlist_feed_prefers_track_ids() {
            let detail = |ids: Value| {
                json!({ "code": 200, "playlist": {
                    "trackIds": ids,
                    "tracks": [{ "id": 1 }]
                } })
            };
            let req = request(
                "netease",
                "https://music.163.com/#/playlist?id=9",
                OptionValues::new(),
                None,
            );
            for (ids, keys) in [
                (json!([{ "id": 3 }, { "id": 2 }]), vec!["3", "2"]),
                (json!([]), vec!["1"]),
            ] {
                let http = Arc::new(FakeHttp::new().json(&route("/playlist/detail"), detail(ids)));
                let ext = NeteaseExtractor::new(Default::default(), http.clone());
                let entries = ext.feed(&http.ctx(), &req).await.unwrap();
                let got: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
                assert_eq!(got, keys);
            }
        }
    }
}
