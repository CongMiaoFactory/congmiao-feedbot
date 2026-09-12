//! Bilibili QR login and media extract.

use crate::http::expand_shortlink;
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
use http::{HeaderMap, HeaderValue, header};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;
use time::OffsetDateTime;
use url::Url;

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const ACCEPT: &str = "application/json, text/plain, */*";
const ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.9,en;q=0.8";
const OPUS_FEATURES: &str = "itemOpusStyle,opusBigCover,onlyfansVote,endFooterHidden,decorationCard,onlyfansAssetsV2,ugcDelete,onlyfansQaCard,commentsNewVersion";
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
            desc: "视频最高高度",
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
pub struct BilibiliEndpoints {
    pub passport_base: String,
    pub api_base: String,
    pub live_api_base: String,
}

impl Default for BilibiliEndpoints {
    fn default() -> Self {
        Self {
            passport_base: "https://passport.bilibili.com".into(),
            api_base: "https://api.bilibili.com".into(),
            live_api_base: "https://api.live.bilibili.com".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BilibiliTarget {
    Video(String),
    Bangumi(String),
    Dynamic(String),
    Live(String),
    Audio(String),
    Article(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BiliPoll {
    Waiting,
    Scanned,
    Success,
    Expired,
    Other(i64),
}

pub struct BilibiliExtractor {
    auth: BilibiliAuth,
    api_base: String,
    live_api_base: String,
    www_base: String,
    device_cookie: Mutex<Option<String>>,
}

impl BilibiliExtractor {
    pub fn new(endpoints: BilibiliEndpoints) -> Self {
        let auth = BilibiliAuth::new(&endpoints);
        Self {
            auth,
            api_base: trim_slash(&endpoints.api_base),
            live_api_base: trim_slash(&endpoints.live_api_base),
            www_base: "https://www.bilibili.com".into(),
            device_cookie: Mutex::new(None),
        }
    }
}

impl Default for BilibiliExtractor {
    fn default() -> Self {
        Self::new(BilibiliEndpoints::default())
    }
}

#[async_trait]
impl Extractor for BilibiliExtractor {
    fn id(&self) -> &'static str {
        "bilibili"
    }
    fn display_name(&self) -> &'static str {
        "Bilibili"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["bili", "b站"]
    }
    fn priority(&self) -> i32 {
        50
    }
    fn matches(&self, url: &Url) -> bool {
        (is_bilibili_host(url.host_str().unwrap_or("")) && parse_target(url).is_some())
            || is_short_bili_host(url.host_str().unwrap_or(""))
    }
    fn options(&self) -> &'static OptionSpec {
        &SPEC
    }
    fn auth(&self) -> Option<&dyn AuthProvider> {
        Some(&self.auth)
    }
    fn extract_kinds(&self) -> &'static [&'static str] {
        &["视频", "番剧", "直播封面", "动态", "音频", "专栏"]
    }
    fn url_examples(&self) -> &'static [&'static str] {
        &[
            "/video/BV...",
            "/bangumi/play/ep...",
            "live.bilibili.com/{room}",
            "/opus/{id}",
            "/audio/au...",
            "/read/cv...",
        ]
    }

    async fn extract(
        &self,
        ctx: &ExtractCtx,
        req: &ExtractRequest,
    ) -> Result<Extracted, ExtractError> {
        let url = expand_if_short(ctx, &req.url).await?;
        let target = parse_target(&url).ok_or(ExtractError::Unsupported)?;
        let cookie = cookie_from_req(req);
        match target {
            BilibiliTarget::Live(room) => self.extract_live(ctx, &room, cookie.as_deref()).await,
            BilibiliTarget::Dynamic(id) => {
                self.extract_dynamic(ctx, &id, cookie.as_deref(), &req.options)
                    .await
            }
            BilibiliTarget::Audio(id) => self.extract_audio(ctx, &id, cookie.as_deref()).await,
            BilibiliTarget::Article(id) => self.extract_article(ctx, &id, cookie.as_deref()).await,
            BilibiliTarget::Video(id) => {
                self.extract_video(ctx, Some(&id), None, cookie.as_deref(), &req.options)
                    .await
            }
            BilibiliTarget::Bangumi(id) => {
                self.extract_bangumi(ctx, &id, cookie.as_deref(), &req.options)
                    .await
            }
        }
    }
}

impl BilibiliExtractor {
    async fn extract_video(
        &self,
        ctx: &ExtractCtx,
        bvid: Option<&str>,
        aid: Option<&str>,
        cookie: Option<&str>,
        options: &feedbot_core::options::OptionValues,
    ) -> Result<Extracted, ExtractError> {
        let mut params = Vec::new();
        if let Some(v) = bvid.filter(|s| is_bvid(s)) {
            params.push(("bvid".into(), v.to_string()));
        } else if let Some(v) = bvid.and_then(av_digits).or(aid.map(str::to_string)) {
            params.push(("aid".into(), v));
        } else if let Some(v) = bvid {
            if is_bvid(v) {
                params.push(("bvid".into(), v.to_string()));
            } else if let Some(n) = av_digits(v) {
                params.push(("aid".into(), n));
            } else {
                return Err(ExtractError::Unsupported);
            }
        } else {
            return Err(ExtractError::Unsupported);
        }
        let view = self
            .api_json(ctx, &self.api_base, "/x/web-interface/view", &params, cookie)
            .await?;
        let data = view
            .get("data")
            .cloned()
            .ok_or_else(|| ExtractError::Failed("视频详情缺少 data".into()))?;
        extracted_from_view(self, ctx, data, cookie, options).await
    }

    async fn extract_bangumi(
        &self,
        ctx: &ExtractCtx,
        id: &str,
        cookie: Option<&str>,
        options: &feedbot_core::options::OptionValues,
    ) -> Result<Extracted, ExtractError> {
        let digits = id
            .get(2..)
            .filter(|s| s.chars().all(|c| c.is_ascii_digit()))
            .ok_or(ExtractError::Unsupported)?;
        let key = if id.len() >= 2 && id[..2].eq_ignore_ascii_case("ss") {
            "season_id"
        } else {
            "ep_id"
        };
        let root = self
            .api_json(
                ctx,
                &self.api_base,
                "/pgc/view/web/season",
                &[(key.into(), digits.into())],
                cookie,
            )
            .await?;
        let episodes = root
            .pointer("/result/episodes")
            .and_then(Value::as_array)
            .ok_or_else(|| ExtractError::Failed("番剧缺少分集".into()))?;
        let episode = if key == "ep_id" {
            episodes
                .iter()
                .find(|e| json_u64(e, "/id").map(|n| n.to_string()) == Some(digits.into()))
                .or_else(|| episodes.first())
        } else {
            episodes.first()
        }
        .ok_or_else(|| ExtractError::Failed("番剧没有分集".into()))?;
        let bvid = json_str(episode, "/bvid").ok_or_else(|| ExtractError::Failed("番剧缺少 bvid".into()))?;
        self.extract_video(ctx, Some(bvid), None, cookie, options)
            .await
    }

