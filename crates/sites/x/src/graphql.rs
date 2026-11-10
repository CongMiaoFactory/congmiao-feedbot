//! Logged-in tweet lookup through the web GraphQL API.

use super::Tweet;
use super::auth::cookie_value;
use super::syndication::MediaDetail;
#[cfg(test)]
use super::syndication::TweetUser;
use feedbot_core::extract::{Engagement, ExtractCtx, ExtractError, HttpRequest, HttpResponse};
use http::header::{COOKIE, ORIGIN, REFERER, USER_AGENT};
use http::{HeaderMap, HeaderName, HeaderValue};
#[cfg(test)]
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use feedbot_site_common::{BROWSER_UA, json_count, json_str};

/// Public web client bearer used by x.com itself (literal `%3D`, not decoded `=`).
const WEB_BEARER: &str = "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";

pub(super) const GQL_TWEET_FEATURES: &str = r#"{"payments_enabled":false,"rweb_xchat_enabled":false,"profile_label_improvements_pcf_label_in_post_enabled":true,"rweb_tipjar_consumption_enabled":true,"verified_phone_label_enabled":false,"creator_subscriptions_tweet_preview_api_enabled":true,"responsive_web_graphql_timeline_navigation_enabled":true,"responsive_web_graphql_skip_user_profile_image_extensions_enabled":false,"premium_content_api_read_enabled":false,"communities_web_enable_tweet_community_results_fetch":true,"c9s_tweet_anatomy_moderator_badge_enabled":true,"responsive_web_grok_analyze_button_fetch_trends_enabled":false,"responsive_web_grok_analyze_post_followups_enabled":true,"responsive_web_jetfuel_frame":true,"responsive_web_grok_share_attachment_enabled":true,"articles_preview_enabled":true,"responsive_web_edit_tweet_api_enabled":true,"graphql_is_translatable_rweb_tweet_is_translatable_enabled":true,"view_counts_everywhere_api_enabled":true,"longform_notetweets_consumption_enabled":true,"responsive_web_twitter_article_tweet_consumption_enabled":true,"tweet_awards_web_tipping_enabled":false,"responsive_web_grok_show_grok_translated_post":false,"responsive_web_grok_analysis_button_from_backend":true,"creator_subscriptions_quote_tweet_preview_enabled":false,"freedom_of_speech_not_reach_fetch_enabled":true,"standardized_nudges_misinfo":true,"tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled":true,"longform_notetweets_rich_text_read_enabled":true,"longform_notetweets_inline_media_enabled":true,"responsive_web_grok_image_annotation_enabled":true,"responsive_web_grok_imagine_annotation_enabled":true,"responsive_web_grok_community_note_auto_translation_is_enabled":false,"responsive_web_enhance_cards_enabled":false}"#;
const GQL_TWEET_TOGGLES: &str = r#"{"withArticleRichContentState":true,"withArticlePlainText":false,"withGrokAnalyze":false,"withDisallowedReplyControls":false}"#;

pub(super) async fn fetch_graphql_tweet(
    ctx: &ExtractCtx,
    query_id: &str,
    id: u64,
    cookie: Option<&str>,
) -> Result<Tweet, ExtractError> {
    let variables = format!(
        r#"{{"tweetId":"{id}","withCommunity":false,"includePromotedContent":false,"withVoice":false}}"#
    );
    let url = graphql_url(
        query_id,
        "TweetResultByRestId",
        &variables,
        GQL_TWEET_FEATURES,
        Some(GQL_TWEET_TOGGLES),
    )?;
    let headers = api_headers(cookie)?;
    let resp = ctx.http.fetch(HttpRequest::get(url, headers)).await?;
    tweet_from_graphql(&graphql_json(&resp, "推文不存在或已删除")?)
}

/// Status, JSON, and auth checks shared by every GraphQL call. A 3xx only
/// arrives when the caller disabled redirects; it means "go log in".
pub(super) fn graphql_json(resp: &HttpResponse, not_found: &str) -> Result<Value, ExtractError> {
    match resp.status {
        300..=399 | 401 | 403 => return Err(ExtractError::AuthRequired("x".into())),
        404 => return Err(ExtractError::Failed(not_found.into())),
        429 => return Err(ExtractError::Network("X 请求过于频繁 (HTTP 429)".into())),
        s if s >= 400 => return Err(ExtractError::Network(format!("HTTP {s}"))),
        _ => {}
    }
    let json: Value =
        serde_json::from_slice(&resp.body).map_err(|e| ExtractError::Failed(e.to_string()))?;
    if graphql_auth_rejected(&json) {
        return Err(ExtractError::AuthRequired("x".into()));
    }
    Ok(json)
}

/// `TweetWithVisibilityResults` wraps the actual tweet.
pub(super) fn unwrap_visibility(result: &Value) -> Option<&Value> {
    if json_str(result, "/__typename") == Some("TweetWithVisibilityResults") {
        result.get("tweet")
    } else {
        Some(result)
    }
}

pub(super) fn graphql_url(
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

pub(super) fn graphql_auth_rejected(json: &Value) -> bool {
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

pub(super) fn tweet_from_graphql(json: &Value) -> Result<Tweet, ExtractError> {
    let result = json
        .pointer("/data/tweetResult/result")
        .and_then(unwrap_visibility)
        .ok_or_else(|| ExtractError::Failed("推文不存在或已删除".into()))?;
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
            json_count(result.pointer("/views/count")),
            json_count(legacy.get("favorite_count")),
            json_count(legacy.get("retweet_count")),
            json_count(legacy.get("reply_count")),
        ),
        media: details.iter().filter_map(MediaDetail::to_raw).collect(),
    })
}

pub(super) fn api_headers(cookie: Option<&str>) -> Result<HeaderMap, ExtractError> {
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

#[cfg(test)]
#[derive(Debug, Deserialize)]
pub(super) struct RestTweet {
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
    pub(super) fn into_tweet(self) -> Tweet {
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
pub(super) struct EntityBlock {
    #[serde(default)]
    media: Vec<MediaDetail>,
}
