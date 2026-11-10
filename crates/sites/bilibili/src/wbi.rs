//! WBI request signing, required by some web endpoints (space feed) to avoid
//! the -352 risk check. Keys come from `/x/web-interface/nav` and rotate daily.

use md5::{Digest, Md5};
use serde_json::Value;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use feedbot_site_common::json_str;

/// Keys are refetched after this, or right away after a -352.
pub(super) const KEY_TTL: Duration = Duration::from_secs(3600);

const MIXIN_KEY_ENC_TAB: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

#[derive(Debug, Clone)]
pub(super) struct WbiKey {
    pub(super) mixin: String,
    pub(super) fetched: Instant,
}

/// Mixin key from a nav response. The response is `-101` without login but
/// still carries `wbi_img`.
pub(super) fn mixin_from_nav(nav: &Value) -> Option<String> {
    let img = key_from_url(json_str(nav, "/data/wbi_img/img_url")?)?;
    let sub = key_from_url(json_str(nav, "/data/wbi_img/sub_url")?)?;
    mixin_key(&format!("{img}{sub}"))
}

/// `https://i0.hdslb.com/bfs/wbi/{key}.png` → `{key}`.
fn key_from_url(url: &str) -> Option<&str> {
    let name = url.rsplit('/').next()?;
    let key = name.split('.').next()?;
    (!key.is_empty()).then_some(key)
}

fn mixin_key(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    MIXIN_KEY_ENC_TAB
        .iter()
        .take(32)
        .map(|&i| bytes.get(i).map(|&b| b as char))
        .collect()
}

/// Signed query string (`…&wts=…&w_rid=…`). Sent as is, so the signed and
/// the sent encoding cannot differ.
pub(super) fn signed_query(params: &[(String, String)], mixin: &str, wts: u64) -> String {
    let mut pairs: Vec<(&str, String)> = params
        .iter()
        .map(|(k, v)| (k.as_str(), v.replace(['!', '\'', '(', ')', '*'], "")))
        .collect();
    let wts = wts.to_string();
    pairs.push(("wts", wts));
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let query = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let digest = Md5::digest(format!("{query}{mixin}").as_bytes());
    let w_rid: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{query}&w_rid={w_rid}")
}

pub(super) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// `encodeURIComponent`, minus the characters signing already strips.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(k: &str, v: &str) -> (String, String) {
        (k.into(), v.into())
    }

    /// Example from bilibili-API-collect's WBI document.
    #[test]
    fn documented_example() {
        let nav = json!({
            "code": -101,
            "data": { "wbi_img": {
                "img_url": "https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png",
                "sub_url": "https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png"
            } }
        });
        let mixin = mixin_from_nav(&nav).unwrap();
        assert_eq!(mixin, "ea1db124af3c7062474693fa704f4ff8");
        let params = [p("foo", "114"), p("bar", "514"), p("zab", "1919810")];
        assert_eq!(
            signed_query(&params, &mixin, 1702204169),
            "bar=514&foo=114&wts=1702204169&zab=1919810&w_rid=8f6f2b5b3d485fe1886cec6a0be8c5d4"
        );
    }

    #[test]
    fn strips_and_encodes_values() {
        let q = signed_query(&[p("k", "a b(!)*'~,")], "m", 1);
        assert!(q.starts_with("k=a%20b~%2C&wts=1&w_rid="), "{q}");
    }

    #[test]
    fn bad_nav_has_no_key() {
        assert_eq!(mixin_from_nav(&json!({ "data": {} })), None);
        let short = json!({ "data": { "wbi_img": {
            "img_url": "https://i0.hdslb.com/bfs/wbi/abc.png",
            "sub_url": "https://i0.hdslb.com/bfs/wbi/def.png"
        } } });
        assert_eq!(mixin_from_nav(&short), None);
    }
}
