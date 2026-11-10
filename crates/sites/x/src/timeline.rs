//! Logged-in profile subscriptions: `x.com/{user}` and `x.com/{user}/media`
//! poll the user's own media posts through the GraphQL `UserMedia` timeline.

use super::XExtractor;
use super::auth::{GQL_USER_FEATURES, GQL_USER_TOGGLES};
use super::graphql::{
    GQL_TWEET_FEATURES, api_headers, graphql_json, graphql_url, unwrap_visibility,
};
use super::is_x_host;
use feedbot_core::extract::{ExtractCtx, ExtractError, FeedEntry, HttpRequest};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use url::Url;

use feedbot_site_common::{is_digits, json_str, parse_url};

/// `UserMedia` takes the tweet feature set plus one timeline flag.
fn timeline_features() -> String {
    format!(
        r#"{{"rweb_video_screen_enabled":false,{}"#,
        &GQL_TWEET_FEATURES[1..]
    )
}
const GQL_MEDIA_TOGGLES: &str = r#"{"withArticlePlainText":false}"#;
/// Entries per poll; one page is enough between two polls.
const MEDIA_PAGE: u32 = 20;
/// Bound on cached screen-name → user-id lookups.
const USER_ID_CACHE_MAX: usize = 1024;

/// First path segments on x.com that are not profiles.
const RESERVED: &[&str] = &[
    "home",
    "explore",
    "search",
    "i",
    "settings",
    "notifications",
    "messages",
    "compose",
    "hashtag",
    "intent",
    "share",
    "login",
    "logout",
    "signup",
    "tos",
    "privacy",
    "about",
    "download",
    "jobs",
    "account",
];

/// Screen name of a profile link: `/{user}` or `/{user}/media`.
pub(super) fn parse_profile(url: &Url) -> Option<String> {
    if !is_x_host(url) {
        return None;
    }
    let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let name = match segs.as_slice() {
        [name] => *name,
        [name, tab] if tab.eq_ignore_ascii_case("media") => *name,
        _ => return None,
    };
    let valid = (1..=15).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !RESERVED.contains(&name.to_ascii_lowercase().as_str());
    valid.then(|| name.to_string())
}

impl XExtractor {
    pub(super) async fn profile_feed(
        &self,
        ctx: &ExtractCtx,
        screen_name: &str,
        cookie: &str,
    ) -> Result<Vec<FeedEntry>, ExtractError> {
        let user_id = self.user_id(ctx, screen_name, cookie).await?;
        let variables = json!({
            "userId": user_id,
            "count": MEDIA_PAGE,
            "includePromotedContent": false,
            "withClientEventToken": false,
            "withBirdwatchNotes": false,
            "withVoice": true,
        })
        .to_string();
        let url = graphql_url(
            &self.auth.query_ids.user_media,
            "UserMedia",
            &variables,
            &timeline_features(),
            Some(GQL_MEDIA_TOGGLES),
        )?;
        let json = graphql_get(ctx, url, cookie).await?;
        media_entries(&json, &user_id, screen_name)
    }

    async fn user_id(
        &self,
        ctx: &ExtractCtx,
        screen_name: &str,
        cookie: &str,
    ) -> Result<String, ExtractError> {
        let key = screen_name.to_ascii_lowercase();
        if let Some(id) = self.lock_user_ids().get(&key) {
            return Ok(id.clone());
        }
        let variables =
            json!({ "screen_name": screen_name, "withGrokTranslatedBio": false }).to_string();
        let url = graphql_url(
            &self.auth.query_ids.user_by_screen_name,
            "UserByScreenName",
            &variables,
            GQL_USER_FEATURES,
            Some(GQL_USER_TOGGLES),
        )?;
        let json = graphql_get(ctx, url, cookie).await?;
        let id = user_id_from(&json)?;
        let mut cache = self.lock_user_ids();
        if cache.len() >= USER_ID_CACHE_MAX {
            cache.clear();
        }
        cache.insert(key, id.clone());
        Ok(id)
    }
}

async fn graphql_get(ctx: &ExtractCtx, url: Url, cookie: &str) -> Result<Value, ExtractError> {
    let headers = api_headers(Some(cookie))?;
    let resp = ctx
        .http
        .fetch(HttpRequest::get(url, headers).without_redirects())
        .await?;
    graphql_json(
        &resp,
        "X 接口已变更（GraphQL 404），请按 config/default.toml 的 [x] 说明更新 query id",
    )
}

fn user_id_from(json: &Value) -> Result<String, ExtractError> {
    let user = json
        .pointer("/data/user/result")
        .filter(|v| v.is_object())
        .ok_or_else(|| ExtractError::Failed("X 用户不存在".into()))?;
    if json_str(user, "/__typename") == Some("UserUnavailable") {
        let reason = json_str(user, "/reason").unwrap_or("unavailable");
        return Err(ExtractError::Failed(format!("X 用户不可用 ({reason})")));
    }
    json_str(user, "/rest_id")
        .filter(|id| is_digits(id))
        .map(str::to_string)
        .ok_or_else(|| ExtractError::Failed("X 用户不存在".into()))
}

