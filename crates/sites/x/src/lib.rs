//! X (Twitter) status extractor via syndication, with optional cookie login.
//!
//! Anonymous requests use the `syndication` endpoint; with a login cookie the
//! `graphql` endpoint is used. Both produce a `Tweet`, which `media` turns into
//! media items. `auth` validates and normalizes cookies. `timeline` polls
//! profile subscriptions (login required).

mod auth;
mod graphql;
mod media;
mod syndication;
mod timeline;

use auth::{XAuth, cookie_from_req};
use graphql::fetch_graphql_tweet;
use media::media_items;
use syndication::fetch_syndication;
use timeline::parse_profile;

use async_trait::async_trait;
use feedbot_core::auth::AuthProvider;
use feedbot_core::extract::{
    Engagement, ExtractCtx, ExtractError, ExtractRequest, Extracted, Extractor, FeedEntry,
    HttpClient, MediaKind,
};
use feedbot_core::options::{OptionDef, OptionDefault, OptionKind, OptionSpec, OptionValue};
use feedbot_core::{Present, SiteMeta};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use url::Url;

use feedbot_site_common::{rewrite_host, select_pages};

const SPEC: OptionSpec = OptionSpec {
    options: &[
        OptionDef {
            name: "q",
            aliases: &["quality"],
            kind: OptionKind::Enum {
                variants: &["orig", "large"],
            },
            default: Some(OptionDefault::Enum("orig")),
            affects_output: true,
            desc: "图片 orig 或 large。视频始终取最高码率 mp4",
        },
        OptionDef {
            name: "p",
            aliases: &["pages"],
            kind: OptionKind::IntRange { min: 1, max: 999 },
            default: None,
            affects_output: true,
            desc: "多图页码或区间，例如 +p:2、+p:1-3",
        },
        OptionDef {
            name: "sp",
            aliases: &["spoiler"],
            kind: OptionKind::Flag,
            default: None,
            affects_output: false,
            desc: "Telegram 遮罩。敏感内容默认开，全年龄用 +sp 打开，-sp 关闭",
        },
    ],
};

/// GraphQL query ids of the x.com web client (from gallery-dl's
/// `extractor/twitter.py`). They rotate; a 404 from X means they need updating.
#[derive(Debug, Clone)]
pub struct XQueryIds {
    pub tweet_result_by_rest_id: String,
    pub user_by_screen_name: String,
    pub user_media: String,
}

impl Default for XQueryIds {
    fn default() -> Self {
        Self {
            tweet_result_by_rest_id: "qxWQxcMLiTPcavz9Qy5hwQ".into(),
            user_by_screen_name: "ck5KkZ8t5cOmoLssopN99Q".into(),
            user_media: "jCRhbOzdgOHp6u9H4g2tEg".into(),
        }
    }
}

pub struct XExtractor {
    auth: XAuth,
    /// Lowercased screen name → user id, for profile subscriptions.
    user_ids: Mutex<HashMap<String, String>>,
}

impl XExtractor {
    pub fn new(http: Arc<dyn HttpClient>, query_ids: XQueryIds) -> Self {
        Self {
            auth: XAuth { http, query_ids },
            user_ids: Mutex::new(HashMap::new()),
        }
    }

    fn lock_user_ids(&self) -> MutexGuard<'_, HashMap<String, String>> {
        self.user_ids.lock().unwrap_or_else(|e| e.into_inner())
    }
}

const META: SiteMeta = SiteMeta {
    id: "x",
    name: "X",
    aliases: &["twitter", "tw"],
    priority: 70,
    kinds: &["图片", "视频", "动图 GIF"],
    examples: &["/status/{id}", "/{user}/status/{id}", "/i/web/status/{id}"],
    feed_examples: &["x.com/{user}", "x.com/{user}/media"],
};

