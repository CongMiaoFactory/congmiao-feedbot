//! QR login, credential checks, and cookie helpers.

use super::BilibiliEndpoints;
use async_trait::async_trait;
use feedbot_core::auth::{
    AuthChallenge, AuthError, AuthInput, AuthMethod, AuthProgress, AuthProvider, AuthSession,
    Credential, CredentialStatus,
};
use feedbot_core::extract::{ExtractError, ExtractRequest, HttpClient, HttpRequest};
use feedbot_site_common::render_qr_png;
use http::{HeaderMap, HeaderValue, header};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use url::Url;

use feedbot_site_common::{BROWSER_UA, credential_text, json_str, trim_slash};

const METHODS: &[AuthMethod] = &[AuthMethod::QrCode];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BiliPoll {
    Waiting,
    Scanned,
    Success,
    Expired,
    Other(i64),
}

pub(super) struct BilibiliAuth {
    http: Arc<dyn HttpClient>,
    passport_base: String,
    api_base: String,
}

impl BilibiliAuth {
    pub(super) fn new(endpoints: &BilibiliEndpoints, http: Arc<dyn HttpClient>) -> Self {
        Self {
            http,
            passport_base: trim_slash(&endpoints.passport_base),
            api_base: trim_slash(&endpoints.api_base),
        }
    }

    /// GET a passport/API endpoint, returning response headers and JSON body.
    pub(super) async fn get_json(
        &self,
        url: &str,
        cookie: Option<&str>,
    ) -> Result<(HeaderMap, Value), AuthError> {
        let url = Url::parse(url).map_err(|e| AuthError::Failed(e.to_string()))?;
        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, HeaderValue::from_static(BROWSER_UA));
        if let Some(cookie) = cookie {
            let value = HeaderValue::from_str(cookie)
                .map_err(|_| AuthError::Failed("cookie 含有非法字符".into()))?;
            headers.insert(header::COOKIE, value);
        }
        let resp = self
            .http
            .fetch(HttpRequest::get(url, headers))
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        if !(200..300).contains(&resp.status) {
            return Err(AuthError::Failed(format!("HTTP {}", resp.status)));
        }
        let body =
            serde_json::from_slice(&resp.body).map_err(|e| AuthError::Failed(e.to_string()))?;
        Ok((resp.headers, body))
    }
}

#[async_trait]
impl AuthProvider for BilibiliAuth {
    fn methods(&self) -> &[AuthMethod] {
        METHODS
    }

    async fn begin(
        &self,
        method: AuthMethod,
        session: &mut AuthSession,
    ) -> Result<AuthChallenge, AuthError> {
        if method != AuthMethod::QrCode {
            return Err(AuthError::UnsupportedMethod);
        }
        let (_, root) = self
            .get_json(
                &format!(
                    "{}/x/passport-login/web/qrcode/generate",
                    self.passport_base
                ),
                None,
            )
            .await?;
        if root.get("code").and_then(Value::as_i64).unwrap_or_default() != 0 {
            return Err(AuthError::Failed(
                json_str(&root, "/message")
                    .unwrap_or("Bilibili 登录失败")
                    .into(),
            ));
        }
        let key = json_str(&root, "/data/qrcode_key")
            .ok_or_else(|| AuthError::Failed("响应缺少 qrcode_key".into()))?;
        let url = json_str(&root, "/data/url")
            .ok_or_else(|| AuthError::Failed("响应缺少二维码地址".into()))?;
        session.method = Some(AuthMethod::QrCode);
        session.step = 0;
        session.blob = serde_json::json!({ "qrcode_key": key });
        Ok(AuthChallenge::ShowQr {
            png: render_qr_png(url)?,
            expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(180),
            poll_interval: Duration::from_secs(3),
        })
    }

