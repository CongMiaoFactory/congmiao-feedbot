use feedbot_core::{Extractor, ExtractorLookup, HttpClient, Ytdlp};
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

use feedbot_bilibili::{BilibiliEndpoints, BilibiliExtractor};
use feedbot_netease::{NeteaseEndpoints, NeteaseExtractor};
use feedbot_pixiv::PixivExtractor;
use feedbot_site_common::NoYtdlp;
use feedbot_x::{XExtractor, XQueryIds};
use feedbot_youtube::YoutubeExtractor;

use crate::normalize;

/// Minimum Jaro-Winkler similarity for a fuzzy platform-name match.
const FUZZY_THRESHOLD: f64 = 0.85;

/// Shared dependencies injected into every site extractor and auth provider.
#[derive(Clone)]
pub struct ExtractorSites {
    pub http: Arc<dyn HttpClient>,
    pub ytdlp: Arc<dyn Ytdlp>,
    pub bilibili: BilibiliEndpoints,
    pub netease: NeteaseEndpoints,
    pub x: XQueryIds,
    /// Largest file the bot can upload; extractors that can choose a quality
    /// pick one that fits.
    pub max_upload_bytes: Option<u64>,
}

pub struct ExtractorRegistry {
    extractors: Vec<Arc<dyn Extractor>>,
    alias_index: HashMap<String, usize>,
}

impl ExtractorRegistry {
    /// Registry with default endpoints and a stand-alone HTTP client, for tests
    /// and tools. yt-dlp is not available: YouTube extraction fails.
    pub fn builtin() -> Self {
        Self::with_sites(ExtractorSites {
            http: crate::default_http(),
            ytdlp: Arc::new(NoYtdlp),
            bilibili: BilibiliEndpoints::default(),
            netease: NeteaseEndpoints::default(),
            x: XQueryIds::default(),
            max_upload_bytes: None,
        })
    }

    pub fn with_sites(sites: ExtractorSites) -> Self {
        let mut extractors: Vec<Arc<dyn Extractor>> = vec![
            Arc::new(PixivExtractor::new(sites.http.clone())),
            Arc::new(XExtractor::new(sites.http.clone(), sites.x)),
            Arc::new(YoutubeExtractor::new(sites.ytdlp)),
            Arc::new(BilibiliExtractor::new(
                sites.bilibili,
                sites.http.clone(),
                sites.max_upload_bytes,
            )),
            Arc::new(NeteaseExtractor::new(sites.netease, sites.http)),
        ];
        extractors.sort_by_key(|e| std::cmp::Reverse(e.priority()));
        let mut alias_index = HashMap::new();
        for (idx, extractor) in extractors.iter().enumerate() {
            alias_index.insert(extractor.id().to_ascii_lowercase(), idx);
            for alias in extractor.aliases() {
                alias_index.insert(alias.to_ascii_lowercase(), idx);
            }
        }
        Self {
            extractors,
            alias_index,
        }
    }

    pub fn resolve_name(&self, name: &str) -> Option<Arc<dyn Extractor>> {
        let key = name.trim().to_ascii_lowercase();
        if key.is_empty() {
            return None;
        }
        if let Some(idx) = self.alias_index.get(&key) {
            return self.extractors.get(*idx).cloned();
        }
        self.alias_index
            .iter()
            .map(|(alias, idx)| (strsim::jaro_winkler(&key, alias), *idx))
            .filter(|(score, _)| *score >= FUZZY_THRESHOLD)
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, idx)| self.extractors[idx].clone())
    }

    pub fn suggest_name(&self, name: &str) -> Option<String> {
        self.resolve_name(name).map(|e| e.id().to_string())
    }

    /// Canonical form of `url`: lowercase host, the owning site's
    /// [`Extractor::rewrite_url`], no tracking parameters. Used for matching,
    /// de-duplication, and stored subscription sources.
    pub fn canonicalize(&self, url: &Url) -> Url {
        normalize::canonicalize(url, &self.extractors)
    }

    pub fn match_url(&self, url: &Url) -> Option<Arc<dyn Extractor>> {
        let canonical = self.canonicalize(url);
        self.extractors
            .iter()
            .find(|e| e.matches(&canonical) || e.matches(url))
            .cloned()
    }

    /// Extractor that can poll `url` as a subscription source.
    pub fn match_feed(&self, url: &Url) -> Option<Arc<dyn Extractor>> {
        let canonical = self.canonicalize(url);
        self.extractors
            .iter()
            .find(|e| e.feed_matches(&canonical) || e.feed_matches(url))
            .cloned()
    }

    /// Exact match on [`Extractor::id`]; use for ids read back from storage.
    pub fn by_id(&self, id: &str) -> Option<Arc<dyn Extractor>> {
        self.extractors.iter().find(|e| e.id() == id).cloned()
    }

    pub fn all(&self) -> &[Arc<dyn Extractor>] {
        &self.extractors
    }
}

