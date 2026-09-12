//! X (Twitter) status extractor via syndication, with optional cookie login.

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
use http::header::{COOKIE, ORIGIN, REFERER, USER_AGENT};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;
use url::Url;

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// Public web client bearer used by x.com itself (literal `%3D`, not decoded `=`).
const WEB_BEARER: &str = "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";

/// gallery-dl query ids; 404 on these is treated as “unverifiable”, not expired.
const GQL_USER_BY_SCREEN_NAME: &str = "ck5KkZ8t5cOmoLssopN99Q";
const GQL_TWEET_BY_REST_ID: &str = "qxWQxcMLiTPcavz9Qy5hwQ";
const GQL_USER_VARS: &str = r#"{"screen_name":"X","withGrokTranslatedBio":false}"#;
const GQL_USER_FEATURES: &str = r#"{"hidden_profile_subscriptions_enabled":true,"payments_enabled":false,"rweb_xchat_enabled":false,"profile_label_improvements_pcf_label_in_post_enabled":true,"rweb_tipjar_consumption_enabled":true,"verified_phone_label_enabled":false,"highlights_tweets_tab_ui_enabled":true,"responsive_web_twitter_article_notes_tab_enabled":true,"subscriptions_feature_can_gift_premium":true,"creator_subscriptions_tweet_preview_api_enabled":true,"responsive_web_graphql_skip_user_profile_image_extensions_enabled":false,"responsive_web_graphql_timeline_navigation_enabled":true,"subscriptions_verification_info_is_identity_verified_enabled":true,"subscriptions_verification_info_verified_since_enabled":true}"#;
const GQL_USER_TOGGLES: &str = r#"{"withAuxiliaryUserLabels":true}"#;
const GQL_TWEET_FEATURES: &str = r#"{"payments_enabled":false,"rweb_xchat_enabled":false,"profile_label_improvements_pcf_label_in_post_enabled":true,"rweb_tipjar_consumption_enabled":true,"verified_phone_label_enabled":false,"creator_subscriptions_tweet_preview_api_enabled":true,"responsive_web_graphql_timeline_navigation_enabled":true,"responsive_web_graphql_skip_user_profile_image_extensions_enabled":false,"premium_content_api_read_enabled":false,"communities_web_enable_tweet_community_results_fetch":true,"c9s_tweet_anatomy_moderator_badge_enabled":true,"responsive_web_grok_analyze_button_fetch_trends_enabled":false,"responsive_web_grok_analyze_post_followups_enabled":true,"responsive_web_jetfuel_frame":true,"responsive_web_grok_share_attachment_enabled":true,"articles_preview_enabled":true,"responsive_web_edit_tweet_api_enabled":true,"graphql_is_translatable_rweb_tweet_is_translatable_enabled":true,"view_counts_everywhere_api_enabled":true,"longform_notetweets_consumption_enabled":true,"responsive_web_twitter_article_tweet_consumption_enabled":true,"tweet_awards_web_tipping_enabled":false,"responsive_web_grok_show_grok_translated_post":false,"responsive_web_grok_analysis_button_from_backend":true,"creator_subscriptions_quote_tweet_preview_enabled":false,"freedom_of_speech_not_reach_fetch_enabled":true,"standardized_nudges_misinfo":true,"tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled":true,"longform_notetweets_rich_text_read_enabled":true,"longform_notetweets_inline_media_enabled":true,"responsive_web_grok_image_annotation_enabled":true,"responsive_web_grok_imagine_annotation_enabled":true,"responsive_web_grok_community_note_auto_translation_is_enabled":false,"responsive_web_enhance_cards_enabled":false}"#;
const GQL_TWEET_TOGGLES: &str = r#"{"withArticleRichContentState":true,"withArticlePlainText":false,"withGrokAnalyze":false,"withDisallowedReplyControls":false}"#;

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
            desc: "多图页码区间，例如 +p:1-3",
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

pub struct XExtractor;

static AUTH: XAuth = XAuth::new();

