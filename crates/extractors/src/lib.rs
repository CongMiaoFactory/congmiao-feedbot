pub mod http;
pub mod normalize;
pub mod qr;
pub mod registry;
pub mod sites;

pub use http::ReqwestClient;
pub use normalize::canonicalize;
pub use registry::{ExtractorRegistry, ExtractorSites};
