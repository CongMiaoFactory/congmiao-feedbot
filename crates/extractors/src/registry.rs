use feedbot_core::Extractor;
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

use crate::normalize::canonicalize;
use crate::sites;
use crate::sites::bilibili::BilibiliEndpoints;
use crate::sites::netease::NeteaseEndpoints;

#[derive(Debug, Clone, Default)]
pub struct ExtractorSites {
    pub bilibili: BilibiliEndpoints,
    pub netease: NeteaseEndpoints,
}

pub struct ExtractorRegistry {
    extractors: Vec<Arc<dyn Extractor>>,
    alias_index: HashMap<String, usize>,
}

impl ExtractorRegistry {
    pub fn builtin() -> Self {
        Self::with_sites(ExtractorSites::default())
    }

    pub fn with_sites(sites: ExtractorSites) -> Self {
        let mut reg = Self {
            extractors: Vec::new(),
            alias_index: HashMap::new(),
        };
        reg.register(Arc::new(sites::pixiv::PixivExtractor));
        reg.register(Arc::new(sites::x::XExtractor));
        reg.register(Arc::new(sites::youtube::YoutubeExtractor));
        reg.register(Arc::new(sites::bilibili::BilibiliExtractor::new(
            sites.bilibili,
        )));
        reg.register(Arc::new(sites::netease::NeteaseExtractor::new(
            sites.netease,
        )));
        reg
    }

    pub fn register(&mut self, extractor: Arc<dyn Extractor>) {
        let idx = self.extractors.len();
        self.index_name(extractor.id(), idx);
        for alias in extractor.aliases() {
            self.index_name(alias, idx);
        }
        self.extractors.push(extractor);
        self.extractors
            .sort_by_key(|e| std::cmp::Reverse(e.priority()));
        self.rebuild_alias_index();
    }

    fn index_name(&mut self, name: &str, idx: usize) {
        self.alias_index.insert(name.to_ascii_lowercase(), idx);
    }

    fn rebuild_alias_index(&mut self) {
        self.alias_index.clear();
        for (idx, extractor) in self.extractors.iter().enumerate() {
            self.alias_index
                .insert(extractor.id().to_ascii_lowercase(), idx);
            for alias in extractor.aliases() {
                self.alias_index.insert(alias.to_ascii_lowercase(), idx);
            }
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
        let mut best: Option<(usize, f64)> = None;
        for (idx, extractor) in self.extractors.iter().enumerate() {
            let score = strsim_score(&key, extractor.id());
            if score >= 0.78 && best.map(|(_, s)| score > s).unwrap_or(true) {
                best = Some((idx, score));
            }
            for alias in extractor.aliases() {
                let s = strsim_score(&key, alias);
                if s >= 0.78 && best.map(|(_, prev)| s > prev).unwrap_or(true) {
                    best = Some((idx, s));
                }
            }
        }
        best.map(|(idx, _)| self.extractors[idx].clone())
    }

    pub fn suggest_name(&self, name: &str) -> Option<String> {
        self.resolve_name(name).map(|e| e.id().to_string())
    }

    pub fn match_url(&self, url: &Url) -> Option<Arc<dyn Extractor>> {
        let canonical = canonicalize(url);
        self.extractors
            .iter()
            .find(|e| e.matches(&canonical) || e.matches(url))
            .cloned()
    }

    pub fn all(&self) -> &[Arc<dyn Extractor>] {
        &self.extractors
    }
}

fn strsim_score(a: &str, b: &str) -> f64 {
    // local jaro-winkler-ish via core's strsim is not re-exported; simple ratio:
    if a == b {
        return 1.0;
    }
    let mut same = 0usize;
    for (ca, cb) in a.chars().zip(b.chars()) {
        if ca == cb {
            same += 1;
        }
    }
    let max = a.len().max(b.len()) as f64;
    if max == 0.0 {
        0.0
    } else {
        same as f64 / max
    }
}