#[async_trait]
impl Extractor for XExtractor {
    fn id(&self) -> &'static str {
        "x"
    }
    fn display_name(&self) -> &'static str {
        "X"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["twitter", "tw"]
    }
    fn priority(&self) -> i32 {
        70
    }
    fn matches(&self, url: &Url) -> bool {
        is_x_host(url) && parse_tweet_id(url).is_some()
    }
    fn options(&self) -> &'static OptionSpec {
        &SPEC
    }
    fn auth(&self) -> Option<&dyn AuthProvider> {
        Some(&AUTH)
    }
    fn extract_kinds(&self) -> &'static [&'static str] {
        &["图片", "视频", "动图 GIF"]
    }
    fn url_examples(&self) -> &'static [&'static str] {
        &["/status/{id}", "/{user}/status/{id}", "/i/web/status/{id}"]
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
            match fetch_graphql_tweet(ctx, id, cookie.as_deref()).await {
                Ok(tweet) => tweet,
                Err(ExtractError::AuthRequired(_)) => {
                    return Err(ExtractError::AuthRequired("x".into()))
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

async fn fetch_syndication(ctx: &ExtractCtx, id: u64) -> Result<Tweet, ExtractError> {
    let token = syndication_token(id);
    let url = Url::parse(&format!(
        "https://cdn.syndication.twimg.com/tweet-result?id={id}&lang=zh&token={token}"
    ))
    .map_err(|e| ExtractError::Failed(e.to_string()))?;
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(BROWSER_UA));
    let resp = ctx
        .http
        .fetch(HttpRequest {
            url,
            headers,
            method: HttpMethod::Get,
        })
        .await?;
    match resp.status {
        404 => return Err(ExtractError::Failed("推文不存在或已删除".into())),
        s if s >= 400 => return Err(ExtractError::Network(format!("HTTP {s}"))),
        _ => {}
    }
    if resp.body.is_empty() {
        return Err(ExtractError::Failed("推文不存在或已删除".into()));
    }
    let body: SyndicationTweet =
        serde_json::from_slice(&resp.body).map_err(|e| ExtractError::Failed(e.to_string()))?;
    if body.is_tombstone() {
        return Err(ExtractError::Failed("推文不存在或已删除".into()));
    }
    Ok(body.into_tweet())
}

async fn fetch_graphql_tweet(
    ctx: &ExtractCtx,
    id: u64,
    cookie: Option<&str>,
) -> Result<Tweet, ExtractError> {
    let variables = format!(
        r#"{{"tweetId":"{id}","withCommunity":false,"includePromotedContent":false,"withVoice":false}}"#
    );
    let url = graphql_url(
        GQL_TWEET_BY_REST_ID,
        "TweetResultByRestId",
        &variables,
        GQL_TWEET_FEATURES,
        Some(GQL_TWEET_TOGGLES),
    )?;
    let headers = api_headers(cookie)?;
    let resp = ctx
        .http
        .fetch(HttpRequest {
            url,
            headers,
            method: HttpMethod::Get,
        })
        .await?;
    match resp.status {
        401 | 403 => return Err(ExtractError::AuthRequired("x".into())),
        404 => return Err(ExtractError::Failed("推文不存在或已删除".into())),
        s if s >= 400 => return Err(ExtractError::Network(format!("HTTP {s}"))),
        _ => {}
    }
    let json: Value =
        serde_json::from_slice(&resp.body).map_err(|e| ExtractError::Failed(e.to_string()))?;
    if graphql_auth_rejected(&json) {
        return Err(ExtractError::AuthRequired("x".into()));
    }
    tweet_from_graphql(&json)
}

fn media_items(tweet: &Tweet, id: u64, orig: bool) -> Vec<MediaItem> {
    tweet
        .media
        .iter()
        .enumerate()
        .filter_map(|(idx, raw)| {
            let url = match raw.kind {
                MediaKind::Photo => rewrite_photo_url(&raw.url, orig).ok()?,
                _ => Url::parse(&raw.url).ok()?,
            };
            let filename = filename_from_url(&url);
            Some(MediaItem {
                id: format!("{id}_p{idx}"),
                kind: raw.kind,
                source: MediaSource::Direct {
                    url,
                    headers: HeaderMap::new(),
                },
                filename,
                cover: None,
            })
        })
        .collect()
}

fn rewrite_photo_url(raw: &str, orig: bool) -> Result<Url, ExtractError> {
    let size = if orig { "orig" } else { "large" };
    let mut url = Url::parse(raw).map_err(|e| ExtractError::Failed(e.to_string()))?;
    strip_size_suffix(&mut url);
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if pairs.iter().any(|(k, _)| k == "name" || k == "format") {
        let mut next = Vec::new();
        let mut saw_name = false;
        for (k, v) in pairs {
            if k == "name" {
                next.push((k, size.to_string()));
                saw_name = true;
            } else {
                next.push((k, v));
            }
        }
        if !saw_name {
            next.push(("name".into(), size.to_string()));
        }
        let encoded = next
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        url.set_query(Some(&encoded));
    } else {
        let mut path = url.path().to_string();
        path.push(':');
        path.push_str(size);
        url.set_path(&path);
    }
    Ok(url)
}

fn strip_size_suffix(url: &mut Url) {
    let Some(last) = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .map(str::to_string)
    else {
        return;
    };
    if let Some((base, suffix)) = last.rsplit_once(':') {
        if matches!(
            suffix,
            "orig" | "large" | "medium" | "small" | "thumb" | "orig.jpg" | "large.jpg"
        ) || suffix.chars().all(|c| c.is_ascii_alphabetic())
        {
            let mut segs: Vec<String> = url.path_segments().unwrap().map(str::to_string).collect();
            if let Some(last) = segs.last_mut() {
                *last = base.to_string();
            }
            let path = format!("/{}", segs.join("/"));
            url.set_path(&path);
        }
    }
}

fn filename_from_url(url: &Url) -> Option<String> {
    url.path_segments()?
        .next_back()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(':').next().unwrap_or(s).to_string())
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

fn select_pages<T>(mut items: Vec<T>, range: Option<(i64, i64)>) -> Vec<T> {
    let Some((start, end)) = range else {
        return items;
    };
    if items.is_empty() || start > end {
        return Vec::new();
    }
    let start = usize::try_from(start.max(1)).unwrap_or(1);
    let end = usize::try_from(end.max(1)).unwrap_or(1);
    if start > items.len() {
        return Vec::new();
    }
    let end = end.min(items.len());
    items.drain(start - 1..end).collect()
}

fn pick_mp4(variants: &[VideoVariant]) -> Option<String> {
    variants
        .iter()
        .filter(|v| {
            v.content_type
                .as_deref()
                .is_some_and(|t| t.starts_with("video/mp4"))
                && v.url.as_deref().is_some_and(|u| !u.is_empty())
        })
        .max_by_key(|v| v.bitrate.unwrap_or(0))
        .and_then(|v| v.url.clone())
}

fn syndication_token(id: u64) -> String {
    let n = (id as f64) / 1e15 * std::f64::consts::PI;
    js_to_string_36(n)
        .chars()
        .filter(|c| *c != '0' && *c != '.')
        .collect()
}

fn js_to_string_36(value: f64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if !value.is_finite() {
        return String::new();
    }
    let negative = value.is_sign_negative() && value != 0.0;
    let value = value.abs();
    let integer = value.trunc();
    let mut frac = value - integer;
    let mut int_n = integer as u128;
    let mut int_digits = Vec::new();
    if int_n == 0 {
        int_digits.push(b'0');
    }
    while int_n > 0 {
        int_digits.push(DIGITS[(int_n % 36) as usize]);
        int_n /= 36;
    }
    int_digits.reverse();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(std::str::from_utf8(&int_digits).unwrap_or("0"));
    if frac > 0.0 {
        out.push('.');
        let mut digits = [0usize; 9];
        for slot in &mut digits {
            frac *= 36.0;
            let digit = (frac.trunc() as usize).min(35);
            *slot = digit;
            frac -= digit as f64;
        }
        if digits[8] >= 18 {
            let mut i = 7isize;
            while i >= 0 {
                digits[i as usize] += 1;
                if digits[i as usize] < 36 {
                    break;
                }
                digits[i as usize] = 0;
                i -= 1;
            }
        }
        for d in &digits[..8] {
            out.push(DIGITS[*d] as char);
        }
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            out.pop();
        }
    }
    out
}

fn cookie_from_req(req: &ExtractRequest) -> Option<String> {
    let raw = std::str::from_utf8(&req.credential.as_ref()?.payload).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(normalize_cookie(trimmed))
    }
}

