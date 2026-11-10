//! Helpers shared by the site crates: JSON and credential access, URL
//! helpers, short-link expansion, QR rendering, and (with the `testing`
//! feature) fakes for whole `extract` / `feed` runs.

pub mod qr;
#[cfg(feature = "testing")]
pub mod testing;

pub use qr::render_qr_png;

use feedbot_core::{
    ExtractError, ExtractRequest, HttpClient, HttpRequest, MediaError, Ytdlp, YtdlpSpec,
};
use http::HeaderMap;
use serde_json::Value;
use std::path::Path;
use url::Url;

pub const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Trimmed UTF-8 credential payload of the request, if any.
pub fn credential_text(req: &ExtractRequest) -> Option<&str> {
    let raw = std::str::from_utf8(&req.credential.as_ref()?.payload).ok()?;
    Some(raw.trim()).filter(|s| !s.is_empty())
}

/// Strip a leading `Cookie:` header name and wrapping quotes from pasted cookies.
pub fn strip_cookie_header(raw: &str) -> &str {
    let mut s = raw.trim();
    for prefix in ["Cookie:", "cookie:", "COOKIE:"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim();
            break;
        }
    }
    s.trim_matches(|c| matches!(c, '"' | '\'' | '“' | '”'))
        .trim()
}

pub fn json_str<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str)
}

pub fn json_u64(v: &Value, pointer: &str) -> Option<u64> {
    let v = v.pointer(pointer)?;
    v.as_u64()
        .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
}

/// Lenient counter parse: integers, non-negative floats, or `"1,234"` strings.
pub fn json_count(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    if let Some(n) = v.as_i64() {
        return u64::try_from(n).ok();
    }
    if let Some(n) = v.as_f64()
        && n.is_finite()
        && n >= 0.0
    {
        return Some(n.trunc() as u64);
    }
    v.as_str()?.replace(',', "").trim().parse().ok()
}

pub fn trim_slash(s: &str) -> String {
    s.trim().trim_end_matches('/').to_string()
}

/// Non-empty and ASCII digits only (numeric ids from URLs and APIs).
pub fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

pub fn parse_url(s: &str) -> Result<Url, ExtractError> {
    Url::parse(s).map_err(|e| ExtractError::Failed(e.to_string()))
}

/// Last non-empty path segment, used as a download filename.
pub fn last_path_segment(url: &Url) -> Option<String> {
    url.path_segments()?
        .next_back()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Keep the 1-based inclusive page range `+p:start-end`.
pub fn select_pages<T>(mut items: Vec<T>, range: Option<(i64, i64)>) -> Vec<T> {
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

/// `url` with its host replaced by `to` when the host is one of `from`.
pub fn rewrite_host(url: &Url, from: &[&str], to: &str) -> Option<Url> {
    let host = url.host_str()?.to_ascii_lowercase();
    if !from.contains(&host.as_str()) {
        return None;
    }
    let mut url = url.clone();
    url.set_host(Some(to)).ok()?;
    Some(url)
}

/// Follow a known URL shortener to its target with one `HEAD` request; other
/// URLs are returned unchanged.
pub async fn expand_shortlink(http: &dyn HttpClient, url: &Url) -> Result<Url, ExtractError> {
    if !is_shortener(url) {
        return Ok(url.clone());
    }
    let resp = http
        .fetch(HttpRequest::head(url.clone(), HeaderMap::new()))
        .await?;
    Ok(resp.final_url)
}

fn is_shortener(url: &Url) -> bool {
    matches!(
        url.host_str().unwrap_or(""),
        "t.co"
            | "b23.tv"
            | "b23.wtf"
            | "bit.ly"
            | "tinyurl.com"
            | "xhslink.com"
            | "youtu.be"
            | "163cn.tv"
    )
}

/// `Ytdlp` for registries built without the CLI (tests, tools): every call
/// fails. The binary always injects a real `YtdlpCli`.
pub struct NoYtdlp;

impl NoYtdlp {
    fn unavailable() -> MediaError {
        MediaError::Ytdlp("未配置 yt-dlp".into())
    }
}

#[async_trait::async_trait]
impl Ytdlp for NoYtdlp {
    async fn probe_json(&self, _url: &Url, _cookies: Option<&str>) -> Result<Value, MediaError> {
        Err(Self::unavailable())
    }

    async fn probe_playlist(
        &self,
        _url: &Url,
        _cookies: Option<&str>,
        _limit: u32,
    ) -> Result<Value, MediaError> {
        Err(Self::unavailable())
    }

    async fn download(
        &self,
        _url: &Url,
        _dest: &Path,
        _spec: &YtdlpSpec,
    ) -> Result<(), MediaError> {
        Err(Self::unavailable())
    }
}
