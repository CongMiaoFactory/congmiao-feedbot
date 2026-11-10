//! The extractor registry (name and URL lookup, URL canonicalization) over
//! the site crates, plus the reqwest-backed `HttpClient` the binary injects.

pub mod http;
pub mod normalize;
pub mod registry;

pub use feedbot_bilibili::BilibiliEndpoints;
pub use feedbot_netease::NeteaseEndpoints;
pub use feedbot_x::XQueryIds;
pub use http::ReqwestClient;
pub use registry::{ExtractorRegistry, ExtractorSites};

use feedbot_core::HttpClient;
use std::sync::Arc;
use std::time::Duration;

/// Stand-alone client for [`ExtractorRegistry::builtin`] (tests, tools).
pub(crate) fn default_http() -> Arc<dyn HttpClient> {
    Arc::new(
        ReqwestClient::new(
            concat!("CongMiaoFeedBot/", env!("CARGO_PKG_VERSION")),
            Duration::from_secs(30),
            Duration::from_secs(10),
        )
        .expect("build default HTTP client"),
    )
}
