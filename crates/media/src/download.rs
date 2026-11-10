use feedbot_core::MediaError;
use http::HeaderMap;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use url::Url;

/// Streams HTTP bodies to disk with a hard size cap, so one large upstream
/// file cannot exhaust memory or disk.
#[derive(Clone)]
pub struct Downloader {
    client: reqwest::Client,
    max_bytes: u64,
}

impl Downloader {
    pub fn new(client: reqwest::Client, max_bytes: u64) -> Self {
        Self {
            client,
            max_bytes: max_bytes.max(1),
        }
    }

    /// Client suited to large bodies: no overall deadline, only connect and
    /// per-read timeouts, so slow but progressing downloads are not cut off.
    pub fn build(
        user_agent: &str,
        connect_timeout: Duration,
        read_timeout: Duration,
        max_bytes: u64,
    ) -> Result<Self, MediaError> {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout)
            .build()
            .map_err(|e| MediaError::Download(e.to_string()))?;
        Ok(Self::new(client, max_bytes))
    }

    pub async fn to_file(
        &self,
        url: &Url,
        headers: &HeaderMap,
        path: &Path,
    ) -> Result<u64, MediaError> {
        let mut builder = self.client.get(url.clone());
        for (name, value) in headers.iter() {
            builder = builder.header(name, value);
        }
        let mut resp = builder
            .send()
            .await
            .map_err(|e| MediaError::Download(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(MediaError::Download(format!("HTTP {}", resp.status())));
        }
        if resp.content_length().is_some_and(|n| n > self.max_bytes) {
            return Err(self.too_large());
        }
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(|e| MediaError::Io(e.to_string()))?;
        let mut written = 0u64;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| MediaError::Download(e.to_string()))?
        {
            written += chunk.len() as u64;
            if written > self.max_bytes {
                drop(file);
                let _ = tokio::fs::remove_file(path).await;
                return Err(self.too_large());
            }
            file.write_all(&chunk)
                .await
                .map_err(|e| MediaError::Io(e.to_string()))?;
        }
        file.flush()
            .await
            .map_err(|e| MediaError::Io(e.to_string()))?;
        Ok(written)
    }

    fn too_large(&self) -> MediaError {
        MediaError::TooLarge(self.max_bytes)
    }
}