    async fn extract_live(
        &self,
        ctx: &ExtractCtx,
        room: &str,
        cookie: Option<&str>,
    ) -> Result<Extracted, ExtractError> {
        let root = self
            .api_json(
                ctx,
                &self.live_api_base,
                "/xlive/web-room/v1/index/getInfoByRoom",
                &[("room_id".into(), room.into())],
                cookie,
            )
            .await?;
        live_from_room(room, &root, &media_headers(cookie)?)
    }

    async fn extract_dynamic(
        &self,
        ctx: &ExtractCtx,
        id: &str,
        cookie: Option<&str>,
        options: &feedbot_core::options::OptionValues,
    ) -> Result<Extracted, ExtractError> {
        let root = self
            .api_json(
                ctx,
                &self.api_base,
                "/x/polymer/web-dynamic/v1/detail",
                &[
                    ("id".into(), id.into()),
                    ("features".into(), OPUS_FEATURES.into()),
                ],
                cookie,
            )
            .await?;
        let item = root
            .pointer("/data/item")
            .filter(|v| !v.is_null())
            .cloned()
            .ok_or_else(|| ExtractError::Failed("动态不存在".into()))?;
        let mut extracted = dynamic_from_item(id, &item, &media_headers(cookie)?)?;
        if needs_opus(&item) {
            if let Ok(opus) = self.fetch_opus(ctx, source_id(&item, id), cookie).await {
                apply_opus(&mut extracted, opus);
            }
        }
        if extracted.items.is_empty() {
            if let Some(url) = embedded_url(item.pointer("/modules/module_dynamic/major").unwrap_or(&Value::Null))
            {
                if let Ok(parsed) = Url::parse(&url) {
                    if let Some(target) = parse_target(&parsed) {
                        if let Ok(mut nested) = self
                            .extract_nested(ctx, target, cookie, options)
                            .await
                        {
                            if extracted.title.as_deref().is_none_or(str::is_empty) {
                                extracted.title = nested.title.take();
                            }
                            extracted.items.append(&mut nested.items);
                        }
                    }
                }
            }
        }
        Ok(extracted)
    }

    async fn extract_nested(
        &self,
        ctx: &ExtractCtx,
        target: BilibiliTarget,
        cookie: Option<&str>,
        options: &feedbot_core::options::OptionValues,
    ) -> Result<Extracted, ExtractError> {
        match target {
            BilibiliTarget::Video(id) => {
                self.extract_video(ctx, Some(&id), None, cookie, options)
                    .await
            }
            BilibiliTarget::Bangumi(id) => self.extract_bangumi(ctx, &id, cookie, options).await,
            BilibiliTarget::Live(room) => self.extract_live(ctx, &room, cookie).await,
            BilibiliTarget::Audio(id) => self.extract_audio(ctx, &id, cookie).await,
            BilibiliTarget::Article(id) => self.extract_article(ctx, &id, cookie).await,
            BilibiliTarget::Dynamic(_) => Err(ExtractError::Unsupported),
        }
    }

    async fn fetch_opus(
        &self,
        ctx: &ExtractCtx,
        id: &str,
        cookie: Option<&str>,
    ) -> Result<OpusProjection, ExtractError> {
        let root = self
            .api_json(
                ctx,
                &self.api_base,
                "/x/polymer/web-dynamic/v1/opus/detail",
                &[
                    ("id".into(), id.into()),
                    ("features".into(), OPUS_FEATURES.into()),
                    ("timezone_offset".into(), "-480".into()),
                ],
                cookie,
            )
            .await?;
        let item = root
            .pointer("/data/item")
            .ok_or_else(|| ExtractError::Failed("Opus 详情缺少 item".into()))?;
        Ok(opus_from_item(item))
    }

    async fn extract_audio(
        &self,
        ctx: &ExtractCtx,
        id: &str,
        cookie: Option<&str>,
    ) -> Result<Extracted, ExtractError> {
        let sid = id.trim_start_matches(|c: char| !c.is_ascii_digit());
        let song = self
            .api_json(
                ctx,
                &self.www_base,
                "/audio/music-service-c/web/song/info",
                &[("sid".into(), sid.into())],
                cookie,
            )
            .await?;
        let stream = self
            .api_json(
                ctx,
                &self.www_base,
                "/audio/music-service-c/web/url",
                &[
                    ("sid".into(), sid.into()),
                    ("quality".into(), "2".into()),
                    ("privilege".into(), "2".into()),
                ],
                cookie,
            )
            .await?;
        audio_from_api(sid, &song, &stream, &media_headers(cookie)?)
    }

    async fn extract_article(
        &self,
        ctx: &ExtractCtx,
        id: &str,
        cookie: Option<&str>,
    ) -> Result<Extracted, ExtractError> {
        let cv = id.trim_start_matches(|c: char| !c.is_ascii_digit());
        let root = self
            .api_json(
                ctx,
                &self.api_base,
                "/x/article/viewinfo",
                &[
                    ("id".into(), cv.into()),
                    ("mobi_app".into(), "pc".into()),
                    ("from".into(), "web".into()),
                ],
                cookie,
            )
            .await?;
        article_from_api(cv, &root, &media_headers(cookie)?)
    }

    async fn api_json(
        &self,
        ctx: &ExtractCtx,
        base: &str,
        path: &str,
        params: &[(String, String)],
        cookie: Option<&str>,
    ) -> Result<Value, ExtractError> {
        self.ensure_buvid(ctx, false).await;
        for attempt in 0..2 {
            let headers = self.request_headers(cookie)?;
            let url = build_url(base, path, params)?;
            let resp = ctx
                .http
                .fetch(HttpRequest {
                    url,
                    headers,
                    method: HttpMethod::Get,
                })
                .await?;
            let value: Value = serde_json::from_slice(&resp.body)
                .map_err(|e| ExtractError::Failed(e.to_string()))?;
            let code = value.get("code").and_then(Value::as_i64).unwrap_or_default();
            if code == -352 && attempt == 0 {
                self.ensure_buvid(ctx, true).await;
                continue;
            }
            return map_bili_code(code, value);
        }
        Err(ExtractError::Failed(
            "Bilibili 风控校验失败，请稍后重试".into(),
        ))
    }