fn normalize_cookie(raw: &str) -> String {
    let mut s = raw.trim();
    for prefix in ["Cookie:", "cookie:", "COOKIE:"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim();
            break;
        }
    }
    let s = s.trim_matches(|c| matches!(c, '"' | '\'' | '“' | '”'));
    if s.is_empty() {
        return String::new();
    }

    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut unlabeled: Vec<String> = Vec::new();
    for line in s.split(['\n', '\r', ';']) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains('\t') {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() >= 2 {
                let name = cols[0].trim();
                let value = strip_wrap_quotes(cols[1].trim());
                if looks_like_cookie_name(name) && !value.is_empty() {
                    upsert_pair(&mut pairs, name, value);
                    continue;
                }
            }
        }
        if let Some((k, v)) = split_cookie_kv(line) {
            upsert_pair(&mut pairs, k, v);
            continue;
        }
        for token in line.split_whitespace() {
            let token = strip_wrap_quotes(token);
            if !token.is_empty() {
                unlabeled.push(token.to_string());
            }
        }
    }

    if !has_pair(&pairs, "auth_token") {
        if let Some(v) = unlabeled.first() {
            pairs.insert(0, ("auth_token".into(), v.clone()));
            if unlabeled.len() >= 2 && !has_pair(&pairs, "ct0") {
                pairs.push(("ct0".into(), unlabeled[1].clone()));
            }
        }
    } else if !has_pair(&pairs, "ct0") {
        if let Some(v) = unlabeled.first() {
            pairs.push(("ct0".into(), v.clone()));
        }
    }

    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn strip_wrap_quotes(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, '"' | '\'' | '“' | '”'))
}