#[async_trait]
impl Extractor for XExtractor {
    fn meta(&self) -> SiteMeta {
        META
    }
    fn matches(&self, url: &Url) -> bool {
        is_x_host(url) && parse_tweet_id(url).is_some()
    }
    fn rewrite_url(&self, url: &Url) -> Option<Url> {
        rewrite_host(
            url,
            &[
                "x.com",
                "mobile.twitter.com",
                "m.twitter.com",
                "www.twitter.com",
            ],
            "twitter.com",
        )
    }
    fn present(&self) -> Present {
        Present::Post { origin: "原帖" }
    }
    fn options(&self) -> &'static OptionSpec {
        &SPEC
    }
    fn auth(&self) -> Option<&dyn AuthProvider> {
        Some(&self.auth)
    }
    fn feed_matches(&self, url: &Url) -> bool {
        parse_profile(url).is_some()
    }
    fn feed_requires_login(&self) -> bool {
        true
    }

    async fn feed(
        &self,
        ctx: &ExtractCtx,
        req: &ExtractRequest,
    ) -> Result<Vec<FeedEntry>, ExtractError> {
        let screen_name = parse_profile(&req.url).ok_or(ExtractError::Unsupported)?;
        let cookie = cookie_from_req(req).ok_or_else(|| ExtractError::AuthRequired("x".into()))?;
        self.profile_feed(ctx, &screen_name, &cookie).await
    }

    async fn extract(
        &self,
        ctx: &ExtractCtx,
        req: &ExtractRequest,
    ) -> Result<Extracted, ExtractError> {
        let id = parse_tweet_id(&req.url).ok_or(ExtractError::Unsupported)?;
        let cookie = cookie_from_req(req);
        let has_cred = cookie.is_some();
        let orig = !matches!(req.options.get("q"), Some(OptionValue::Enum(q)) if q == "large");
        let page_range = match req.options.get("p") {
            Some(OptionValue::IntRange { start, end }) => Some((*start, *end)),
            _ => None,
        };

        let tweet = if has_cred {
            let query_id = &self.auth.query_ids.tweet_result_by_rest_id;
            match fetch_graphql_tweet(ctx, query_id, id, cookie.as_deref()).await {
                Ok(tweet) => tweet,
                Err(ExtractError::AuthRequired(_)) => {
                    return Err(ExtractError::AuthRequired("x".into()));
                }
                Err(_) => fetch_syndication(ctx, id).await?,
            }
        } else {
            fetch_syndication(ctx, id).await?
        };

        let items = media_items(&tweet, id, orig);
        let items = select_pages(items, page_range);
        if items.is_empty() {
            return Err(ExtractError::Failed("没有可发送的媒体".into()));
        }

        let canonical_url = Url::parse(&format!("https://x.com/i/status/{id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?;
        let screen = tweet.screen_name.as_deref().filter(|s| !s.is_empty());
        let author = tweet
            .name
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| screen.map(|s| format!("@{s}")));
        let author_url = screen.and_then(|s| Url::parse(&format!("https://x.com/{s}")).ok());
        let text = tweet.text.as_deref().map(str::trim).unwrap_or("");
        let title = title_from_text(text);
        let description = if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        };

        Ok(Extracted {
            extractor_id: self.id().into(),
            canonical_url,
            title,
            author,
            author_url,
            description,
            tags: Vec::new(),
            nsfw: tweet.nsfw,
            ai: false,
            engagement: tweet.engagement,
            items,
        })
    }
}

fn is_x_host(url: &Url) -> bool {
    matches!(
        url.host_str().unwrap_or(""),
        "twitter.com" | "www.twitter.com" | "x.com" | "www.x.com" | "mobile.twitter.com"
    )
}

fn parse_tweet_id(url: &Url) -> Option<u64> {
    let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    for pair in segs.windows(2) {
        if pair[0] == "status" {
            let digits = pair[1].split(|c: char| !c.is_ascii_digit()).next()?;
            return digits.parse().ok();
        }
    }
    None
}

struct Tweet {
    text: Option<String>,
    name: Option<String>,
    screen_name: Option<String>,
    nsfw: bool,
    engagement: Option<Engagement>,
    media: Vec<RawMedia>,
}

