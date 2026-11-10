//! Cookie login: normalization of pasted cookies and validation.

use super::XQueryIds;
use super::graphql::{api_headers, graphql_auth_rejected, graphql_url};
use async_trait::async_trait;
use feedbot_core::auth::{
    AuthChallenge, AuthError, AuthInput, AuthMethod, AuthProgress, AuthProvider, AuthSession,
    Credential, CredentialStatus,
};
use feedbot_core::extract::{ExtractRequest, HttpClient, HttpRequest};
use serde_json::Value;
use std::sync::Arc;

use feedbot_site_common::{credential_text, strip_cookie_header};

const GQL_USER_VARS: &str = r#"{"screen_name":"X","withGrokTranslatedBio":false}"#;
pub(super) const GQL_USER_FEATURES: &str = r#"{"hidden_profile_subscriptions_enabled":true,"payments_enabled":false,"rweb_xchat_enabled":false,"profile_label_improvements_pcf_label_in_post_enabled":true,"rweb_tipjar_consumption_enabled":true,"verified_phone_label_enabled":false,"highlights_tweets_tab_ui_enabled":true,"responsive_web_twitter_article_notes_tab_enabled":true,"subscriptions_feature_can_gift_premium":true,"creator_subscriptions_tweet_preview_api_enabled":true,"responsive_web_graphql_skip_user_profile_image_extensions_enabled":false,"responsive_web_graphql_timeline_navigation_enabled":true,"subscriptions_verification_info_is_identity_verified_enabled":true,"subscriptions_verification_info_verified_since_enabled":true}"#;
pub(super) const GQL_USER_TOGGLES: &str = r#"{"withAuxiliaryUserLabels":true}"#;

pub struct XAuth {
    pub(super) http: Arc<dyn HttpClient>,
    pub(super) query_ids: XQueryIds,
}

impl XAuth {
    pub(super) async fn verify_cookie(&self, cookie: &str) -> Result<(), AuthError> {
        if cookie_value(cookie, "auth_token").is_none_or(|v| v.len() < 8) {
            return Err(AuthError::Failed(
                "cookie 缺少 auth_token 或太短。请复制 Value，不是名称。".into(),
            ));
        }
        if cookie_value(cookie, "ct0").is_none_or(|v| v.len() < 8) {
            return Err(AuthError::Failed(
                "cookie 缺少 ct0。请同时粘贴 auth_token 和 ct0，例如：\nauth_token=xxxx; ct0=yyyy"
                    .into(),
            ));
        }
        let headers = api_headers(Some(cookie))
            .map_err(|_| AuthError::Failed("cookie 含有非法字符".into()))?;
        let url = graphql_url(
            &self.query_ids.user_by_screen_name,
            "UserByScreenName",
            GQL_USER_VARS,
            GQL_USER_FEATURES,
            Some(GQL_USER_TOGGLES),
        )
        .map_err(|e| AuthError::Failed(e.to_string()))?;
        // Redirects mean "go log in", so they must not be followed.
        let resp = self
            .http
            .fetch(HttpRequest::get(url, headers).without_redirects())
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        let status = resp.status;
        if (300..400).contains(&status) || status == 401 {
            return Err(AuthError::Failed(format!(
                "cookie 无效或已过期 (HTTP {status})"
            )));
        }
        let body = String::from_utf8_lossy(&resp.body);
        if status == 403 {
            if graphql_auth_rejected(&serde_json::from_str(&body).unwrap_or(Value::Null))
                || body.to_ascii_lowercase().contains("bad authentication")
            {
                return Err(AuthError::Failed(format!(
                    "cookie 无效或已过期 (HTTP {status})"
                )));
            }
            tracing::warn!(
                status,
                "x cookie probe returned 403; accepting structurally valid cookies"
            );
            return Ok(());
        }
        if !(200..300).contains(&status) {
            tracing::warn!(status, "x cookie probe inconclusive; accepting cookies");
            return Ok(());
        }
        if let Ok(json) = serde_json::from_str::<Value>(&body)
            && graphql_auth_rejected(&json)
        {
            return Err(AuthError::Failed("cookie 无效或已过期".into()));
        }
        Ok(())
    }
}

const METHODS: &[AuthMethod] = &[AuthMethod::Cookie];

#[async_trait]
impl AuthProvider for XAuth {
    fn methods(&self) -> &[AuthMethod] {
        METHODS
    }