impl ExtractorLookup for ExtractorRegistry {
    fn by_id(&self, id: &str) -> Option<Arc<dyn Extractor>> {
        ExtractorRegistry::by_id(self, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_id_is_exact() {
        let registry = ExtractorRegistry::builtin();
        for e in registry.all() {
            assert_eq!(registry.by_id(e.id()).map(|f| f.id()), Some(e.id()));
        }
        // Aliases and near-misses resolve by name, but never by id.
        assert!(registry.resolve_name("pixi").is_some());
        assert!(registry.by_id("pixi").is_none());
        assert!(registry.by_id("Pixiv").is_none());
        assert!(registry.by_id("").is_none());
    }

    #[test]
    fn canonicalize_uses_site_rewrites() {
        let registry = ExtractorRegistry::builtin();
        let canon = |s: &str| registry.canonicalize(&Url::parse(s).unwrap()).to_string();
        assert_eq!(
            canon("https://youtu.be/abc123?si=1&t=5"),
            "https://youtube.com/watch?v=abc123&t=5"
        );
        assert_eq!(
            canon("https://music.youtube.com/watch?v=abc&si=xx"),
            "https://music.youtube.com/watch?v=abc"
        );
        assert_eq!(
            canon("https://X.com/foo/status/1?utm_source=t"),
            "https://twitter.com/foo/status/1"
        );
        assert_eq!(
            canon("https://www.pixiv.net/artworks/1"),
            "https://pixiv.net/artworks/1"
        );
        assert_eq!(
            canon("https://b23.tv/BV1xx411c7mD?share_source=copy"),
            "https://bilibili.com/video/BV1xx411c7mD"
        );
        // Opaque short codes need a request; `extract` expands them.
        assert_eq!(canon("https://b23.tv/AbCdEf1"), "https://b23.tv/AbCdEf1");
        assert_eq!(
            canon("https://example.com/a?z=1"),
            "https://example.com/a?z=1"
        );
    }

    #[test]
    fn multibyte_path_segments_do_not_panic() {
        let registry = ExtractorRegistry::builtin();
        for s in [
            "https://www.bilibili.com/video/中文",
            "https://www.bilibili.com/bangumi/play/中文",
            "https://b23.tv/中文",
        ] {
            let url = Url::parse(s).unwrap();
            let _ = registry.canonicalize(&url);
            let _ = registry.match_url(&url);
        }
    }

    #[test]
    fn names_and_feeds_resolve_to_sites() {
        let registry = ExtractorRegistry::builtin();
        for (id, names) in [
            ("youtube", &["youtube", "yt", "ytb", "ytbm"][..]),
            ("netease", &["netease", "ncm", "163", "网易云"]),
            ("bilibili", &["bilibili", "bili", "b站"]),
            ("pixiv", &["pixiv", "px"]),
            ("x", &["x", "twitter", "tw"]),
        ] {
            for name in names {
                assert_eq!(
                    registry.resolve_name(name).map(|e| e.id()),
                    Some(id),
                    "{name}"
                );
            }
        }
        assert_eq!(
            registry
                .match_feed(&Url::parse("https://m.bilibili.com/space/1").unwrap())
                .map(|e| e.id()),
            Some("bilibili")
        );
    }
}
