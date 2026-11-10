//! Anonymous tweet lookup through `cdn.syndication.twimg.com`.

use super::media::pick_mp4;
use super::{RawMedia, Tweet};
use feedbot_core::extract::{Engagement, ExtractCtx, ExtractError, HttpRequest, MediaKind};
use http::header::USER_AGENT;
use http::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use feedbot_site_common::{BROWSER_UA, json_count};

pub(super) async fn fetch_syndication(ctx: &ExtractCtx, id: u64) -> Result<Tweet, ExtractError> {
    let token = syndication_token(id);
    let url = Url::parse(&format!(
        "https://cdn.syndication.twimg.com/tweet-result?id={id}&lang=zh&token={token}"
    ))
    .map_err(|e| ExtractError::Failed(e.to_string()))?;
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(BROWSER_UA));
    let resp = ctx.http.fetch(HttpRequest::get(url, headers)).await?;
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

pub(super) fn syndication_token(id: u64) -> String {
    let n = (id as f64) / 1e15 * std::f64::consts::PI;
    js_to_string_36(n)
        .chars()
        .filter(|c| *c != '0' && *c != '.')
        .collect()
}

pub(super) fn js_to_string_36(value: f64) -> String {
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

#[derive(Debug, Deserialize)]
pub(super) struct SyndicationTweet {
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
    pub(super) fn is_tombstone(&self) -> bool {
        self.typename
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("Tombstone"))
    }

    pub(super) fn into_tweet(self) -> Tweet {
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
        if media.is_empty()
            && let Some(url) = self.video.as_ref().and_then(|v| pick_mp4(&v.variants))
        {
            media.push(RawMedia {
                kind: MediaKind::Video,
                url,
            });
        }
        Tweet {
            text: self.text,
            name: self.user.as_ref().and_then(|u| u.name.clone()),
            screen_name: self.user.as_ref().and_then(|u| u.screen_name.clone()),
            nsfw: self.possibly_sensitive,
            engagement: Engagement::packed(
                self.view_count.or_else(|| {
                    json_count(self.views.as_ref().and_then(|v| v.get("count")))
                        .or_else(|| json_count(self.views.as_ref()))
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
pub(super) struct TweetUser {
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default, alias = "screenName")]
    pub(super) screen_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct MediaDetail {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    media_url_https: Option<String>,
    #[serde(default, alias = "videoInfo")]
    video_info: Option<VideoBlock>,
}

impl MediaDetail {
    pub(super) fn to_raw(&self) -> Option<RawMedia> {
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
pub(super) struct PhotoItem {
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct VideoBlock {
    #[serde(default)]
    variants: Vec<VideoVariant>,
}

#[derive(Debug, Deserialize)]
pub(super) struct VideoVariant {
    #[serde(default)]
    pub(super) bitrate: Option<u64>,
    #[serde(default)]
    pub(super) content_type: Option<String>,
    #[serde(default)]
    pub(super) url: Option<String>,
}