fn looks_like_cookie_name(k: &str) -> bool {
    !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn split_cookie_kv(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once('=').or_else(|| {
        line.split_once(':')
            .filter(|(k, _)| looks_like_cookie_name(k.trim()))
    })?;
    let k = k.trim();
    let v = strip_wrap_quotes(v.trim());
    if looks_like_cookie_name(k) && !v.is_empty() {
        Some((k, v))
    } else {
        None
    }
}

fn has_pair(pairs: &[(String, String)], name: &str) -> bool {
    pairs.iter().any(|(k, _)| k == name)
}

fn upsert_pair(pairs: &mut Vec<(String, String)>, k: &str, v: &str) {
    if let Some(existing) = pairs.iter_mut().find(|(ek, _)| ek == k) {
        existing.1 = v.to_string();
    } else {
        pairs.push((k.to_string(), v.to_string()));
    }
}

fn cookie_value<'a>(cookie: &'a str, name: &str) -> Option<&'a str> {
    for part in cookie.split(';') {
        let part = part.trim();
        let (k, v) = part.split_once('=')?;
        if k.trim() == name {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

fn graphql_url(
    query_id: &str,
    op: &str,
    variables: &str,
    features: &str,
    field_toggles: Option<&str>,
) -> Result<Url, ExtractError> {
    let mut url = Url::parse(&format!("https://x.com/i/api/graphql/{query_id}/{op}"))
        .map_err(|e| ExtractError::Failed(e.to_string()))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("variables", variables);
        q.append_pair("features", features);
        if let Some(toggles) = field_toggles {
            q.append_pair("fieldToggles", toggles);
        }
    }
    Ok(url)
}

fn graphql_auth_rejected(json: &Value) -> bool {
    let Some(errors) = json.get("errors").and_then(|e| e.as_array()) else {
        return false;
    };
    errors.iter().any(|err| {
        let code = err.get("code").and_then(Value::as_u64).unwrap_or(0);
        matches!(code, 32 | 89 | 135 | 215 | 226 | 326)
            || err.get("message").and_then(Value::as_str).is_some_and(|m| {
                let m = m.to_ascii_lowercase();
                m.contains("bad authentication")
                    || m.contains("not authorized")
                    || m.contains("could not authenticate")
                    || m.contains("unauthorized")
            })
    })
}

fn tweet_from_graphql(json: &Value) -> Result<Tweet, ExtractError> {
    let mut result = json
        .pointer("/data/tweetResult/result")
        .ok_or_else(|| ExtractError::Failed("推文不存在或已删除".into()))?;
    if result
        .get("__typename")
        .and_then(Value::as_str)
        .is_some_and(|t| t == "TweetWithVisibilityResults")
    {
        result = result
            .get("tweet")
            .ok_or_else(|| ExtractError::Failed("推文不存在或已删除".into()))?;
    }
    let typename = result
        .get("__typename")
        .and_then(Value::as_str)
        .unwrap_or("");
    if typename == "TweetUnavailable" {
        let reason = result.get("reason").and_then(Value::as_str).unwrap_or("");
        return match reason {
            "NsfwViewerHasNoStatedAge" | "NsfwLoggedOut" | "Protected" => {
                Err(ExtractError::AuthRequired("x".into()))
            }
            _ => Err(ExtractError::Failed(format!("推文不可用 ({reason})"))),
        };
    }
    let legacy = result.get("legacy").unwrap_or(result);
    let user = result.pointer("/core/user_results/result/legacy");
    let media_src = legacy
        .pointer("/extended_entities/media")
        .or_else(|| legacy.pointer("/entities/media"))
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let details: Vec<MediaDetail> = serde_json::from_value(media_src).unwrap_or_default();
    Ok(Tweet {
        text: legacy
            .get("full_text")
            .or_else(|| legacy.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string),
        name: user
            .and_then(|u| u.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        screen_name: user
            .and_then(|u| u.get("screen_name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        nsfw: legacy
            .get("possibly_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        engagement: Engagement::packed(
            json_u64(result.pointer("/views/count")),
            json_u64(legacy.get("favorite_count")),
            json_u64(legacy.get("retweet_count")),
            json_u64(legacy.get("reply_count")),
        ),
        media: details.iter().filter_map(MediaDetail::to_raw).collect(),
    })
}

fn json_u64(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    if let Some(n) = v.as_i64() {
        return u64::try_from(n).ok();
    }
    if let Some(n) = v.as_f64() {
        if n.is_finite() && n >= 0.0 {
            return Some(n.trunc() as u64);
        }
    }
    v.as_str()
        .and_then(|s| s.replace(',', "").trim().parse().ok())
}

fn api_headers(cookie: Option<&str>) -> Result<HeaderMap, ExtractError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(BROWSER_UA));
    headers.insert(ORIGIN, HeaderValue::from_static("https://x.com"));
    headers.insert(REFERER, HeaderValue::from_static("https://x.com/"));
    headers.insert(
        HeaderName::from_static("authorization"),
        HeaderValue::from_static(WEB_BEARER),
    );
    headers.insert(
        HeaderName::from_static("x-twitter-client-language"),
        HeaderValue::from_static("zh"),
    );
    if let Some(cookie) = cookie {
        let value = HeaderValue::from_str(cookie)
            .map_err(|_| ExtractError::Failed("cookie 含有非法字符".into()))?;
        headers.insert(COOKIE, value);
        if let Some(ct0) = cookie_value(cookie, "ct0") {
            let csrf = HeaderValue::from_str(ct0)
                .map_err(|_| ExtractError::Failed("ct0 含有非法字符".into()))?;
            headers.insert(HeaderName::from_static("x-csrf-token"), csrf);
            headers.insert(
                HeaderName::from_static("x-twitter-auth-type"),
                HeaderValue::from_static("OAuth2Session"),
            );
            headers.insert(
                HeaderName::from_static("x-twitter-active-user"),
                HeaderValue::from_static("yes"),
            );
        }
    }
    Ok(headers)
}

#[derive(Debug, Deserialize)]
struct SyndicationTweet {
    #[serde(default, rename = "__typename")]
    typename: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default, alias = "possiblySensitive")]
    possibly_sensitive: bool,
    #[serde(default)]
    user: Option<TweetUser>,
    #[serde(default, rename = "mediaDetails")]
    media_details: Vec<MediaDetail>,
    #[serde(default)]
    photos: Vec<PhotoItem>,
    #[serde(default)]
    video: Option<VideoBlock>,
    #[serde(default, alias = "favoriteCount")]
    favorite_count: Option<u64>,
    #[serde(default, alias = "retweetCount")]
    retweet_count: Option<u64>,
    #[serde(default, alias = "conversationCount", alias = "replyCount")]
    conversation_count: Option<u64>,
    #[serde(default, alias = "viewCount")]
    view_count: Option<u64>,
    #[serde(default)]
    views: Option<Value>,
}