/// Own, non-retweet posts with media, newest first. Every
/// `tweet_results.result` in the response is considered, so changes to the
/// timeline layout (grid modules, pinned entries) do not hide posts.
fn media_entries(
    json: &Value,
    user_id: &str,
    screen_name: &str,
) -> Result<Vec<FeedEntry>, ExtractError> {
    let instructions = json
        .pointer("/data/user/result/timeline/timeline/instructions")
        .or_else(|| json.pointer("/data/user/result/timeline_v2/timeline/instructions"))
        .ok_or_else(|| ExtractError::Failed("X 未返回时间线".into()))?;
    let mut tweets = Vec::new();
    collect_tweets(instructions, &mut tweets);
    let ids: BTreeSet<u64> = tweets
        .into_iter()
        .filter_map(|tweet| own_media_post(tweet, user_id))
        .collect();
    ids.into_iter()
        .rev()
        .map(|id| {
            let url = parse_url(&format!("https://x.com/{screen_name}/status/{id}"))?;
            Ok(FeedEntry {
                key: id.to_string(),
                url,
            })
        })
        .collect()
}

fn collect_tweets<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            if let Some(result) = map.get("tweet_results").and_then(|r| r.get("result")) {
                out.push(result);
                return;
            }
            for child in map.values() {
                collect_tweets(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_tweets(child, out);
            }
        }
        _ => {}
    }
}

