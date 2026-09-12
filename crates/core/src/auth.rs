//! Platform-agnostic authentication challenges and credentials.

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Display, Formatter};
use std::time::Duration;
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    QrCode,
    Cookie,
    Token,
    UserPass,
}

impl Display for AuthMethod {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QrCode => write!(f, "qr"),
            Self::Cookie => write!(f, "cookie"),
            Self::Token => write!(f, "token"),
            Self::UserPass => write!(f, "userpass"),
        }
    }
}

/// Renderable, platform-agnostic instruction for the bot layer.
#[derive(Clone)]
pub enum AuthChallenge {
    ShowQr {
        png: Bytes,
        expires_at: OffsetDateTime,
        poll_interval: Duration,
    },
    AskText {
        prompt: String,
        secret: bool,
    },
    OpenUrl {
        url: Url,
        hint: String,
    },
    Done,
}

impl Debug for AuthChallenge {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ShowQr {
                expires_at,
                poll_interval,
                png,
            } => f
                .debug_struct("ShowQr")
                .field("png_len", &png.len())
                .field("expires_at", expires_at)
                .field("poll_interval", poll_interval)
                .finish(),
            Self::AskText { prompt, secret } => f
                .debug_struct("AskText")
                .field("prompt", prompt)
                .field("secret", secret)
                .finish(),
            Self::OpenUrl { url, hint } => f
                .debug_struct("OpenUrl")
                .field("url", url)
                .field("hint", hint)
                .finish(),
            Self::Done => write!(f, "Done"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum AuthInput {
    Text(String),
    Poll,
    Cancel,
}

#[derive(Debug, Clone)]
pub enum AuthProgress {
    /// Keep polling; do not resend the QR image.
    Pending,
    /// One-shot status such as "已扫码，请在客户端确认".
    Hint(String),
    Continue(AuthChallenge),
    Completed(Credential),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Valid,
    Expired,
    Invalid,
    Unknown,
}

/// Opaque login session state owned by the platform implementation.
#[derive(Debug, Clone, Default)]
pub struct AuthSession {
    pub extractor_id: String,
    pub method: Option<AuthMethod>,
    pub step: u32,
    /// Platform-private JSON blob. Must never contain secrets in logs.
    pub blob: serde_json::Value,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    pub extractor_id: String,
    pub method: AuthMethod,
    /// Plaintext payload held only in memory after decrypt.
    pub payload: Vec<u8>,
    pub expires_at: Option<OffsetDateTime>,
}

impl Credential {
    pub fn masked_payload(&self) -> &'static str {
        "***"
    }
}

impl Debug for Credential {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("extractor_id", &self.extractor_id)
            .field("method", &self.method)
            .field("payload", &self.masked_payload())
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Display for Credential {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Credential({}/{} payload=***)",
            self.extractor_id, self.method
        )
    }
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("不支持的登录方式")]
    UnsupportedMethod,
    #[error("登录已过期，请重新 /login")]
    SessionExpired,
    #[error("登录失败: {0}")]
    Failed(String),
    #[error("凭据无效")]
    InvalidCredential,
}

#[async_trait::async_trait]
pub trait AuthProvider: Send + Sync {
    fn methods(&self) -> &[AuthMethod];
    async fn begin(
        &self,
        method: AuthMethod,
        session: &mut AuthSession,
    ) -> Result<AuthChallenge, AuthError>;
    async fn advance(
        &self,
        session: &mut AuthSession,
        input: AuthInput,
    ) -> Result<AuthProgress, AuthError>;
    async fn check(&self, credential: &Credential) -> Result<CredentialStatus, AuthError>;
    async fn refresh(&self, credential: &Credential) -> Result<Option<Credential>, AuthError>;
}
