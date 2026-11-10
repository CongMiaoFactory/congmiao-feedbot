use feedbot_core::{ExtractError, HttpClient, HttpMethod, HttpRequest, HttpResponse};
use http::HeaderMap;
use reqwest::redirect::Policy;
use std::time::Duration;

/// Shared HTTP client for extractors and auth providers.
///
/// There is deliberately no cookie store: credentials are per user and are
/// always passed as explicit `Cookie` headers. A shared jar would replay one
/// user's `Set-Cookie` responses on another user's anonymous requests.
pub struct ReqwestClient {
    follow: reqwest::Client,
    no_follow: reqwest::Client,
}

impl ReqwestClient {
    pub fn new(
        user_agent: &str,
        timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<Self, ExtractError> {
        let build = |policy: Policy| {
            reqwest::Client::builder()
                .user_agent(user_agent)
                .timeout(timeout)
                .connect_timeout(connect_timeout)
                .redirect(policy)
                .build()
                .map_err(|e| ExtractError::Network(e.to_string()))
        };
        Ok(Self {
            follow: build(Policy::limited(10))?,
            no_follow: build(Policy::none())?,
        })
    }

    /// Redirect-following client, also used for media downloads.
    pub fn inner(&self) -> &reqwest::Client {
        &self.follow
    }
}

#[async_trait::async_trait]
impl HttpClient for ReqwestClient {
    async fn fetch(&self, req: HttpRequest) -> Result<HttpResponse, ExtractError> {
        let client = if req.follow_redirects {
            &self.follow
        } else {
            &self.no_follow
        };
        let builder = match req.method {
            HttpMethod::Get => client.get(req.url),
            HttpMethod::Head => client.head(req.url),
            HttpMethod::Post => client.post(req.url).body(req.body),
        };
        let response = builder
            .headers(req.headers)
            .send()
            .await
            .map_err(|e| ExtractError::Network(e.to_string()))?;
        let status = response.status().as_u16();
        let final_url = response.url().clone();
        let headers: HeaderMap = response.headers().clone();
        let body = response
            .bytes()
            .await
            .map_err(|e| ExtractError::Network(e.to_string()))?;
        Ok(HttpResponse {
            status,
            headers,
            body,
            final_url,
        })
    }
}
