//! URL canonicalization: lowercase host, the owning site's rewrite
//! ([`Extractor::rewrite_url`]), tracking-query strip.

use feedbot_core::Extractor;
use std::sync::Arc;
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

/// Use [`crate::ExtractorRegistry::canonicalize`]; this takes the extractors
/// so sites keep their own host rules.
pub(crate) fn canonicalize(url: &Url, extractors: &[Arc<dyn Extractor>]) -> Url {
    let mut url = url.clone();
    if let Some(host) = url.host_str().map(|h| h.to_ascii_lowercase()) {
        let _ = url.set_host(Some(&host));
    }
    if let Some(next) = extractors.iter().find_map(|e| e.rewrite_url(&url)) {
        url = next;
    }
    strip_tracking(&mut url);
    url
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
        // Re-encode: the pairs above are decoded, so `a%26b` must not come
        // back as a bare `a&b`.
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.extend_pairs(&pairs);
        url.set_query(Some(&query.finish()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(url: &Url) -> Vec<(String, String)> {
        url.query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    fn p(k: &str, v: &str) -> (String, String) {
        (k.into(), v.into())
    }

    #[test]
    fn strips_tracking_and_lowercases_host() {
        let c = canonicalize(
            &Url::parse("https://Example.COM/a?utm_source=t&z=1&si=x").unwrap(),
            &[],
        );
        assert_eq!(c.host_str(), Some("example.com"));
        assert_eq!(pairs(&c), [p("z", "1")]);
        let bare = canonicalize(
            &Url::parse("https://example.com/a?utm_medium=c").unwrap(),
            &[],
        );
        assert!(bare.query().is_none());
    }

    #[test]
    fn keeps_encoded_query_values() {
        let c = canonicalize(
            &Url::parse("https://example.com/a?q=a%26b&z=1").unwrap(),
            &[],
        );
        assert_eq!(pairs(&c), [p("q", "a&b"), p("z", "1")]);
        let plus = canonicalize(&Url::parse("https://example.com/a?q=a+b").unwrap(), &[]);
        assert_eq!(pairs(&plus), [p("q", "a b")]);
    }
}
