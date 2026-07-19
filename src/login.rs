use std::{collections::BTreeMap, io::Cursor, time::SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use image::{DynamicImage, ImageFormat, Luma};
use qrcode::QrCode;
use reqwest::{Client, header::SET_COOKIE};
use serde_json::Value;

use crate::{Config, RuntimeCredentials};

#[derive(Debug)]
pub struct LoginChallenge {
    pub key: String,
    pub image: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginPoll {
    Waiting,
    Scanned,
    Success,
    Expired,
}

#[derive(Clone)]
pub struct LoginService {
    client: Client,
    credentials: RuntimeCredentials,
    bilibili_passport_base: String,
    netease_api_base: String,
}

impl LoginService {
    pub fn new(config: &Config, credentials: RuntimeCredentials) -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!(
                "Mozilla/5.0 (CongmiaoFeedBot/",
                env!("CARGO_PKG_VERSION"),
                " QR Login)"
            ))
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            credentials,
            bilibili_passport_base: config.bilibili_passport_base.trim_end_matches('/').into(),
            netease_api_base: config.netease_api_base.trim_end_matches('/').into(),
        })
    }

    pub async fn bilibili_challenge(&self) -> Result<LoginChallenge> {
        let root: Value = self
            .client
            .get(format!(
                "{}/x/passport-login/web/qrcode/generate",
                self.bilibili_passport_base
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure_api_success(&root, "Bilibili")?;
        let key = value_str(&root, "/data/qrcode_key")?;
        let url = value_str(&root, "/data/url")?;
        Ok(LoginChallenge {
            key: key.to_string(),
            image: render_qr(url)?,
        })
    }

    pub async fn poll_bilibili(&self, key: &str) -> Result<LoginPoll> {
        let response = self
            .client
            .get(format!(
                "{}/x/passport-login/web/qrcode/poll",
                self.bilibili_passport_base
            ))
            .query(&[("qrcode_key", key)])
            .send()
            .await?
            .error_for_status()?;
        let cookies = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|value| value.split(';').next())
            .filter_map(|pair| pair.split_once('='))
            .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
            .collect::<BTreeMap<_, _>>();
        let root: Value = response.json().await?;
        ensure_api_success(&root, "Bilibili")?;
        match root.pointer("/data/code").and_then(Value::as_i64) {
            Some(0) => {
                if cookies.is_empty() {
                    bail!("Bilibili 登录成功但响应中没有 Cookie");
                }
                let cookie = cookies
                    .into_iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                self.credentials.set_bilibili(cookie).await?;
                Ok(LoginPoll::Success)
            }
            Some(86038) => Ok(LoginPoll::Expired),
            Some(86090) => Ok(LoginPoll::Scanned),
            Some(86101) => Ok(LoginPoll::Waiting),
            Some(code) => Err(anyhow!(
                "Bilibili 扫码状态 {code}: {}",
                root.pointer("/data/message")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误")
            )),
            None => bail!("Bilibili 扫码响应缺少状态码"),
        }
    }

    pub async fn netease_challenge(&self) -> Result<LoginChallenge> {
        let timestamp = timestamp_ms();
        let root: Value = self
            .client
            .get(format!("{}/login/qr/key", self.netease_api_base))
            .query(&[("timestamp", timestamp.as_str())])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let key = value_str(&root, "/data/unikey")?.to_string();
        let root: Value = self
            .client
            .get(format!("{}/login/qr/create", self.netease_api_base))
            .query(&[
                ("key", key.as_str()),
                ("qrimg", "true"),
                ("timestamp", timestamp.as_str()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let url = value_str(&root, "/data/qrurl")?;
        Ok(LoginChallenge {
            key,
            image: render_qr(url)?,
        })
    }

    pub async fn poll_netease(&self, key: &str) -> Result<LoginPoll> {
        let timestamp = timestamp_ms();
        let mut response = self
            .client
            .get(format!("{}/login/qr/check", self.netease_api_base))
            .query(&[("key", key), ("timestamp", timestamp.as_str())])
            .send()
            .await?;
        // api-enhanced 文档说明：部分账号扫码后会在默认检查上返回 502，
        // 此时应改用 noCookie 模式完成授权。
        if response.status().as_u16() == 502 {
            response = self
                .client
                .get(format!("{}/login/qr/check", self.netease_api_base))
                .query(&[
                    ("key", key),
                    ("timestamp", timestamp.as_str()),
                    ("noCookie", "true"),
                ])
                .send()
                .await?;
        }
        let root: Value = response.error_for_status()?.json().await?;
        match root.get("code").and_then(Value::as_i64) {
            Some(800) => Ok(LoginPoll::Expired),
            Some(801) => Ok(LoginPoll::Waiting),
            Some(802) => Ok(LoginPoll::Scanned),
            Some(803) => {
                let cookie = root
                    .get("cookie")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .context("网易云扫码成功但响应中没有 Cookie")?;
                self.credentials.set_netease(cookie.to_string()).await?;
                Ok(LoginPoll::Success)
            }
            Some(code) => Err(anyhow!(
                "网易云扫码状态 {code}: {}",
                root.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误")
            )),
            None => bail!("网易云扫码响应缺少状态码"),
        }
    }
}

fn ensure_api_success(root: &Value, name: &str) -> Result<()> {
    if root.get("code").and_then(Value::as_i64).unwrap_or_default() != 0 {
        bail!(
            "{name} API: {}",
            root.get("message")
                .and_then(Value::as_str)
                .unwrap_or("未知错误")
        );
    }
    Ok(())
}

fn value_str<'a>(root: &'a Value, pointer: &str) -> Result<&'a str> {
    root.pointer(pointer)
        .and_then(Value::as_str)
        .with_context(|| format!("响应缺少字段 {pointer}"))
}

fn timestamp_ms() -> String {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

fn render_qr(content: &str) -> Result<Vec<u8>> {
    let code = QrCode::new(content.as_bytes()).context("生成二维码失败")?;
    let image = code
        .render::<Luma<u8>>()
        .min_dimensions(512, 512)
        .quiet_zone(true)
        .build();
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageLuma8(image).write_to(&mut output, ImageFormat::Png)?;
    Ok(output.into_inner())
}
