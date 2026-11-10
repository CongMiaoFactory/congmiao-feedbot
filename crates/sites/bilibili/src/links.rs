//! Recognizing Bilibili links and turning them into a `BilibiliTarget`.

use super::BilibiliTarget;
use super::dynamic::normalize_asset;
use feedbot_core::extract::{ExtractCtx, ExtractError};
use feedbot_site_common::expand_shortlink;
use url::Url;

use feedbot_site_common::{is_digits, rewrite_host};

pub(super) fn parse_target(url: &Url) -> Option<BilibiliTarget> {
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
            "opus" | "dynamic" if next.is_some_and(|id| id.chars().all(|c| c.is_ascii_digit())) => {
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

pub(super) fn is_bilibili_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "bilibili.com" || host.ends_with(".bilibili.com")
}

pub(super) fn is_short_bili_host(host: &str) -> bool {
    matches!(host.to_ascii_lowercase().as_str(), "b23.tv" | "b23.wtf")
}

/// Host aliases, and `b23.tv/{BV…|av…}` short links whose target is known
/// without a request; other short codes are expanded in `extract`.
pub(super) fn rewrite(url: &Url) -> Option<Url> {
    let host = url.host_str()?.to_ascii_lowercase();
    if is_short_bili_host(&host) {
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let [id] = segments.as_slice() else {
            return None;
        };
        if !(is_bvid(id) || is_av(id)) {
            return None;
        }
        let mut next = url.clone();
        next.set_host(Some("bilibili.com")).ok()?;
        next.set_path(&format!("/video/{id}"));
        return Some(next);
    }
    rewrite_host(url, &["m.bilibili.com", "www.bilibili.com"], "bilibili.com")
}

/// The rest of `s` after a two-letter ASCII `prefix`, case-insensitive.
/// Never slices inside a multi-byte character.
fn after_prefix<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..2)?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[2..])
}

pub(super) fn is_bvid(s: &str) -> bool {
    s.len() == 12
        && after_prefix(s, "BV").is_some_and(|rest| rest.chars().all(|c| c.is_ascii_alphanumeric()))
}

pub(super) fn is_av(s: &str) -> bool {
    after_prefix(s, "av").is_some_and(is_digits)
}

pub(super) fn is_ep_ss(s: &str) -> bool {
    after_prefix(s, "ep")
        .or_else(|| after_prefix(s, "ss"))
        .is_some_and(is_digits)
}

pub(super) fn is_au(s: &str) -> bool {
    after_prefix(s, "au").is_some_and(is_digits)
}

pub(super) fn is_cv(s: &str) -> bool {
    after_prefix(s, "cv").is_some_and(is_digits)
}

pub(super) fn av_digits(s: &str) -> Option<String> {
    if is_av(s) {
        Some(s[2..].to_string())
    } else if s.chars().all(|c| c.is_ascii_digit()) {
        Some(s.to_string())
    } else {
        None
    }
}

pub(super) async fn expand_if_short(ctx: &ExtractCtx, url: &Url) -> Result<Url, ExtractError> {
    if is_short_bili_host(url.host_str().unwrap_or("")) {
        expand_shortlink(ctx.http.as_ref(), url).await
    } else {
        Ok(url.clone())
    }
}

pub(super) fn parse_https_url(s: &str) -> Option<Url> {
    Url::parse(&normalize_asset(s)?).ok()
}