    async fn advance(
        &self,
        session: &mut AuthSession,
        input: AuthInput,
    ) -> Result<AuthProgress, AuthError> {
        match input {
            AuthInput::Cancel => Err(AuthError::Failed("已取消".into())),
            AuthInput::Text(_) => Err(AuthError::Failed("请扫码登录".into())),
            AuthInput::Poll => {
                let key = session
                    .blob
                    .get("qrcode_key")
                    .and_then(Value::as_str)
                    .ok_or(AuthError::SessionExpired)?
                    .to_string();
                let mut poll_url = Url::parse(&format!(
                    "{}/x/passport-login/web/qrcode/poll",
                    self.passport_base
                ))
                .map_err(|e| AuthError::Failed(e.to_string()))?;
                poll_url.query_pairs_mut().append_pair("qrcode_key", &key);
                let (headers, root) = self.get_json(poll_url.as_str(), None).await?;
                let cookie = cookies_from_set_cookie(&headers);
                if root.get("code").and_then(Value::as_i64).unwrap_or_default() != 0 {
                    return Err(AuthError::Failed(
                        json_str(&root, "/message")
                            .unwrap_or("Bilibili 登录失败")
                            .into(),
                    ));
                }
                let code = root.pointer("/data/code").and_then(Value::as_i64);
                match bili_poll_code(code) {
                    BiliPoll::Waiting => Ok(AuthProgress::Pending),
                    BiliPoll::Scanned => {
                        if session.step == 0 {
                            session.step = 1;
                            Ok(AuthProgress::Hint("已扫码，请在客户端确认".into()))
                        } else {
                            Ok(AuthProgress::Pending)
                        }
                    }
                    BiliPoll::Expired => Err(AuthError::SessionExpired),
                    BiliPoll::Success => {
                        if cookie.is_empty() {
                            return Err(AuthError::Failed(
                                "Bilibili 登录成功但响应中没有 Cookie".into(),
                            ));
                        }
                        if !cookie_has_value(&cookie, "SESSDATA") {
                            return Err(AuthError::Failed("Cookie 缺少 SESSDATA".into()));
                        }
                        Ok(AuthProgress::Completed(Credential {
                            extractor_id: "bilibili".into(),
                            method: AuthMethod::QrCode,
                            payload: cookie.into_bytes(),
                            expires_at: None,
                        }))
                    }
                    BiliPoll::Other(code) => Err(AuthError::Failed(format!(
                        "Bilibili 扫码状态 {code}: {}",
                        json_str(&root, "/data/message").unwrap_or("未知错误")
                    ))),
                }
            }
        }
    }

    async fn check(&self, credential: &Credential) -> Result<CredentialStatus, AuthError> {
        let Ok(raw) = std::str::from_utf8(&credential.payload) else {
            return Ok(CredentialStatus::Invalid);
        };
        let cookie = raw.trim();
        if cookie.is_empty() || !cookie_has_value(cookie, "SESSDATA") {
            return Ok(CredentialStatus::Invalid);
        }
        let (_, root) = self
            .get_json(
                &format!("{}/x/web-interface/nav", self.api_base),
                Some(cookie),
            )
            .await?;
        let code = root.get("code").and_then(Value::as_i64).unwrap_or(-1);
        let logged_in = root
            .pointer("/data/isLogin")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if code == 0 && logged_in {
            Ok(CredentialStatus::Valid)
        } else {
            Ok(CredentialStatus::Invalid)
        }
    }

    async fn refresh(&self, _credential: &Credential) -> Result<Option<Credential>, AuthError> {
        Ok(None)
    }
}

pub(crate) fn bili_poll_code(code: Option<i64>) -> BiliPoll {
    match code {
        Some(0) => BiliPoll::Success,
        Some(86038) => BiliPoll::Expired,
        Some(86090) => BiliPoll::Scanned,
        Some(86101) => BiliPoll::Waiting,
        Some(other) => BiliPoll::Other(other),
        None => BiliPoll::Other(-1),
    }
}

pub(crate) fn cookies_from_set_cookie(headers: &HeaderMap) -> String {
    let pairs = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .filter(|(name, _)| !name.is_empty())
        .collect::<BTreeMap<_, _>>();
    join_pairs(&pairs)
}

pub(super) fn map_bili_code(code: i64, value: Value) -> Result<Value, ExtractError> {
    if code == 0 {
        return Ok(value);
    }
    let message = json_str(&value, "/message").unwrap_or("未知错误");
    Err(match code {
        -101 => ExtractError::AuthRequired("bilibili".into()),
        -352 | 412 => ExtractError::Failed(format!(
            "Bilibili 风控校验失败（{message}），请稍后重试或使用 /login bili 更新登录状态"
        )),
        404 | 4101139 => ExtractError::Failed(format!("Bilibili: {message}")),
        _ => ExtractError::Failed(format!("Bilibili: {message}")),
    })
}

pub(super) fn cookie_from_req(req: &ExtractRequest) -> Option<String> {
    credential_text(req).map(str::to_string)
}

pub(crate) fn merge_cookies(account: Option<&str>, device: Option<&str>) -> Option<String> {
    let mut pairs = parse_cookie_pairs(account.unwrap_or(""));
    for (k, v) in parse_cookie_pairs(device.unwrap_or("")) {
        pairs.entry(k).or_insert(v);
    }
    if pairs.is_empty() {
        None
    } else {
        Some(join_pairs(&pairs))
    }
}

pub(super) fn parse_cookie_pairs(cookie: &str) -> BTreeMap<String, String> {
    cookie
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

pub(super) fn join_pairs(pairs: &BTreeMap<String, String>) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

pub(super) fn cookie_has_value(cookie: &str, name: &str) -> bool {
    parse_cookie_pairs(cookie)
        .get(name)
        .is_some_and(|v| !v.is_empty())
}