impl SyndicationTweet {
    fn is_tombstone(&self) -> bool {
        self.typename
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("Tombstone"))
    }

    fn into_tweet(self) -> Tweet {
        let mut media = Vec::new();
        for detail in &self.media_details {
            if let Some(item) = detail.to_raw() {
                media.push(item);
            }
        }
        if media.is_empty() {
            for photo in &self.photos {
                if let Some(url) = photo.url.as_deref().filter(|s| !s.is_empty()) {
                    media.push(RawMedia {
                        kind: MediaKind::Photo,
                        url: url.to_string(),
                    });
                }
            }
        }
        if media.is_empty() {
            if let Some(url) = self.video.as_ref().and_then(|v| pick_mp4(&v.variants)) {
                media.push(RawMedia {
                    kind: MediaKind::Video,
                    url,
                });
            }
        }
        Tweet {
            text: self.text,
            name: self.user.as_ref().and_then(|u| u.name.clone()),
            screen_name: self.user.as_ref().and_then(|u| u.screen_name.clone()),
            nsfw: self.possibly_sensitive,
            engagement: Engagement::packed(
                self.view_count.or_else(|| {
                    json_u64(self.views.as_ref().and_then(|v| v.get("count")))
                        .or_else(|| json_u64(self.views.as_ref()))
                }),
                self.favorite_count,
                self.retweet_count,
                self.conversation_count,
            ),
            media,
        }
    }
}

