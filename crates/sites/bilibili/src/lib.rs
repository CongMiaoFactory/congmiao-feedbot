//! Bilibili QR login and media extract.
//!
//! `api` holds request plumbing and the per-target extract methods; the other
//! modules map API JSON into `Extracted` (`video`, `dynamic`, `content`), parse
//! links (`links`), poll space subscriptions (`feed`, signed by `wbi`), and
//! implement login (`auth`).

mod api;
mod auth;
mod content;
mod dynamic;
mod feed;
mod links;
mod video;
mod wbi;

use auth::{BilibiliAuth, cookie_from_req};
use feed::parse_space;
use links::{expand_if_short, is_bilibili_host, is_short_bili_host, parse_target, rewrite};

use async_trait::async_trait;
use feedbot_core::auth::AuthProvider;
use feedbot_core::extract::{
    ExtractCtx, ExtractError, ExtractRequest, Extracted, Extractor, FeedEntry, HttpClient,
};
use feedbot_core::options::{OptionDef, OptionDefault, OptionKind, OptionSpec};
use feedbot_core::{Present, SiteMeta};
use std::sync::{Arc, Mutex};
use url::Url;

use feedbot_site_common::trim_slash;

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

pub struct BilibiliExtractor {
    auth: BilibiliAuth,
    api_base: String,
    live_api_base: String,
    www_base: String,
    device_cookie: Mutex<Option<String>>,
    wbi_key: Mutex<Option<wbi::WbiKey>>,
    /// Largest file the bot can send; video quality is picked to fit it.
    max_upload_bytes: Option<u64>,
}

impl BilibiliExtractor {
    pub fn new(
        endpoints: BilibiliEndpoints,
        http: Arc<dyn HttpClient>,
        max_upload_bytes: Option<u64>,
    ) -> Self {
        let auth = BilibiliAuth::new(&endpoints, http);
        Self {
            auth,
            api_base: trim_slash(&endpoints.api_base),
            live_api_base: trim_slash(&endpoints.live_api_base),
            www_base: "https://www.bilibili.com".into(),
            device_cookie: Mutex::new(None),
            wbi_key: Mutex::new(None),
            max_upload_bytes,
        }
    }
}

const META: SiteMeta = SiteMeta {
    id: "bilibili",
    name: "Bilibili",
    aliases: &["bili", "b站"],
    priority: 50,
    kinds: &["视频", "番剧", "直播封面", "动态", "音频", "专栏"],
    examples: &[
        "/video/BV...",
        "/bangumi/play/ep...",
        "live.bilibili.com/{room}",
        "/opus/{id}",
        "/audio/au...",
        "/read/cv...",
    ],
    feed_examples: &["space.bilibili.com/{uid}", "space.bilibili.com/{uid}/video"],
};

#[async_trait]
impl Extractor for BilibiliExtractor {
    fn meta(&self) -> SiteMeta {
        META
    }
    fn matches(&self, url: &Url) -> bool {
        (is_bilibili_host(url.host_str().unwrap_or("")) && parse_target(url).is_some())
            || is_short_bili_host(url.host_str().unwrap_or(""))
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
        parse_space(url).is_some()
    }

    async fn feed(
        &self,
        ctx: &ExtractCtx,
        req: &ExtractRequest,
    ) -> Result<Vec<FeedEntry>, ExtractError> {
        let source = parse_space(&req.url).ok_or(ExtractError::Unsupported)?;
        let cookie = cookie_from_req(req);
        self.space_feed(ctx, &source, cookie.as_deref()).await
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

#[cfg(test)]
mod tests {
    use super::auth::{BiliPoll, bili_poll_code, cookies_from_set_cookie, merge_cookies};
    use super::dynamic::dynamic_from_item;
    use super::links::parse_target;
    use super::video::{quality_height, video_from_view_play};
    use super::*;
    use feedbot_core::extract::{MediaKind, MediaSource};
    use feedbot_core::options::{OptionValues, merge_layers};
    use http::{HeaderMap, HeaderValue, header};

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
        let mut headers = http::HeaderMap::new();
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
        let ext = BilibiliExtractor::new(
            BilibiliEndpoints::default(),
            feedbot_site_common::testing::no_http(),
            None,
        );
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
        assert!(ext.feed_matches(&url("https://space.bilibili.com/1")));
        assert!(!ext.feed_matches(&url("https://www.bilibili.com/video/BV1xx411c7mD")));
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
        let extracted =
            video_from_view_play("BV1xx411c7mD", &view, &play, 1080, None, None).unwrap();
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
    fn stream_choice_fits_upload_limit() {
        use super::video::play_source;
        // 100 s; audio 128 kbps; 1080p 8 Mbps (~102 MB), 720p 3 Mbps (~39 MB).
        let play = serde_json::json!({
            "dash": {
                "duration": 100,
                "video": [
                    { "baseUrl": "https://upos.example/1080.m4s", "height": 1080, "codecid": 7, "bandwidth": 8_000_000 },
                    { "baseUrl": "https://upos.example/720.m4s", "height": 720, "codecid": 7, "bandwidth": 3_000_000 }
                ],
                "audio": [{ "baseUrl": "https://upos.example/a.m4s", "bandwidth": 128_000 }]
            }
        });
        let video_url = |max: Option<u64>| match play_source(&play, 1080, max) {
            Some(MediaSource::Separated { video, .. }) => video.to_string(),
            other => panic!("{other:?}"),
        };
        assert!(video_url(None).ends_with("1080.m4s"));
        assert!(video_url(Some(200_000_000)).ends_with("1080.m4s"));
        assert!(video_url(Some(50_000_000)).ends_with("720.m4s"));
        // Nothing fits: best within the height, transcoded later.
        assert!(video_url(Some(10_000_000)).ends_with("1080.m4s"));
    }

    #[test]
    fn options_default_1080() {
        let opts = merge_layers(
            BilibiliExtractor::new(
                BilibiliEndpoints::default(),
                feedbot_site_common::testing::no_http(),
                None,
            )
            .options(),
            &OptionValues::new(),
            &[],
        )
        .unwrap();
        assert_eq!(quality_height(&opts), 1080);
    }
}