    async fn ensure_buvid(&self, ctx: &ExtractCtx, force: bool) {
        {
            let guard = self.device_cookie.lock().unwrap_or_else(|e| e.into_inner());
            if !force && guard.as_deref().is_some_and(|c| cookie_has_value(c, "buvid3")) {
                return;
            }
        }
        let Ok(url) = Url::parse(&format!("{}/x/frontend/finger/spi", self.api_base)) else {
            return;
        };
        let Ok(headers) = browser_headers(None) else {
            return;
        };
        let Ok(resp) = ctx
            .http
            .fetch(HttpRequest {
                url,
                headers,
                method: HttpMethod::Get,
            })
            .await
        else {
            return;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&resp.body) else {
            return;
        };
        let Some(buvid3) = json_str(&value, "/data/b_3").filter(|s| !s.is_empty()) else {
            return;
        };
        let buvid4 = json_str(&value, "/data/b_4").filter(|s| !s.is_empty());
        let mut pairs = BTreeMap::new();
        pairs.insert("buvid3".into(), buvid3.to_string());
        if let Some(v) = buvid4 {
            pairs.insert("buvid4".into(), v.to_string());
        }
        let joined = join_pairs(&pairs);
        if let Ok(mut guard) = self.device_cookie.lock() {
            *guard = Some(joined);
        }
    }

    fn request_headers(&self, account: Option<&str>) -> Result<HeaderMap, ExtractError> {
        let device = self
            .device_cookie
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let merged = merge_cookies(account, device.as_deref());
        browser_headers(merged.as_deref())
    }
}

async fn extracted_from_view(
    this: &BilibiliExtractor,
    ctx: &ExtractCtx,
    data: Value,
    cookie: Option<&str>,
    options: &feedbot_core::options::OptionValues,
) -> Result<Extracted, ExtractError> {
    let cid = json_u64(&data, "/cid")
        .ok_or_else(|| ExtractError::Failed("视频详情缺少 cid".into()))?
        .to_string();
    let bvid = json_str(&data, "/bvid")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ExtractError::Failed("视频详情缺少 bvid".into()))?
        .to_string();
    let height = quality_height(options);
    let qn = qn_for(height);
    let play = this
        .api_json(
            ctx,
            &this.api_base,
            "/x/player/playurl",
            &[
                ("bvid".into(), bvid.clone()),
                ("cid".into(), cid),
                ("qn".into(), qn.into()),
                ("fnval".into(), "4048".into()),
                ("fourk".into(), "1".into()),
            ],
            cookie,
        )
        .await?;
    video_from_view_play(&bvid, &data, play.get("data").unwrap_or(&Value::Null), height, cookie)
}

fn quality_height(options: &feedbot_core::options::OptionValues) -> u32 {
    match options.get("q") {
        Some(OptionValue::Enum(q)) => q.parse().unwrap_or(1080),
        _ => 1080,
    }
}

fn qn_for(height: u32) -> &'static str {
    match height {
        1080.. => "80",
        720..=1079 => "64",
        480..=719 => "32",
        _ => "16",
    }
}

