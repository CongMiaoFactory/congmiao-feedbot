//! URL canonicalization: host aliases, tracking-query strip, lowercase host.

use url::Url;

const TRACKING_PREFIXES: &[&str] = &[
    "utm_",
    "spm",
    "from",
    "share",
    "vd_source",
    "ref",
    "referrer",
    "fbclid",
    "gclid",
    "si",
];

pub fn canonicalize(url: &Url) -> Url {
    let mut url = url.clone();
    if let Some(host) = url.host_str().map(|h| h.to_ascii_lowercase()) {
        if matches!(host.as_str(), "youtu.be" | "www.youtu.be") {
            rewrite_youtu_be(&mut url);
        } else {
            let mapped = match host.as_str() {
                "x.com" | "mobile.twitter.com" | "m.twitter.com" | "www.twitter.com" => {
                    "twitter.com"
                }
                "m.bilibili.com" | "www.bilibili.com" => "bilibili.com",
                "www.youtube.com" | "m.youtube.com" => "youtube.com",
                "www.music.youtube.com" => "music.youtube.com",
                "www.pixiv.net" => "pixiv.net",
                other => other,
            };
            let _ = url.set_host(Some(mapped));
        }
    }
    strip_tracking(&mut url);
    url
}

fn rewrite_youtu_be(url: &mut Url) {
    let id = url
        .path_segments()
        .and_then(|mut s| s.next())
        .unwrap_or("")
        .to_string();
    let extra: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "v")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let _ = url.set_host(Some("youtube.com"));
    url.set_path("/watch");
    let mut q = format!("v={id}");
    for (k, v) in extra {
        q.push('&');
        q.push_str(&k);
        q.push('=');
        q.push_str(&v);
    }
    if id.is_empty() {
        url.set_query(None);
    } else {
        url.set_query(Some(&q));
    }
}

fn strip_tracking(url: &mut Url) {
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| {
            let key = k.to_ascii_lowercase();
            !TRACKING_PREFIXES
                .iter()
                .any(|p| key == *p || key.starts_with(p))
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if pairs.is_empty() {
        url.set_query(None);
    } else {
        let encoded = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        url.set_query(Some(&encoded));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_x_and_strips_utm() {
        let u = Url::parse("https://x.com/foo/status/1?utm_source=t&utm_medium=c").unwrap();
        let c = canonicalize(&u);
        assert_eq!(c.host_str(), Some("twitter.com"));
        assert!(c.query().is_none());
    }

    #[test]
    fn keeps_music_host_and_rewrites_youtu_be() {
        let music =
            canonicalize(&Url::parse("https://music.youtube.com/watch?v=abc&si=xx").unwrap());
        assert_eq!(music.host_str(), Some("music.youtube.com"));
        assert_eq!(
            music
                .query_pairs()
                .find(|(k, _)| k == "v")
                .map(|(_, v)| v.into_owned()),
            Some("abc".into())
        );
        assert!(music.query_pairs().all(|(k, _)| k != "si"));

        let short = canonicalize(&Url::parse("https://youtu.be/abc123?si=1").unwrap());
        assert_eq!(short.host_str(), Some("youtube.com"));
        assert_eq!(short.path(), "/watch");
        assert_eq!(
            short
                .query_pairs()
                .find(|(k, _)| k == "v")
                .map(|(_, v)| v.into_owned()),
            Some("abc123".into())
        );
    }
}