struct RawMedia {
    kind: MediaKind,
    url: String,
}

fn title_from_text(text: &str) -> Option<String> {
    let line = text.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    const MAX: usize = 80;
    if line.chars().count() <= MAX {
        Some(line.to_string())
    } else {
        Some(line.chars().take(MAX).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::auth::{cookie_value, normalize_cookie};
    use super::graphql::{RestTweet, graphql_auth_rejected, tweet_from_graphql};
    use super::media::{media_items, rewrite_photo_url};
    use super::syndication::{SyndicationTweet, syndication_token};
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn parse_status_urls() {
        assert_eq!(
            parse_tweet_id(&url("https://x.com/foo/status/1234567890")),
            Some(1234567890)
        );
        assert_eq!(
            parse_tweet_id(&url("https://twitter.com/i/web/status/99")),
            Some(99)
        );
        assert_eq!(
            parse_tweet_id(&url("https://x.com/i/status/42?s=20")),
            Some(42)
        );
        assert_eq!(
            parse_tweet_id(&url(
                "https://twitter.com/user/status/1110439106041929729/photo/1"
            )),
            Some(1110439106041929729)
        );
        assert_eq!(parse_tweet_id(&url("https://x.com/foo")), None);
        assert!(
            XExtractor::new(
                feedbot_site_common::testing::no_http(),
                XQueryIds::default()
            )
            .matches(&url("https://x.com/a/status/1"))
        );
        assert!(
            !XExtractor::new(
                feedbot_site_common::testing::no_http(),
                XQueryIds::default()
            )
            .matches(&url("https://x.com/foo"))
        );
    }

    #[test]
    fn syndication_token_matches_embed() {
        assert_eq!(syndication_token(1110439106041929729), "2owjpclbr6f");
    }

    #[test]
    fn photo_url_name_query_and_suffix() {
        let orig =
            rewrite_photo_url("https://pbs.twimg.com/media/CfwfpnJWwAEXwe3.jpg", true).unwrap();
        assert!(orig.as_str().ends_with(":orig"));
        let large = rewrite_photo_url(
            "https://pbs.twimg.com/media/abc?format=jpg&name=small",
            false,
        )
        .unwrap();
        assert!(large.as_str().contains("name=large"));
        assert!(large.as_str().contains("format=jpg"));
    }

    #[test]
    fn syndication_photo_fixture() {
        let body: SyndicationTweet = serde_json::from_value(serde_json::json!({
            "text": "hello\nworld",
            "possibly_sensitive": true,
            "user": { "name": "Eden", "screen_name": "edent" },
            "mediaDetails": [{
                "type": "photo",
                "media_url_https": "https://pbs.twimg.com/media/CfwfpnJWwAEXwe3.jpg"
            }]
        }))
        .unwrap();
        let tweet = body.into_tweet();
        assert!(tweet.nsfw);
        assert_eq!(tweet.media.len(), 1);
        assert_eq!(tweet.media[0].kind, MediaKind::Photo);
        let items = media_items(&tweet, 1, true);
        assert_eq!(items.len(), 1);
        assert_eq!(
            title_from_text(tweet.text.as_deref().unwrap()).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn video_picks_highest_mp4() {
        let body: SyndicationTweet = serde_json::from_value(serde_json::json!({
            "text": "vid",
            "mediaDetails": [{
                "type": "video",
                "video_info": {
                    "variants": [
                        { "content_type": "application/x-mpegURL", "url": "https://v.twimg.com/a.m3u8" },
                        { "bitrate": 256000, "content_type": "video/mp4", "url": "https://v.twimg.com/lo.mp4" },
                        { "bitrate": 2176000, "content_type": "video/mp4", "url": "https://v.twimg.com/hi.mp4" }
                    ]
                }
            }]
        }))
        .unwrap();
        let tweet = body.into_tweet();
        assert_eq!(tweet.media[0].kind, MediaKind::Video);
        assert_eq!(tweet.media[0].url, "https://v.twimg.com/hi.mp4");
    }

    #[test]
    fn gif_is_animation() {
        let body: SyndicationTweet = serde_json::from_value(serde_json::json!({
            "mediaDetails": [{
                "type": "animated_gif",
                "video_info": {
                    "variants": [
                        { "bitrate": 0, "content_type": "video/mp4", "url": "https://v.twimg.com/g.mp4" }
                    ]
                }
            }]
        }))
        .unwrap();
        let tweet = body.into_tweet();
        assert_eq!(tweet.media[0].kind, MediaKind::Animation);
    }

    #[test]
    fn rest_extended_entities() {
        let body: RestTweet = serde_json::from_value(serde_json::json!({
            "full_text": "pics",
            "possibly_sensitive": false,
            "user": { "name": "A", "screen_name": "a" },
            "extended_entities": {
                "media": [
                    { "type": "photo", "media_url_https": "https://pbs.twimg.com/media/a.jpg" },
                    { "type": "photo", "media_url_https": "https://pbs.twimg.com/media/b.jpg" },
                    { "type": "photo", "media_url_https": "https://pbs.twimg.com/media/c.jpg" }
                ]
            }
        }))
        .unwrap();
        let tweet = body.into_tweet();
        let items = select_pages(media_items(&tweet, 9, true), Some((2, 3)));
        assert_eq!(items.len(), 2);
        assert!(items[0].id.ends_with("_p1"));
    }

    #[test]
    fn tombstone_and_no_media() {
        let tomb: SyndicationTweet = serde_json::from_value(serde_json::json!({
            "__typename": "Tombstone"
        }))
        .unwrap();
        assert!(tomb.is_tombstone());
        let empty: SyndicationTweet = serde_json::from_value(serde_json::json!({
            "text": "no pics"
        }))
        .unwrap();
        assert!(empty.into_tweet().media.is_empty());
    }

    #[test]
    fn page_filter() {
        let items = vec!['a', 'b', 'c', 'd'];
        assert_eq!(select_pages(items.clone(), None), items);
        assert_eq!(select_pages(items.clone(), Some((2, 3))), vec!['b', 'c']);
        assert!(select_pages(items, Some((10, 12))).is_empty());
    }

    #[test]
    fn cookie_normalize() {
        assert_eq!(normalize_cookie(" abc123 "), "auth_token=abc123");
        assert_eq!(
            normalize_cookie("auth_token=aaa; ct0=bbb"),
            "auth_token=aaa; ct0=bbb"
        );
        assert_eq!(
            normalize_cookie("auth_token=aaa\nct0=bbb"),
            "auth_token=aaa; ct0=bbb"
        );
        assert_eq!(
            normalize_cookie("Cookie: auth_token=aaa; ct0=bbb"),
            "auth_token=aaa; ct0=bbb"
        );
        assert_eq!(normalize_cookie("aaa\nbbb"), "auth_token=aaa; ct0=bbb");
        assert_eq!(
            normalize_cookie("auth_token\taaa\t.x.com\nct0\tbbb\t.x.com"),
            "auth_token=aaa; ct0=bbb"
        );
        assert_eq!(cookie_value("auth_token=aaa; ct0=bbb", "ct0"), Some("bbb"));
    }

    #[test]
    fn graphql_tweet_fixture() {
        let json = serde_json::json!({
            "data": {
                "tweetResult": {
                    "result": {
                        "__typename": "Tweet",
                        "legacy": {
                            "full_text": "pics",
                            "possibly_sensitive": true,
                            "extended_entities": {
                                "media": [
                                    { "type": "photo", "media_url_https": "https://pbs.twimg.com/media/a.jpg" },
                                    { "type": "photo", "media_url_https": "https://pbs.twimg.com/media/b.jpg" }
                                ]
                            }
                        },
                        "core": {
                            "user_results": {
                                "result": {
                                    "legacy": { "name": "A", "screen_name": "a" }
                                }
                            }
                        }
                    }
                }
            }
        });
        let tweet = tweet_from_graphql(&json).unwrap();
        assert!(tweet.nsfw);
        assert_eq!(tweet.screen_name.as_deref(), Some("a"));
        assert_eq!(tweet.media.len(), 2);
        assert_eq!(tweet.media[0].kind, MediaKind::Photo);
        assert!(tweet.engagement.is_none());
    }

    #[test]
    fn graphql_engagement_counts() {
        let json = serde_json::json!({
            "data": {
                "tweetResult": {
                    "result": {
                        "__typename": "Tweet",
                        "legacy": {
                            "full_text": "pics",
                            "favorite_count": 320,
                            "retweet_count": 18,
                            "reply_count": 41,
                            "extended_entities": {
                                "media": [
                                    { "type": "photo", "media_url_https": "https://pbs.twimg.com/media/a.jpg" }
                                ]
                            }
                        },
                        "views": { "count": "12345" }
                    }
                }
            }
        });
        let tweet = tweet_from_graphql(&json).unwrap();
        let e = tweet.engagement.expect("engagement");
        assert_eq!(e.views, Some(12345));
        assert_eq!(e.likes, Some(320));
        assert_eq!(e.shares, Some(18));
        assert_eq!(e.comments, Some(41));
    }

    #[test]
    fn syndication_engagement_camel_case_and_missing() {
        let full: SyndicationTweet = serde_json::from_value(serde_json::json!({
            "text": "hi",
            "favoriteCount": 320,
            "retweetCount": 18,
            "conversationCount": 41,
            "viewCount": 12345,
            "mediaDetails": [{
                "type": "photo",
                "media_url_https": "https://pbs.twimg.com/media/a.jpg"
            }]
        }))
        .unwrap();
        let e = full.into_tweet().engagement.expect("engagement");
        assert_eq!(e.views, Some(12345));
        assert_eq!(e.likes, Some(320));
        assert_eq!(e.shares, Some(18));
        assert_eq!(e.comments, Some(41));

        let likes_only: SyndicationTweet = serde_json::from_value(serde_json::json!({
            "text": "hi",
            "favoriteCount": 7,
            "mediaDetails": [{
                "type": "photo",
                "media_url_https": "https://pbs.twimg.com/media/a.jpg"
            }]
        }))
        .unwrap();
        let e = likes_only.into_tweet().engagement.expect("likes only");
        assert_eq!(e.views, None);
        assert_eq!(e.likes, Some(7));
        assert_eq!(e.shares, None);
        assert_eq!(e.comments, None);
    }

    #[test]
    fn graphql_auth_error_codes() {
        let json = serde_json::json!({
            "errors": [{ "code": 215, "message": "Bad Authentication data." }]
        });
        assert!(graphql_auth_rejected(&json));
        assert!(!graphql_auth_rejected(&serde_json::json!({ "data": {} })));
    }

    #[test]
    fn spoiler_option_unset_plus_minus() {
        use feedbot_core::options::{OptionValues, merge_layers};
        let spec = XExtractor::new(
            feedbot_site_common::testing::no_http(),
            XQueryIds::default(),
        )
        .options();
        assert!(spec.get("sp").is_some());
        let unset = merge_layers(spec, &OptionValues::new(), &[]).unwrap();
        assert!(unset.get("sp").is_none());
        let plus = merge_layers(spec, &OptionValues::new(), &["+sp".into()]).unwrap();
        assert!(plus.flag("sp"));
        let minus = merge_layers(spec, &OptionValues::new(), &["-sp".into()]).unwrap();
        assert_eq!(minus.get("sp"), Some(&OptionValue::Flag(false)));
    }

    mod flow {
        use super::*;
        use feedbot_core::extract::MediaSource;
        use feedbot_core::options::OptionValues;
        use feedbot_site_common::testing::{FakeHttp, request};
        use serde_json::json;

        const SYNDICATION: &str = "cdn.syndication.twimg.com/tweet-result";
        const URL: &str = "https://twitter.com/a/status/20";
        const COOKIE: &str = "auth_token=aaaaaaaaaa; ct0=bbbbbbbbbb";

        fn graphql_route() -> String {
            let id = XQueryIds::default().tweet_result_by_rest_id;
            format!("x.com/i/api/graphql/{id}/TweetResultByRestId")
        }

        fn syndication() -> serde_json::Value {
            json!({
                "text": "hello",
                "user": { "name": "Eden", "screen_name": "edent" },
                "mediaDetails": [{
                    "type": "photo",
                    "media_url_https": "https://pbs.twimg.com/media/s.jpg"
                }]
            })
        }

        fn graphql() -> serde_json::Value {
            json!({ "data": { "tweetResult": { "result": {
                "__typename": "Tweet",
                "legacy": { "full_text": "pics", "extended_entities": { "media": [
                    { "type": "photo", "media_url_https": "https://pbs.twimg.com/media/a.jpg" },
                    { "type": "photo", "media_url_https": "https://pbs.twimg.com/media/b.jpg" }
                ] } },
                "core": { "user_results": { "result": {
                    "legacy": { "name": "A", "screen_name": "a" }
                } } }
            } } } })
        }

        fn options(ext: &XExtractor, tokens: &[&str]) -> OptionValues {
            let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
            ext.resolve_options(&OptionValues::new(), &tokens).unwrap()
        }

        fn photo_url(extracted: &Extracted, idx: usize) -> String {
            match &extracted.items[idx].source {
                MediaSource::Direct { url, .. } => url.to_string(),
                other => panic!("{other:?}"),
            }
        }

        #[tokio::test]
        async fn anonymous_uses_syndication() {
            let http = Arc::new(FakeHttp::new().json(SYNDICATION, syndication()));
            let ext = XExtractor::new(http.clone(), Default::default());
            let req = request("x", URL, options(&ext, &[]), None);
            let extracted = ext.extract(&http.ctx(), &req).await.unwrap();
            assert_eq!(extracted.author.as_deref(), Some("Eden"));
            assert_eq!(
                extracted.canonical_url.as_str(),
                "https://x.com/i/status/20"
            );
            assert!(photo_url(&extracted, 0).starts_with("https://pbs.twimg.com/media/s"));
            let query = http.last(SYNDICATION).url.query().unwrap().to_string();
            assert!(query.starts_with("id=20&"), "{query}");
        }

        #[tokio::test]
        async fn logged_in_uses_graphql_with_page_range() {
            let http = Arc::new(FakeHttp::new().json(&graphql_route(), graphql()));
            let ext = XExtractor::new(http.clone(), Default::default());
            let req = request("x", URL, options(&ext, &["+p:2"]), Some(COOKIE));
            let extracted = ext.extract(&http.ctx(), &req).await.unwrap();
            assert_eq!(extracted.items.len(), 1);
            assert!(photo_url(&extracted, 0).starts_with("https://pbs.twimg.com/media/b"));
            let sent = http.last(&graphql_route());
            assert_eq!(sent.headers["x-csrf-token"], "bbbbbbbbbb");
            assert_eq!(http.urls().len(), 1);
        }

        #[tokio::test]
        async fn graphql_failure_falls_back_unless_auth() {
            let http = Arc::new(
                FakeHttp::new()
                    .status(&graphql_route(), 500, "")
                    .json(SYNDICATION, syndication()),
            );
            let ext = XExtractor::new(http.clone(), Default::default());
            let req = request("x", URL, options(&ext, &[]), Some(COOKIE));
            let extracted = ext.extract(&http.ctx(), &req).await.unwrap();
            assert_eq!(extracted.author.as_deref(), Some("Eden"));

            let http = Arc::new(FakeHttp::new().status(&graphql_route(), 401, ""));
            let ext = XExtractor::new(http.clone(), Default::default());
            let err = ext.extract(&http.ctx(), &req).await.unwrap_err();
            assert!(matches!(err, ExtractError::AuthRequired(_)), "{err:?}");
        }
    }
}