fn own_media_post(tweet: &Value, user_id: &str) -> Option<u64> {
    let tweet = unwrap_visibility(tweet)?;
    let legacy = tweet.get("legacy")?;
    if legacy.get("retweeted_status_result").is_some() {
        return None;
    }
    let author = json_str(tweet, "/core/user_results/result/rest_id")
        .or_else(|| json_str(legacy, "/user_id_str"));
    if author.is_some_and(|a| a != user_id) {
        return None;
    }
    let has_media = legacy
        .pointer("/extended_entities/media")
        .or_else(|| legacy.pointer("/entities/media"))
        .and_then(Value::as_array)
        .is_some_and(|m| !m.is_empty());
    if !has_media {
        return None;
    }
    json_str(tweet, "/rest_id")
        .or_else(|| json_str(legacy, "/id_str"))?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn timeline_features_extend_tweet_features() {
        let features: Value = serde_json::from_str(&timeline_features()).unwrap();
        let tweet: Value = serde_json::from_str(GQL_TWEET_FEATURES).unwrap();
        assert_eq!(features["rweb_video_screen_enabled"], false);
        let extra = features.as_object().unwrap().len() - tweet.as_object().unwrap().len();
        assert_eq!(extra, 1);
    }

    #[test]
    fn profile_links() {
        let name = |s: &str| parse_profile(&url(s));
        assert_eq!(name("https://x.com/jack").as_deref(), Some("jack"));
        assert_eq!(
            name("https://twitter.com/Some_User/media").as_deref(),
            Some("Some_User")
        );
        assert_eq!(name("https://x.com/jack/status/20"), None);
        assert_eq!(name("https://x.com/jack/likes"), None);
        assert_eq!(name("https://x.com/home"), None);
        assert_eq!(name("https://x.com/i/bookmarks"), None);
        assert_eq!(name("https://x.com/"), None);
        assert_eq!(name("https://x.com/name-with-dash"), None);
        assert_eq!(name("https://x.com/abcdefghijklmnop"), None);
        assert_eq!(name("https://example.com/jack"), None);
    }

    fn tweet(id: &str, author: &str, media: bool) -> Value {
        let mut legacy = json!({ "id_str": id, "user_id_str": author });
        if media {
            legacy["extended_entities"] = json!({ "media": [{ "type": "photo" }] });
        }
        json!({
            "__typename": "Tweet",
            "rest_id": id,
            "core": { "user_results": { "result": { "rest_id": author } } },
            "legacy": legacy
        })
    }

    fn timeline(instructions: Value) -> Value {
        json!({ "data": { "user": { "result": { "timeline": { "timeline": {
            "instructions": instructions
        } } } } } })
    }

    #[test]
    fn media_timeline_entries() {
        let mut retweet = tweet("500", "1", true);
        retweet["legacy"]["retweeted_status_result"] = json!({ "result": tweet("400", "9", true) });
        let json = timeline(json!([
            { "type": "TimelinePinEntry", "entry": { "content": { "itemContent": {
                "tweet_results": { "result": tweet("100", "1", true) }
            } } } },
            { "type": "TimelineAddEntries", "entries": [
                { "entryId": "profile-grid-0", "content": { "items": [
                    { "item": { "itemContent": { "tweet_results": { "result": tweet("300", "1", true) } } } },
                    { "item": { "itemContent": { "tweet_results": { "result": {
                        "__typename": "TweetWithVisibilityResults",
                        "tweet": tweet("200", "1", true)
                    } } } } },
                    { "item": { "itemContent": { "tweet_results": { "result": tweet("250", "1", false) } } } },
                    { "item": { "itemContent": { "tweet_results": { "result": tweet("260", "2", true) } } } },
                    { "item": { "itemContent": { "tweet_results": { "result": retweet } } } }
                ] } },
                { "entryId": "cursor-bottom-1", "content": { "value": "abc" } }
            ] }
        ]));
        let entries = media_entries(&json, "1", "jack").unwrap();
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["300", "200", "100"]);
        assert_eq!(entries[0].url.as_str(), "https://x.com/jack/status/300");
    }

    #[test]
    fn timeline_errors() {
        assert!(media_entries(&json!({ "data": { "user": {} } }), "1", "jack").is_err());
        assert!(
            media_entries(&timeline(json!([])), "1", "jack")
                .unwrap()
                .is_empty()
        );
    }

    mod feed {
        use super::*;
        use feedbot_core::auth::{AuthMethod, Credential};
        use feedbot_core::extract::{
            ExtractRequest, Extractor, HttpClient, HttpResponse, RequesterInfo,
        };
        use feedbot_core::options::OptionValues;
        use feedbot_site_common::NoYtdlp;
        use http::HeaderMap;
        use std::sync::{Arc, Mutex};

        /// Answers `UserByScreenName` and `UserMedia`; records request paths.
        #[derive(Default)]
        struct FakeX {
            paths: Mutex<Vec<String>>,
        }

        #[async_trait::async_trait]
        impl HttpClient for FakeX {
            async fn fetch(&self, req: HttpRequest) -> Result<HttpResponse, ExtractError> {
                let path = req.url.path().to_string();
                assert!(req.headers.contains_key("x-csrf-token"));
                let body = if path.ends_with("/UserByScreenName") {
                    json!({ "data": { "user": { "result": { "rest_id": "1" } } } })
                } else {
                    timeline(json!([{ "type": "TimelineAddEntries", "entries": [
                        { "content": { "items": [
                            { "item": { "itemContent": { "tweet_results": {
                                "result": tweet("300", "1", true)
                            } } } }
                        ] } }
                    ] }]))
                };
                self.paths.lock().unwrap().push(path);
                Ok(HttpResponse {
                    status: 200,
                    headers: HeaderMap::new(),
                    body: body.to_string().into(),
                    final_url: req.url,
                })
            }
        }

        fn request(cookie: Option<&str>) -> ExtractRequest {
            ExtractRequest {
                url: url("https://twitter.com/jack/media"),
                options: OptionValues::new(),
                credential: cookie.map(|c| Credential {
                    extractor_id: "x".into(),
                    method: AuthMethod::Cookie,
                    payload: c.as_bytes().to_vec(),
                    expires_at: None,
                }),
                requester: RequesterInfo {
                    user_id: 1,
                    chat_id: 1,
                },
            }
        }

        #[tokio::test]
        async fn polls_with_cached_user_id() {
            let http = Arc::new(FakeX::default());
            let ctx = ExtractCtx::new(http.clone(), Arc::new(NoYtdlp));
            let ext = XExtractor::new(http.clone(), Default::default());
            let cookie = "auth_token=aaaaaaaaaa; ct0=bbbbbbbbbb";

            let err = ext.feed(&ctx, &request(None)).await.unwrap_err();
            assert!(matches!(err, ExtractError::AuthRequired(_)));

            for _ in 0..2 {
                let entries = ext.feed(&ctx, &request(Some(cookie))).await.unwrap();
                assert_eq!(entries[0].url.as_str(), "https://x.com/jack/status/300");
            }
            let paths = http.paths.lock().unwrap();
            let ops: Vec<&str> = paths
                .iter()
                .map(|p| p.rsplit('/').next().unwrap())
                .collect();
            assert_eq!(ops, ["UserByScreenName", "UserMedia", "UserMedia"]);
            assert!(ext.feed_requires_login());
        }
    }

    #[test]
    fn user_lookup() {
        let ok =
            json!({ "data": { "user": { "result": { "__typename": "User", "rest_id": "12" } } } });
        assert_eq!(user_id_from(&ok).unwrap(), "12");
        let gone = json!({ "data": { "user": { "result": {
            "__typename": "UserUnavailable", "reason": "Suspended"
        } } } });
        assert!(
            user_id_from(&gone)
                .unwrap_err()
                .to_string()
                .contains("Suspended")
        );
        assert!(user_id_from(&json!({ "data": {} })).is_err());
    }
}