#[derive(Debug, Deserialize)]
struct TweetUser {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, alias = "screenName")]
    screen_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MediaDetail {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    media_url_https: Option<String>,
    #[serde(default, alias = "videoInfo")]
    video_info: Option<VideoBlock>,
}

impl MediaDetail {
    fn to_raw(&self) -> Option<RawMedia> {
        match self.kind.as_deref() {
            Some("photo") => Some(RawMedia {
                kind: MediaKind::Photo,
                url: self.media_url_https.clone()?,
            }),
            Some("video") => Some(RawMedia {
                kind: MediaKind::Video,
                url: pick_mp4(self.video_info.as_ref()?.variants.as_slice())?,
            }),
            Some("animated_gif") => Some(RawMedia {
                kind: MediaKind::Animation,
                url: pick_mp4(self.video_info.as_ref()?.variants.as_slice())?,
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PhotoItem {
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VideoBlock {
    #[serde(default)]
    variants: Vec<VideoVariant>,
}

#[derive(Debug, Deserialize)]
struct VideoVariant {
    #[serde(default)]
    bitrate: Option<u64>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct RestTweet {
    #[serde(default)]
    full_text: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    possibly_sensitive: bool,
    #[serde(default)]
    user: Option<TweetUser>,
    #[serde(default)]
    extended_entities: Option<EntityBlock>,
    #[serde(default)]
    entities: Option<EntityBlock>,
}

#[cfg(test)]
impl RestTweet {
    fn into_tweet(self) -> Tweet {
        let media_src = self
            .extended_entities
            .as_ref()
            .or(self.entities.as_ref())
            .map(|e| e.media.as_slice())
            .unwrap_or(&[]);
        let media = media_src.iter().filter_map(MediaDetail::to_raw).collect();
        Tweet {
            text: self.full_text.or(self.text),
            name: self.user.as_ref().and_then(|u| u.name.clone()),
            screen_name: self.user.as_ref().and_then(|u| u.screen_name.clone()),
            nsfw: self.possibly_sensitive,
            engagement: None,
            media,
        }
    }
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct EntityBlock {
    #[serde(default)]
    media: Vec<MediaDetail>,
}

pub struct XAuth {
    client: OnceLock<reqwest::Client>,
}

impl XAuth {
    const fn new() -> Self {
        Self {
            client: OnceLock::new(),
        }
    }

    fn client(&self) -> Result<&reqwest::Client, AuthError> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let built = reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        let _ = self.client.set(built);
        self.client
            .get()
            .ok_or_else(|| AuthError::Failed("初始化 HTTP 客户端失败".into()))
    }

    async fn verify_cookie(&self, cookie: &str) -> Result<(), AuthError> {
        if cookie_value(cookie, "auth_token").is_none_or(|v| v.len() < 8) {
            return Err(AuthError::Failed(
                "cookie 缺少 auth_token 或太短。请复制 Value，不是名称。".into(),
            ));
        }
        let Some(ct0) = cookie_value(cookie, "ct0").filter(|v| v.len() >= 8) else {
            return Err(AuthError::Failed(
                "cookie 缺少 ct0。请同时粘贴 auth_token 和 ct0，例如：\nauth_token=xxxx; ct0=yyyy"
                    .into(),
            ));
        };
        let cookie_header = HeaderValue::from_str(cookie)
            .map_err(|_| AuthError::Failed("cookie 含有非法字符".into()))?;
        let csrf =
            HeaderValue::from_str(ct0).map_err(|_| AuthError::Failed("ct0 含有非法字符".into()))?;
        let url = graphql_url(
            GQL_USER_BY_SCREEN_NAME,
            "UserByScreenName",
            GQL_USER_VARS,
            GQL_USER_FEATURES,
            Some(GQL_USER_TOGGLES),
        )
        .map_err(|e| AuthError::Failed(e.to_string()))?;
        let client = self.client()?;
        let resp = client
            .get(url)
            .header(USER_AGENT, BROWSER_UA)
            .header(ORIGIN, "https://x.com")
            .header(REFERER, "https://x.com/")
            .header("authorization", WEB_BEARER)
            .header(COOKIE, cookie_header)
            .header("x-csrf-token", csrf)
            .header("x-twitter-auth-type", "OAuth2Session")
            .header("x-twitter-active-user", "yes")
            .send()
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        let status = resp.status();
        if status.is_redirection() || status.as_u16() == 401 {
            return Err(AuthError::Failed(format!(
                "cookie 无效或已过期 (HTTP {status})"
            )));
        }
        if status.as_u16() == 403 {
            let body = resp.text().await.unwrap_or_default();
            if graphql_auth_rejected(&serde_json::from_str(&body).unwrap_or(Value::Null))
                || body.to_ascii_lowercase().contains("bad authentication")
            {
                return Err(AuthError::Failed(format!(
                    "cookie 无效或已过期 (HTTP {status})"
                )));
            }
            tracing::warn!(%status, "x cookie probe returned 403; accepting structurally valid cookies");
            return Ok(());
        }
        if !status.is_success() {
            tracing::warn!(%status, "x cookie probe inconclusive; accepting cookies");
            return Ok(());
        }
        let body = resp
            .text()
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        if let Ok(json) = serde_json::from_str::<Value>(&body) {
            if graphql_auth_rejected(&json) {
                return Err(AuthError::Failed("cookie 无效或已过期".into()));
            }
        }
        Ok(())
    }
}

const METHODS: &[AuthMethod] = &[AuthMethod::Cookie];

#[async_trait]
impl AuthProvider for XAuth {
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
            prompt: "请发送 Cookie，必须同时包含 auth_token 和 ct0（复制的是 Value，不是名称）。\n\n推荐一行：\nauth_token=xxxx; ct0=yyyy\n\n两行也可以：\nauth_token=xxxx\nct0=yyyy\n\n在已登录 x.com 的浏览器：F12 → Application → Cookies → https://x.com。含凭据的消息会在处理后删除。".into(),
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
            AuthInput::Poll => Err(AuthError::Failed("X 登录不需要轮询".into())),
            AuthInput::Text(text) => {
                let cookie = normalize_cookie(&text);
                self.verify_cookie(&cookie).await?;
                session.method = Some(AuthMethod::Cookie);
                session.step = 1;
                Ok(AuthProgress::Completed(Credential {
                    extractor_id: "x".into(),
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

#[cfg(test)]
mod tests {
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
        assert!(XExtractor.matches(&url("https://x.com/a/status/1")));
        assert!(!XExtractor.matches(&url("https://x.com/foo")));
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
        use feedbot_core::options::{merge_layers, OptionValues};
        let spec = XExtractor.options();
        assert!(spec.get("sp").is_some());
        let unset = merge_layers(spec, &OptionValues::new(), &[]).unwrap();
        assert!(unset.get("sp").is_none());
        let plus = merge_layers(spec, &OptionValues::new(), &["+sp".into()]).unwrap();
        assert!(plus.flag("sp"));
        let minus = merge_layers(spec, &OptionValues::new(), &["-sp".into()]).unwrap();
        assert_eq!(minus.get("sp"), Some(&OptionValue::Flag(false)));
    }
}