    async fn begin(
        &self,
        method: AuthMethod,
        _session: &mut AuthSession,
    ) -> Result<AuthChallenge, AuthError> {
        if method != AuthMethod::Cookie {
            return Err(AuthError::UnsupportedMethod);
        }
        Ok(AuthChallenge::AskText {
            prompt: "请发送 Cookie，必须同时包含 auth_token 和 ct0（复制的是 Value，不是名称）。\n\n推荐一行：\nauth_token=xxxx; ct0=yyyy\n\n两行也可以：\nauth_token=xxxx\nct0=yyyy\n\n在已登录 x.com 的浏览器：F12 → Application → Cookies → https://x.com。含凭据的消息会在处理后删除。".into(),
            secret: true,
        })
    }

    async fn advance(
        &self,
        session: &mut AuthSession,
        input: AuthInput,
    ) -> Result<AuthProgress, AuthError> {
        match input {
            AuthInput::Cancel => Err(AuthError::Failed("已取消".into())),
            AuthInput::Poll => Err(AuthError::Failed("X 登录不需要轮询".into())),
            AuthInput::Text(text) => {
                let cookie = normalize_cookie(&text);
                self.verify_cookie(&cookie).await?;
                session.method = Some(AuthMethod::Cookie);
                session.step = 1;
                Ok(AuthProgress::Completed(Credential {
                    extractor_id: "x".into(),
                    method: AuthMethod::Cookie,
                    payload: cookie.into_bytes(),
                    expires_at: None,
                }))
            }
        }
    }

    async fn check(&self, credential: &Credential) -> Result<CredentialStatus, AuthError> {
        let Ok(raw) = std::str::from_utf8(&credential.payload) else {
            return Ok(CredentialStatus::Invalid);
        };
        let cookie = normalize_cookie(raw);
        match self.verify_cookie(&cookie).await {
            Ok(()) => Ok(CredentialStatus::Valid),
            Err(AuthError::Failed(msg))
                if msg.contains("无效") || msg.contains("缺少") || msg.contains("非法") =>
            {
                Ok(CredentialStatus::Invalid)
            }
            Err(e) => Err(e),
        }
    }

    async fn refresh(&self, _credential: &Credential) -> Result<Option<Credential>, AuthError> {
        Ok(None)
    }
}

pub(super) fn cookie_from_req(req: &ExtractRequest) -> Option<String> {
    credential_text(req).map(normalize_cookie)
}

pub(super) fn normalize_cookie(raw: &str) -> String {
    let s = strip_cookie_header(raw);
    if s.is_empty() {
        return String::new();
    }

    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut unlabeled: Vec<String> = Vec::new();
    for line in s.split(['\n', '\r', ';']) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains('\t') {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() >= 2 {
                let name = cols[0].trim();
                let value = strip_wrap_quotes(cols[1].trim());
                if looks_like_cookie_name(name) && !value.is_empty() {
                    upsert_pair(&mut pairs, name, value);
                    continue;
                }
            }
        }
        if let Some((k, v)) = split_cookie_kv(line) {
            upsert_pair(&mut pairs, k, v);
            continue;
        }
        for token in line.split_whitespace() {
            let token = strip_wrap_quotes(token);
            if !token.is_empty() {
                unlabeled.push(token.to_string());
            }
        }
    }

    if !has_pair(&pairs, "auth_token") {
        if let Some(v) = unlabeled.first() {
            pairs.insert(0, ("auth_token".into(), v.clone()));
            if unlabeled.len() >= 2 && !has_pair(&pairs, "ct0") {
                pairs.push(("ct0".into(), unlabeled[1].clone()));
            }
        }
    } else if !has_pair(&pairs, "ct0")
        && let Some(v) = unlabeled.first()
    {
        pairs.push(("ct0".into(), v.clone()));
    }

    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

pub(super) fn strip_wrap_quotes(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, '"' | '\'' | '“' | '”'))
}

pub(super) fn looks_like_cookie_name(k: &str) -> bool {
    !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub(super) fn split_cookie_kv(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once('=').or_else(|| {
        line.split_once(':')
            .filter(|(k, _)| looks_like_cookie_name(k.trim()))
    })?;
    let k = k.trim();
    let v = strip_wrap_quotes(v.trim());
    if looks_like_cookie_name(k) && !v.is_empty() {
        Some((k, v))
    } else {
        None
    }
}

pub(super) fn has_pair(pairs: &[(String, String)], name: &str) -> bool {
    pairs.iter().any(|(k, _)| k == name)
}

pub(super) fn upsert_pair(pairs: &mut Vec<(String, String)>, k: &str, v: &str) {
    if let Some(existing) = pairs.iter_mut().find(|(ek, _)| ek == k) {
        existing.1 = v.to_string();
    } else {
        pairs.push((k.to_string(), v.to_string()));
    }
}

pub(super) fn cookie_value<'a>(cookie: &'a str, name: &str) -> Option<&'a str> {
    for part in cookie.split(';') {
        let part = part.trim();
        let (k, v) = part.split_once('=')?;
        if k.trim() == name {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}