pub(crate) fn video_from_view_play(
    bvid: &str,
    data: &Value,
    play: &Value,
    target_height: u32,
    cookie: Option<&str>,
) -> Result<Extracted, ExtractError> {
    let headers = media_headers(cookie)?;
    let source = with_headers(
        play_source(play, target_height)
            .ok_or_else(|| ExtractError::Failed("没有可用的视频地址".into()))?,
        headers,
    );
    let cover = json_str(data, "/pic").and_then(parse_https_url);
    let owner_mid = json_u64(data, "/owner/mid");
    Ok(Extracted {
        extractor_id: "bilibili".into(),
        canonical_url: Url::parse(&format!("https://www.bilibili.com/video/{bvid}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?,
        title: json_str(data, "/title").map(str::to_string),
        author: json_str(data, "/owner/name").map(str::to_string),
        author_url: owner_mid.and_then(|n| Url::parse(&format!("https://space.bilibili.com/{n}")).ok()),
        description: json_str(data, "/desc")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: Engagement::packed(
            json_u64(data, "/stat/view"),
            json_u64(data, "/stat/like"),
            json_u64(data, "/stat/share"),
            json_u64(data, "/stat/reply"),
        ),
        items: vec![MediaItem {
            id: bvid.into(),
            kind: MediaKind::Video,
            source,
            filename: Some(format!("bilibili-{bvid}.mp4")),
            cover,
        }],
    })
}

fn with_headers(source: MediaSource, headers: HeaderMap) -> MediaSource {
    match source {
        MediaSource::Direct { url, .. } => MediaSource::Direct { url, headers },
        MediaSource::Separated { video, audio, .. } => MediaSource::Separated {
            video,
            audio,
            headers,
        },
        other => other,
    }
}

fn play_source(play: &Value, target_height: u32) -> Option<MediaSource> {
    if let Some(url) = json_str(play, "/durl/0/url").and_then(parse_https_url) {
        return Some(MediaSource::Direct {
            url,
            headers: HeaderMap::new(),
        });
    }
    let videos = play.pointer("/dash/video").and_then(Value::as_array)?;
    let selected = pick_video_stream(videos, target_height)?;
    let video = json_str(selected, "/baseUrl")
        .or_else(|| json_str(selected, "/base_url"))
        .and_then(parse_https_url)?;
    let audio = play
        .pointer("/dash/audio")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|a| {
            json_str(a, "/baseUrl")
                .or_else(|| json_str(a, "/base_url"))
                .and_then(parse_https_url)
        });
    Some(match audio {
        Some(audio) => MediaSource::Separated {
            video,
            audio,
            headers: HeaderMap::new(),
        },
        None => MediaSource::Direct {
            url: video,
            headers: HeaderMap::new(),
        },
    })
}

fn pick_video_stream(streams: &[Value], target: u32) -> Option<&Value> {
    let target = u64::from(target);
    let within = |s: &&Value| json_u64(s, "/height").unwrap_or(u64::MAX) <= target;
    let avc = |s: &&Value| {
        json_u64(s, "/codecid") == Some(7)
            || json_str(s, "/codecs").is_some_and(|c| c.starts_with("avc"))
    };
    streams
        .iter()
        .filter(within)
        .filter(avc)
        .max_by_key(|s| json_u64(s, "/height").unwrap_or_default())
        .or_else(|| {
            streams
                .iter()
                .filter(within)
                .max_by_key(|s| json_u64(s, "/height").unwrap_or_default())
        })
        .or_else(|| {
            streams
                .iter()
                .filter(avc)
                .min_by_key(|s| json_u64(s, "/height").unwrap_or(u64::MAX))
        })
        .or_else(|| {
            streams
                .iter()
                .min_by_key(|s| json_u64(s, "/height").unwrap_or(u64::MAX))
        })
}

fn live_from_room(id: &str, root: &Value, headers: &HeaderMap) -> Result<Extracted, ExtractError> {
    let room = root.pointer("/data/room_info").unwrap_or(&Value::Null);
    let anchor = root
        .pointer("/data/anchor_info/base_info")
        .unwrap_or(&Value::Null);
    let cover = json_str(room, "/cover")
        .or_else(|| json_str(room, "/keyframe"))
        .and_then(parse_https_url)
        .ok_or_else(|| ExtractError::Failed("直播没有封面".into()))?;
    let uid = json_u64(room, "/uid");
    Ok(Extracted {
        extractor_id: "bilibili".into(),
        canonical_url: Url::parse(&format!("https://live.bilibili.com/{id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?,
        title: json_str(room, "/title").map(str::to_string),
        author: json_str(anchor, "/uname").map(str::to_string),
        author_url: uid.and_then(|n| Url::parse(&format!("https://space.bilibili.com/{n}")).ok()),
        description: json_str(room, "/description")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: Engagement::packed(json_u64(room, "/online"), None, None, None),
        items: vec![MediaItem {
            id: id.into(),
            kind: MediaKind::Photo,
            source: MediaSource::Direct {
                url: cover,
                headers: headers.clone(),
            },
            filename: Some(format!("bilibili-live-{id}.jpg")),
            cover: None,
        }],
    })
}

#[derive(Default)]
struct OpusProjection {
    title: String,
    text: String,
    pictures: Vec<String>,
}

fn dynamic_from_item(
    id: &str,
    item: &Value,
    headers: &HeaderMap,
) -> Result<Extracted, ExtractError> {
    let outer_dynamic = item
        .pointer("/modules/module_dynamic")
        .unwrap_or(&Value::Null);
    let outer_text = dynamic_text(outer_dynamic);
    let is_forward = json_str(item, "/type") == Some("DYNAMIC_TYPE_FORWARD")
        || item.get("orig").is_some_and(|v| !v.is_null());
    let source_item = item
        .get("orig")
        .filter(|v| !v.is_null())
        .unwrap_or(item);
    let source_dynamic = source_item
        .pointer("/modules/module_dynamic")
        .unwrap_or(&Value::Null);
    let source_major = source_dynamic.pointer("/major").unwrap_or(&Value::Null);
    let source_id = source_id(source_item, id);
    let title = major_title(source_major);
    let mut text = dynamic_text(source_dynamic);
    let pictures = pictures_from_dynamic(source_dynamic);
    if is_forward {
        let origin_author = author_name(source_item);
        let origin = if text.is_empty() {
            "原动态已删除或不可见".into()
        } else if origin_author.is_empty() {
            text.clone()
        } else {
            format!("转发自 {origin_author}：\n{text}")
        };
        text = if outer_text.is_empty() {
            origin
        } else {
            format!("{outer_text}\n\n{origin}")
        };
    }
    let mut items = photos_from_urls(source_id, &pictures, headers);
    if items.is_empty() {
        if let Some(cover) = major_cover(source_major).and_then(parse_https_url) {
            items.push(photo_item(source_id, 0, cover, headers));
        }
    }
    let author_mid = json_u64(item, "/modules/module_author/mid")
        .or_else(|| json_u64(item, "/modules/module_author/author/mid"))
        .or_else(|| json_u64(item, "/basic/uid"));
    Ok(Extracted {
        extractor_id: "bilibili".into(),
        canonical_url: Url::parse(&format!("https://www.bilibili.com/opus/{id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?,
        title: (!title.is_empty()).then_some(title),
        author: {
            let n = author_name(item);
            (!n.is_empty()).then_some(n)
        },
        author_url: author_mid
            .and_then(|n| Url::parse(&format!("https://space.bilibili.com/{n}")).ok()),
        description: (!text.is_empty()).then_some(text),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: Engagement::packed(
            json_u64(item, "/modules/module_stat/view/count"),
            json_u64(item, "/modules/module_stat/like/count"),
            json_u64(item, "/modules/module_stat/forward/count"),
            json_u64(item, "/modules/module_stat/comment/count"),
        ),
        items,
    })
}

fn apply_opus(extracted: &mut Extracted, opus: OpusProjection) {
    if !opus.title.is_empty() {
        extracted.title = Some(opus.title);
    }
    if !opus.text.is_empty() {
        extracted.description = Some(opus.text);
    }
    if !opus.pictures.is_empty() {
        if let Ok(headers) = media_headers(None) {
            let id = extracted.items.first().map(|i| i.id.as_str()).unwrap_or("opus");
            extracted.items = photos_from_urls(id, &opus.pictures, &headers);
        }
    }
}

fn opus_from_item(item: &Value) -> OpusProjection {
    let mut title = json_str(item, "/basic/title")
        .unwrap_or_default()
        .to_string();
    let mut paragraphs = Vec::new();
    let mut pictures = Vec::new();
    if let Some(modules) = item.get("modules").and_then(Value::as_array) {
        for module in modules {
            if title.is_empty() {
                title = json_str(module, "/module_title/text")
                    .unwrap_or_default()
                    .to_string();
            }
            let Some(content) = module.get("module_content") else {
                continue;
            };
            if let Some(items) = content.pointer("/paragraphs").and_then(Value::as_array) {
                for paragraph in items {
                    let line = opus_paragraph_text(paragraph);
                    if !line.is_empty() {
                        paragraphs.push(line);
                    }
                    if let Some(pics) = paragraph.pointer("/pic/pics").and_then(Value::as_array) {
                        for pic in pics {
                            if let Some(url) = json_str(pic, "/url")
                                .or_else(|| json_str(pic, "/src"))
                                .and_then(normalize_asset)
                            {
                                pictures.push(url);
                            }
                        }
                    }
                }
            }
        }
    }
    OpusProjection {
        title,
        text: paragraphs.join("\n\n"),
        pictures,
    }
}

fn audio_from_api(
    id: &str,
    song_root: &Value,
    stream: &Value,
    headers: &HeaderMap,
) -> Result<Extracted, ExtractError> {
    let song = song_root.get("data").unwrap_or(&Value::Null);
    let mut sources: Vec<Url> = stream
        .pointer("/data/cdns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(parse_https_url)
        .collect();
    if sources.is_empty() {
        return Err(ExtractError::Failed("没有可用的音频地址".into()));
    }
    let url = sources.remove(0);
    let cover = json_str(song, "/cover").and_then(parse_https_url);
    let uid = json_u64(song, "/uid");
    Ok(Extracted {
        extractor_id: "bilibili".into(),
        canonical_url: Url::parse(&format!("https://www.bilibili.com/audio/au{id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?,
        title: json_str(song, "/title").map(str::to_string),
        author: json_str(song, "/uname").map(str::to_string),
        author_url: uid.and_then(|n| Url::parse(&format!("https://space.bilibili.com/{n}")).ok()),
        description: json_str(song, "/intro")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: Engagement::packed(
            json_u64(song, "/passtime"),
            None,
            None,
            json_u64(song, "/comment"),
        ),
        items: vec![MediaItem {
            id: format!("au{id}"),
            kind: MediaKind::Audio,
            source: MediaSource::Direct {
                url,
                headers: headers.clone(),
            },
            filename: Some(format!("bilibili-au{id}.m4a")),
            cover,
        }],
    })
}

fn article_from_api(id: &str, root: &Value, headers: &HeaderMap) -> Result<Extracted, ExtractError> {
    let data = root.get("data").unwrap_or(&Value::Null);
    let cover = json_str(data, "/banner_url").and_then(parse_https_url);
    let mid = json_u64(data, "/mid");
    let items = match cover {
        Some(url) => vec![MediaItem {
            id: format!("cv{id}"),
            kind: MediaKind::Photo,
            source: MediaSource::Direct {
                url,
                headers: headers.clone(),
            },
            filename: Some(format!("bilibili-cv{id}.jpg")),
            cover: None,
        }],
        None => Vec::new(),
    };
    Ok(Extracted {
        extractor_id: "bilibili".into(),
        canonical_url: Url::parse(&format!("https://www.bilibili.com/read/cv{id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?,
        title: json_str(data, "/title").map(str::to_string),
        author: json_str(data, "/author_name").map(str::to_string),
        author_url: mid.and_then(|n| Url::parse(&format!("https://space.bilibili.com/{n}")).ok()),
        description: json_str(data, "/summary")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: Engagement::packed(
            json_u64(data, "/stats/view"),
            json_u64(data, "/stats/like"),
            None,
            json_u64(data, "/stats/reply"),
        ),
        items,
    })
}

struct BilibiliAuth {
    client: reqwest::Client,
    passport_base: String,
    api_base: String,
}

impl BilibiliAuth {
    fn new(endpoints: &BilibiliEndpoints) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(format!(
                "Mozilla/5.0 (CongmiaoFeedBot/{} QR Login)",
                env!("CARGO_PKG_VERSION")
            ))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            passport_base: trim_slash(&endpoints.passport_base),
            api_base: trim_slash(&endpoints.api_base),
        }
    }
}

#[async_trait]
impl AuthProvider for BilibiliAuth {
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
        let root: Value = self
            .client
            .get(format!(
                "{}/x/passport-login/web/qrcode/generate",
                self.passport_base
            ))
            .send()
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?
            .error_for_status()
            .map_err(|e| AuthError::Failed(e.to_string()))?
            .json()
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        if root.get("code").and_then(Value::as_i64).unwrap_or_default() != 0 {
            return Err(AuthError::Failed(
                json_str(&root, "/message")
                    .unwrap_or("Bilibili 登录失败")
                    .into(),
            ));
        }
        let key = json_str(&root, "/data/qrcode_key")
            .ok_or_else(|| AuthError::Failed("响应缺少 qrcode_key".into()))?;
        let url = json_str(&root, "/data/url")
            .ok_or_else(|| AuthError::Failed("响应缺少二维码地址".into()))?;
        session.method = Some(AuthMethod::QrCode);
        session.step = 0;
        session.blob = serde_json::json!({ "qrcode_key": key });
        Ok(AuthChallenge::ShowQr {
            png: render_qr_png(url)?,
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
                    .get("qrcode_key")
                    .and_then(Value::as_str)
                    .ok_or(AuthError::SessionExpired)?
                    .to_string();
                let response = self
                    .client
                    .get(format!(
                        "{}/x/passport-login/web/qrcode/poll",
                        self.passport_base
                    ))
                    .query(&[("qrcode_key", key.as_str())])
                    .send()
                    .await
                    .map_err(|e| AuthError::Failed(e.to_string()))?
                    .error_for_status()
                    .map_err(|e| AuthError::Failed(e.to_string()))?;
                let cookie = cookies_from_set_cookie(response.headers());
                let root: Value = response
                    .json()
                    .await
                    .map_err(|e| AuthError::Failed(e.to_string()))?;
                if root.get("code").and_then(Value::as_i64).unwrap_or_default() != 0 {
                    return Err(AuthError::Failed(
                        json_str(&root, "/message").unwrap_or("Bilibili 登录失败").into(),
                    ));
                }
                let code = root.pointer("/data/code").and_then(Value::as_i64);
                match bili_poll_code(code) {
                    BiliPoll::Waiting => Ok(AuthProgress::Pending),
                    BiliPoll::Scanned => {
                        if session.step == 0 {
                            session.step = 1;
                            Ok(AuthProgress::Hint("已扫码，请在客户端确认".into()))
                        } else {
                            Ok(AuthProgress::Pending)
                        }
                    }
                    BiliPoll::Expired => Err(AuthError::SessionExpired),
                    BiliPoll::Success => {
                        if cookie.is_empty() {
                            return Err(AuthError::Failed(
                                "Bilibili 登录成功但响应中没有 Cookie".into(),
                            ));
                        }
                        if !cookie_has_value(&cookie, "SESSDATA") {
                            return Err(AuthError::Failed("Cookie 缺少 SESSDATA".into()));
                        }
                        Ok(AuthProgress::Completed(Credential {
                            extractor_id: "bilibili".into(),
                            method: AuthMethod::QrCode,
                            payload: cookie.into_bytes(),
                            expires_at: None,
                        }))
                    }
                    BiliPoll::Other(code) => Err(AuthError::Failed(format!(
                        "Bilibili 扫码状态 {code}: {}",
                        json_str(&root, "/data/message").unwrap_or("未知错误")
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
        if cookie.is_empty() || !cookie_has_value(cookie, "SESSDATA") {
            return Ok(CredentialStatus::Invalid);
        }
        let root: Value = self
            .client
            .get(format!("{}/x/web-interface/nav", self.api_base))
            .header(header::COOKIE, cookie)
            .header(header::USER_AGENT, BROWSER_UA)
            .send()
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?
            .json()
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        let code = root.get("code").and_then(Value::as_i64).unwrap_or(-1);
        let logged_in = root
            .pointer("/data/isLogin")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if code == 0 && logged_in {
            Ok(CredentialStatus::Valid)
        } else {
            Ok(CredentialStatus::Invalid)
        }
    }

    async fn refresh(&self, _credential: &Credential) -> Result<Option<Credential>, AuthError> {
        Ok(None)
    }
}

pub(crate) fn bili_poll_code(code: Option<i64>) -> BiliPoll {
    match code {
        Some(0) => BiliPoll::Success,
        Some(86038) => BiliPoll::Expired,
        Some(86090) => BiliPoll::Scanned,
        Some(86101) => BiliPoll::Waiting,
        Some(other) => BiliPoll::Other(other),
        None => BiliPoll::Other(-1),
    }
}

pub(crate) fn cookies_from_set_cookie(headers: &reqwest::header::HeaderMap) -> String {
    let pairs = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .filter(|(name, _)| !name.is_empty())
        .collect::<BTreeMap<_, _>>();
    join_pairs(&pairs)
}

fn map_bili_code(code: i64, value: Value) -> Result<Value, ExtractError> {
    if code == 0 {
        return Ok(value);
    }
    let message = json_str(&value, "/message").unwrap_or("未知错误");
    Err(match code {
        -101 => ExtractError::AuthRequired("bilibili".into()),
        -352 | 412 => ExtractError::Failed(format!(
            "Bilibili 风控校验失败（{message}），请稍后重试或使用 /login bili 更新登录状态"
        )),
        404 | 4101139 => ExtractError::Failed(format!("Bilibili: {message}")),
        _ => ExtractError::Failed(format!("Bilibili: {message}")),
    })
}

fn parse_target(url: &Url) -> Option<BilibiliTarget> {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    if host == "live.bilibili.com" {
        return segments
            .iter()
            .find(|s| s.chars().all(|c| c.is_ascii_digit()))
            .map(|s| BilibiliTarget::Live((*s).to_string()));
    }
    if host == "t.bilibili.com" {
        return segments
            .first()
            .filter(|id| id.chars().all(|c| c.is_ascii_digit()))
            .map(|id| BilibiliTarget::Dynamic((*id).to_string()));
    }
    if !is_bilibili_host(&host) {
        return None;
    }
    for (index, segment) in segments.iter().enumerate() {
        let next = segments.get(index + 1).copied();
        match segment.to_ascii_lowercase().as_str() {
            "opus" | "dynamic"
                if next.is_some_and(|id| id.chars().all(|c| c.is_ascii_digit())) =>
            {
                return Some(BilibiliTarget::Dynamic(next.unwrap().to_string()));
            }
            "video" if next.is_some_and(|id| is_bvid(id) || is_av(id)) => {
                return Some(BilibiliTarget::Video(next.unwrap().to_string()));
            }
            "play" if next.is_some_and(is_ep_ss) => {
                return Some(BilibiliTarget::Bangumi(next.unwrap().to_string()));
            }
            "audio" if next.is_some_and(is_au) => {
                return Some(BilibiliTarget::Audio(next.unwrap().to_string()));
            }
            "read"
                if next == Some("mobile")
                    && segments
                        .get(index + 2)
                        .is_some_and(|id| id.chars().all(|c| c.is_ascii_digit())) =>
            {
                return Some(BilibiliTarget::Article(segments[index + 2].to_string()));
            }
            "read" if next == Some("mobile") => {
                let id = url
                    .query_pairs()
                    .find(|(n, _)| n == "id")
                    .map(|(_, v)| v.into_owned())?;
                return Some(BilibiliTarget::Article(id));
            }
            "read" if next.is_some_and(is_cv) => {
                return Some(BilibiliTarget::Article(next.unwrap().to_string()));
            }
            _ => {}
        }
    }
    None
}

fn is_bilibili_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "bilibili.com" || host.ends_with(".bilibili.com")
}

fn is_short_bili_host(host: &str) -> bool {
    matches!(host.to_ascii_lowercase().as_str(), "b23.tv" | "b23.wtf")
}

fn is_bvid(s: &str) -> bool {
    s.len() == 12
        && s[..2].eq_ignore_ascii_case("BV")
        && s[2..].chars().all(|c| c.is_ascii_alphanumeric())
}

fn is_av(s: &str) -> bool {
    s.len() > 2 && s[..2].eq_ignore_ascii_case("av") && s[2..].chars().all(|c| c.is_ascii_digit())
}

fn is_ep_ss(s: &str) -> bool {
    s.len() > 2
        && (s[..2].eq_ignore_ascii_case("ep") || s[..2].eq_ignore_ascii_case("ss"))
        && s[2..].chars().all(|c| c.is_ascii_digit())
}

fn is_au(s: &str) -> bool {
    s.len() > 2 && s[..2].eq_ignore_ascii_case("au") && s[2..].chars().all(|c| c.is_ascii_digit())
}

fn is_cv(s: &str) -> bool {
    s.len() > 2 && s[..2].eq_ignore_ascii_case("cv") && s[2..].chars().all(|c| c.is_ascii_digit())
}

fn av_digits(s: &str) -> Option<String> {
    if is_av(s) {
        Some(s[2..].to_string())
    } else if s.chars().all(|c| c.is_ascii_digit()) {
        Some(s.to_string())
    } else {
        None
    }
}

async fn expand_if_short(ctx: &ExtractCtx, url: &Url) -> Result<Url, ExtractError> {
    if is_short_bili_host(url.host_str().unwrap_or("")) {
        expand_shortlink(ctx.http.as_ref(), url).await
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

pub(crate) fn merge_cookies(account: Option<&str>, device: Option<&str>) -> Option<String> {
    let mut pairs = parse_cookie_pairs(account.unwrap_or(""));
    for (k, v) in parse_cookie_pairs(device.unwrap_or("")) {
        pairs.entry(k).or_insert(v);
    }
    if pairs.is_empty() {
        None
    } else {
        Some(join_pairs(&pairs))
    }
}

fn parse_cookie_pairs(cookie: &str) -> BTreeMap<String, String> {
    cookie
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

fn join_pairs(pairs: &BTreeMap<String, String>) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn cookie_has_value(cookie: &str, name: &str) -> bool {
    parse_cookie_pairs(cookie)
        .get(name)
        .is_some_and(|v| !v.is_empty())
}

fn media_headers(cookie: Option<&str>) -> Result<HeaderMap, ExtractError> {
    browser_headers(cookie)
}

fn browser_headers(cookie: Option<&str>) -> Result<HeaderMap, ExtractError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static(BROWSER_UA),
    );
    headers.insert(header::ACCEPT, HeaderValue::from_static(ACCEPT));
    headers.insert(
        header::ACCEPT_LANGUAGE,
        HeaderValue::from_static(ACCEPT_LANGUAGE),
    );
    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://www.bilibili.com"),
    );
    headers.insert(
        header::REFERER,
        HeaderValue::from_static("https://www.bilibili.com/"),
    );
    if let Some(c) = cookie.filter(|s| !s.is_empty()) {
        let value = HeaderValue::from_str(c)
            .map_err(|_| ExtractError::Failed("cookie 含有非法字符".into()))?;
        headers.insert(header::COOKIE, value);
    }
    Ok(headers)
}

fn build_url(base: &str, path: &str, params: &[(String, String)]) -> Result<Url, ExtractError> {
    let mut url = Url::parse(&format!("{base}{path}")).map_err(|e| ExtractError::Failed(e.to_string()))?;
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    Ok(url)
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

fn parse_https_url(s: &str) -> Option<Url> {
    Url::parse(&normalize_asset(s)?).ok()
}

fn normalize_asset(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with("//") {
        return Some(format!("https:{value}"));
    }
    if let Some(stripped) = value.strip_prefix("http://") {
        return Some(format!("https://{stripped}"));
    }
    value.starts_with("https://").then(|| value.to_string())
}

fn photos_from_urls(id: &str, urls: &[String], headers: &HeaderMap) -> Vec<MediaItem> {
    let mut seen = HashSet::new();
    urls.iter()
        .filter_map(|u| parse_https_url(u))
        .filter(|u| seen.insert(u.as_str().to_string()))
        .enumerate()
        .map(|(i, url)| photo_item(id, i, url, headers))
        .collect()
}

fn photo_item(id: &str, index: usize, url: Url, headers: &HeaderMap) -> MediaItem {
    MediaItem {
        id: format!("{id}_{index}"),
        kind: MediaKind::Photo,
        source: MediaSource::Direct {
            url,
            headers: headers.clone(),
        },
        filename: Some(format!("bilibili-dynamic-{id}-{index}.jpg")),
        cover: None,
    }
}

fn needs_opus(item: &Value) -> bool {
    json_str(
        item.pointer("/modules/module_dynamic/major").unwrap_or(&Value::Null),
        "/type",
    ) == Some("MAJOR_TYPE_OPUS")
        || json_str(
            item.pointer("/orig/modules/module_dynamic/major")
                .unwrap_or(&Value::Null),
            "/type",
        ) == Some("MAJOR_TYPE_OPUS")
}

fn source_id<'a>(item: &'a Value, fallback: &'a str) -> &'a str {
    json_str(item, "/id_str")
        .or_else(|| json_str(item, "/basic/dyn_id_str"))
        .unwrap_or(fallback)
}

fn author_name(item: &Value) -> String {
    json_str(item, "/modules/module_author/name")
        .or_else(|| json_str(item, "/modules/module_author/uname"))
        .or_else(|| json_str(item, "/modules/module_author/author/name"))
        .unwrap_or_default()
        .to_string()
}

fn dynamic_text(dynamic: &Value) -> String {
    let text = json_str(dynamic, "/desc/text")
        .or_else(|| json_str(dynamic, "/major/opus/summary/text"))
        .unwrap_or_default();
    if !text.is_empty() {
        return text.to_string();
    }
    dynamic
        .pointer("/desc/rich_text_nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|node| {
                    json_str(node, "/text")
                        .or_else(|| json_str(node, "/orig_text"))
                        .or_else(|| json_str(node, "/emoji/text"))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn major_title(major: &Value) -> String {
    [
        "/opus/title",
        "/archive/title",
        "/pgc/title",
        "/article/title",
        "/music/title",
        "/live/title",
        "/common/title",
        "/title",
    ]
    .into_iter()
    .find_map(|p| json_str(major, p).filter(|s| !s.is_empty()))
    .unwrap_or_default()
    .to_string()
}

fn major_cover(major: &Value) -> Option<&str> {
    [
        "/archive/cover",
        "/pgc/cover",
        "/article/cover",
        "/article/covers/0",
        "/music/cover",
        "/live/cover",
        "/common/cover",
        "/cover",
    ]
    .into_iter()
    .find_map(|p| json_str(major, p))
}

fn pictures_from_dynamic(dynamic: &Value) -> Vec<String> {
    dynamic
        .pointer("/major/opus/pics")
        .or_else(|| dynamic.pointer("/major/draw/items"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| {
                    json_str(v, "/url")
                        .or_else(|| json_str(v, "/src"))
                        .or_else(|| json_str(v, "/live_url"))
                        .and_then(normalize_asset)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn embedded_url(major: &Value) -> Option<String> {
    let archive = major.get("archive").unwrap_or(&Value::Null);
    if let Some(bvid) = json_str(archive, "/bvid") {
        return Some(format!("https://www.bilibili.com/video/{bvid}"));
    }
    if let Some(aid) = json_u64(archive, "/aid") {
        return Some(format!("https://www.bilibili.com/video/av{aid}"));
    }
    for pointer in [
        "/archive/jump_url",
        "/pgc/jump_url",
        "/article/jump_url",
        "/music/jump_url",
        "/live/jump_url",
        "/common/jump_url",
    ] {
        if let Some(url) = json_str(major, pointer).and_then(normalize_asset) {
            return Some(url);
        }
    }
    if let Some(auid) = json_u64(major, "/music/id") {
        return Some(format!("https://www.bilibili.com/audio/au{auid}"));
    }
    if let Some(article_id) = json_u64(major, "/article/id") {
        return Some(format!("https://www.bilibili.com/read/cv{article_id}"));
    }
    if let Some(room_id) = json_u64(major, "/live/room_id").or_else(|| json_u64(major, "/live/id")) {
        return Some(format!("https://live.bilibili.com/{room_id}"));
    }
    None
}

fn opus_paragraph_text(paragraph: &Value) -> String {
    if let Some(nodes) = paragraph.pointer("/text/nodes").and_then(Value::as_array) {
        let text: String = nodes
            .iter()
            .filter_map(|node| {
                json_str(node, "/word/words")
                    .or_else(|| json_str(node, "/rich/text"))
                    .or_else(|| json_str(node, "/rich/orig_text"))
            })
            .collect();
        if !text.is_empty() {
            return text;
        }
    }
    json_str(paragraph, "/text/text")
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use feedbot_core::options::{merge_layers, OptionValues};

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn poll_codes_map() {
        assert_eq!(bili_poll_code(Some(86101)), BiliPoll::Waiting);
        assert_eq!(bili_poll_code(Some(86090)), BiliPoll::Scanned);
        assert_eq!(bili_poll_code(Some(86038)), BiliPoll::Expired);
        assert_eq!(bili_poll_code(Some(0)), BiliPoll::Success);
        assert!(matches!(bili_poll_code(Some(1)), BiliPoll::Other(1)));
    }

    #[test]
    fn set_cookie_join_keeps_session() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_static("SESSDATA=abc; Path=/; HttpOnly"),
        );
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_static("bili_jct=tok; Path=/"),
        );
        let cookie = cookies_from_set_cookie(&headers);
        assert!(cookie.contains("SESSDATA=abc"));
        assert!(cookie.contains("bili_jct=tok"));
    }

    #[test]
    fn merge_does_not_overwrite_account() {
        let merged = merge_cookies(Some("SESSDATA=a; buvid3=old"), Some("buvid3=new; buvid4=x"));
        let s = merged.unwrap();
        assert!(s.contains("SESSDATA=a"));
        assert!(s.contains("buvid3=old"));
        assert!(s.contains("buvid4=x"));
        assert!(!s.contains("buvid3=new"));
    }

    #[test]
    fn url_hit_and_reject() {
        let ext = BilibiliExtractor::default();
        assert!(ext.matches(&url("https://www.bilibili.com/video/BV1xx411c7mD")));
        assert!(ext.matches(&url("https://bilibili.com/bangumi/play/ep123")));
        assert!(ext.matches(&url("https://live.bilibili.com/12345")));
        assert!(ext.matches(&url("https://t.bilibili.com/987")));
        assert!(ext.matches(&url("https://bilibili.com/opus/987")));
        assert!(ext.matches(&url("https://bilibili.com/audio/au1")));
        assert!(ext.matches(&url("https://bilibili.com/read/cv2")));
        assert!(ext.matches(&url("https://b23.tv/abcdef")));
        assert!(!ext.matches(&url("https://bilibili.com/")));
        assert!(!ext.matches(&url("https://space.bilibili.com/1")));
        let reg = crate::ExtractorRegistry::builtin();
        for name in ["bilibili", "bili", "b站"] {
            assert_eq!(reg.resolve_name(name).unwrap().id(), "bilibili");
        }
        assert_eq!(
            parse_target(&url("https://www.bilibili.com/video/BV1xx411c7mD")),
            Some(BilibiliTarget::Video("BV1xx411c7mD".into()))
        );
        assert_eq!(
            parse_target(&url("https://www.bilibili.com/video/av170001")),
            Some(BilibiliTarget::Video("av170001".into()))
        );
    }

    #[test]
    fn video_fixture_is_separated() {
        let view = serde_json::json!({
            "bvid": "BV1xx411c7mD",
            "cid": 1,
            "title": "t",
            "desc": "d",
            "pic": "https://i0.hdslb.com/cover.jpg",
            "owner": { "mid": 2, "name": "up" },
            "stat": { "view": 10, "like": 3, "share": 1, "reply": 2 }
        });
        let play = serde_json::json!({
            "dash": {
                "video": [{
                    "baseUrl": "https://upos.example/v.m4s",
                    "height": 1080,
                    "codecid": 7,
                    "codecs": "avc1"
                }],
                "audio": [{ "baseUrl": "https://upos.example/a.m4s" }]
            }
        });
        let extracted = video_from_view_play("BV1xx411c7mD", &view, &play, 1080, None).unwrap();
        assert_eq!(extracted.title.as_deref(), Some("t"));
        assert_eq!(extracted.engagement.as_ref().unwrap().views, Some(10));
        assert!(matches!(
            extracted.items[0].source,
            MediaSource::Separated { .. }
        ));
        assert!(extracted.engagement.as_ref().unwrap().likes.is_some());
    }

    #[test]
    fn dynamic_fixture_is_photo() {
        let item = serde_json::json!({
            "id_str": "99",
            "modules": {
                "module_author": { "name": "up", "mid": 1 },
                "module_stat": { "like": { "count": 4 } },
                "module_dynamic": {
                    "desc": { "text": "hello" },
                    "major": {
                        "draw": { "items": [{ "src": "https://i0.hdslb.com/p.jpg" }] }
                    }
                }
            }
        });
        let headers = HeaderMap::new();
        let extracted = dynamic_from_item("99", &item, &headers).unwrap();
        assert_eq!(extracted.items[0].kind, MediaKind::Photo);
        assert_eq!(extracted.description.as_deref(), Some("hello"));
        assert_eq!(extracted.engagement.as_ref().unwrap().likes, Some(4));
        assert!(extracted.engagement.as_ref().unwrap().views.is_none());
    }

    #[test]
    fn options_default_1080() {
        let opts = merge_layers(BilibiliExtractor::default().options(), &OptionValues::new(), &[]).unwrap();
        assert_eq!(quality_height(&opts), 1080);
    }
}
