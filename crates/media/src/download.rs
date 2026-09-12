use bytes::Bytes;
use feedbot_core::MediaError;
use http::HeaderMap;
use std::path::Path;
use tokio::io::AsyncWriteExt;
use url::Url;

#[derive(Clone)]
pub struct Downloader {
    client: reqwest::Client,
}

impl Downloader {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    pub async fn to_bytes(&self, url: &Url, headers: &HeaderMap) -> Result<Bytes, MediaError> {
        let mut builder = self.client.get(url.clone());
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| MediaError::Download(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(MediaError::Download(format!("HTTP {}", resp.status())));
        }
        resp.bytes()
            .await
            .map_err(|e| MediaError::Download(e.to_string()))
    }

    pub async fn to_file(
        &self,
        url: &Url,
        headers: &HeaderMap,
        path: &Path,
    ) -> Result<u64, MediaError> {
        let bytes = self.to_bytes(url, headers).await?;
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(|e| MediaError::Io(e.to_string()))?;
        file.write_all(&bytes)
            .await
            .map_err(|e| MediaError::Io(e.to_string()))?;
        Ok(bytes.len() as u64)
    }
}
