//! Pixiv artwork extractor via the website ajax API and a PHPSESSID cookie.

use async_trait::async_trait;
use feedbot_core::auth::{
    AuthChallenge, AuthError, AuthInput, AuthMethod, AuthProgress, AuthProvider, AuthSession,
    Credential, CredentialStatus,
};
use feedbot_core::extract::{
    ExtractCtx, ExtractError, ExtractRequest, Extracted, Extractor, HttpMethod, HttpRequest,
    MediaItem, MediaKind, MediaSource, UgoiraFrame,
};
use feedbot_core::options::{OptionDef, OptionDefault, OptionKind, OptionSpec, OptionValue};
use http::header::{COOKIE, REFERER, USER_AGENT};
use http::{HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::sync::OnceLock;
use std::time::Duration;
use url::Url;

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const REFERER_PIXIV: &str = "https://www.pixiv.net/";
const TAG_LIMIT: usize = 20;

const SPEC: OptionSpec = OptionSpec {
    options: &[
        OptionDef {
            name: "q",
            aliases: &["quality"],
            kind: OptionKind::Enum {
                variants: &["original", "regular"],
            },
            default: Some(OptionDefault::Enum("regular")),
            affects_output: true,
            desc: "原图或 master1200。默认 regular，避免超 10MB 被降级成文件",
        },
        OptionDef {
            name: "p",
            aliases: &["pages"],
            kind: OptionKind::IntRange { min: 1, max: 999 },
            default: None,
            affects_output: true,
            desc: "页码区间，例如 +p:1-5（仅 illust/manga）",
        },
        OptionDef {
            name: "sp",
            aliases: &["spoiler"],
            kind: OptionKind::Flag,
            default: None,
            affects_output: false,
            desc: "遮罩 R18 默认开，全年龄用 +sp 打开，-sp 关闭",
        },
    ],
};

pub struct PixivExtractor;

static AUTH: PixivAuth = PixivAuth::new();

#[async_trait]
impl Extractor for PixivExtractor {
    fn id(&self) -> &'static str {
        "pixiv"
    }
    fn display_name(&self) -> &'static str {
        "Pixiv"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["px"]
    }
    fn priority(&self) -> i32 {
        80
    }
    fn matches(&self, url: &Url) -> bool {
        is_pixiv_host(url) && parse_illust_id(url).is_some()
    }
    fn options(&self) -> &'static OptionSpec {
        &SPEC
    }
    fn auth(&self) -> Option<&dyn AuthProvider> {
        Some(&AUTH)
    }
    fn extract_kinds(&self) -> &'static [&'static str] {
        &["插画", "漫画", "动图 ugoira"]
    }
    fn url_examples(&self) -> &'static [&'static str] {
        &["/artworks/{id}", "/i/{id}", "member_illust.php?illust_id="]
    }

    async fn extract(
        &self,
        ctx: &ExtractCtx,
        req: &ExtractRequest,
    ) -> Result<Extracted, ExtractError> {
        let id = parse_illust_id(&req.url).ok_or(ExtractError::Unsupported)?;
        let cookie = cookie_from_req(req);
        let has_cred = cookie.is_some();
        let headers = pixiv_headers(cookie.as_deref())?;

        let detail_url = ajax_url(&format!("illust/{id}"))?;
        let body: IllustBody = ajax_body(ctx, detail_url, headers.clone(), has_cred).await?;

        let title = body
            .illust_title
            .clone()
            .or(body.title.clone())
            .filter(|s| !s.is_empty());
        let author = body.user_name.clone().filter(|s| !s.is_empty());
        let author_url = body.user_id().and_then(pixiv_user_url);
        let description = body
            .illust_comment
            .as_deref()
            .map(html_to_text)
            .filter(|s| !s.is_empty());
        let tags = clip_tags(&tags_from_body(&body));
        let canonical_url = Url::parse(&format!("https://www.pixiv.net/artworks/{id}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?;
        let want_original =
            matches!(req.options.get("q"), Some(OptionValue::Enum(q)) if q == "original");
        let page_range = match req.options.get("p") {
            Some(OptionValue::IntRange { start, end }) => Some((*start, *end)),
            _ => None,
        };

        let items = if body.illust_type() == 2 {
            vec![ugoira_item(ctx, id, headers, has_cred).await?]
        } else {
            illust_items(ctx, id, &body, headers, has_cred, want_original, page_range).await?
        };

        Ok(Extracted {
            extractor_id: self.id().into(),
            canonical_url,
            title,
            author,
            author_url,
            description,
            tags,
            nsfw: body.nsfw(),
            ai: body.ai(),
            engagement: None,
            items,
        })
    }
}

async fn illust_items(
    ctx: &ExtractCtx,
    id: u64,
    body: &IllustBody,
    headers: HeaderMap,
    has_cred: bool,
    want_original: bool,
    page_range: Option<(i64, i64)>,
) -> Result<Vec<MediaItem>, ExtractError> {
    let pages = if body.page_count() > 1 {
        let url = ajax_url(&format!("illust/{id}/pages"))?;
        ajax_body::<Vec<PageItem>>(ctx, url, headers.clone(), has_cred).await?
    } else {
        vec![PageItem {
            urls: body.urls.clone().unwrap_or_default(),
        }]
    };
    let mut items = Vec::with_capacity(pages.len());
    for (idx, page) in pages.into_iter().enumerate() {
        let Some(raw) = pick_url(&page.urls, want_original) else {
            return Err(ExtractError::Failed(format!(
                "第 {} 页缺少图片地址",
                idx + 1
            )));
        };
        let url = Url::parse(raw).map_err(|e| ExtractError::Failed(e.to_string()))?;
        let filename = filename_from_url(&url);
        items.push(MediaItem {
            id: format!("{id}_p{idx}"),
            kind: MediaKind::Photo,
            source: MediaSource::Direct {
                url,
                headers: headers.clone(),
            },
            filename,
            cover: None,
        });
    }
    let items = select_pages(items, page_range);
    if items.is_empty() {
        return Err(ExtractError::Failed("没有符合条件的页".into()));
    }
    Ok(items)
}

async fn ugoira_item(
    ctx: &ExtractCtx,
    id: u64,
    headers: HeaderMap,
    has_cred: bool,
) -> Result<MediaItem, ExtractError> {
    let url = ajax_url(&format!("illust/{id}/ugoira_meta"))?;
    let meta: UgoiraMeta = ajax_body(ctx, url, headers.clone(), has_cred).await?;
    let src = meta
        .original_src
        .as_deref()
        .or(meta.src.as_deref())
        .ok_or_else(|| ExtractError::Failed("缺少 ugoira zip 地址".into()))?;
    let zip = Url::parse(src).map_err(|e| ExtractError::Failed(e.to_string()))?;
    if meta.frames.is_empty() {
        return Err(ExtractError::Failed("ugoira 没有帧".into()));
    }
    let frames = meta
        .frames
        .into_iter()
        .map(|f| UgoiraFrame {
            file: f.file,
            delay_ms: f.delay,
        })
        .collect();
    Ok(MediaItem {
        id: id.to_string(),
        kind: MediaKind::Animation,
        source: MediaSource::UgoiraZip {
            zip,
            headers,
            frames,
        },
        filename: Some(format!("{id}_ugoira.zip")),
        cover: None,
    })
}

async fn ajax_body<T: DeserializeOwned>(
    ctx: &ExtractCtx,
    url: Url,
    headers: HeaderMap,
    has_cred: bool,
) -> Result<T, ExtractError> {
    let resp = ctx
        .http
        .fetch(HttpRequest {
            url,
            headers,
            method: HttpMethod::Get,
        })
        .await?;
    match resp.status {
        401 | 403 => {
            return if has_cred {
                Err(ExtractError::Failed(format!("HTTP {}", resp.status)))
            } else {
                Err(ExtractError::AuthRequired("pixiv".into()))
            };
        }
        404 => return Err(ExtractError::Failed("作品不存在或已删除".into())),
        s if s >= 400 => return Err(ExtractError::Network(format!("HTTP {s}"))),
        _ => {}
    }
    let env: AjaxEnvelope<T> =
        serde_json::from_slice(&resp.body).map_err(|e| ExtractError::Failed(e.to_string()))?;
    if env.error {
        return if has_cred {
            Err(ExtractError::Failed(if env.message.is_empty() {
                "pixiv 返回错误".into()
            } else {
                env.message
            }))
        } else {
            Err(ExtractError::AuthRequired("pixiv".into()))
        };
    }
    env.body
        .ok_or_else(|| ExtractError::Failed("pixiv 响应缺少 body".into()))
}

fn ajax_url(path: &str) -> Result<Url, ExtractError> {
    Url::parse(&format!("https://www.pixiv.net/ajax/{path}"))
        .map_err(|e| ExtractError::Failed(e.to_string()))
}

fn pixiv_headers(cookie: Option<&str>) -> Result<HeaderMap, ExtractError> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(BROWSER_UA));
    headers.insert(REFERER, HeaderValue::from_static(REFERER_PIXIV));
    if let Some(cookie) = cookie {
        let value = HeaderValue::from_str(cookie)
            .map_err(|_| ExtractError::Failed("cookie 含有非法字符".into()))?;
        headers.insert(COOKIE, value);
    }
    Ok(headers)
}

fn cookie_from_req(req: &ExtractRequest) -> Option<String> {
    let raw = std::str::from_utf8(&req.credential.as_ref()?.payload).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(normalize_cookie(trimmed))
    }
}

fn normalize_cookie(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.contains('=') {
        trimmed.to_string()
    } else {
        format!("PHPSESSID={trimmed}")
    }
}

fn is_pixiv_host(url: &Url) -> bool {
    matches!(url.host_str().unwrap_or(""), "pixiv.net" | "www.pixiv.net")
}

fn parse_illust_id(url: &Url) -> Option<u64> {
    if let Some((_, value)) = url.query_pairs().find(|(k, _)| k == "illust_id") {
        return value.parse().ok();
    }
    let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    for pair in segs.windows(2) {
        if pair[0] == "artworks" || pair[0] == "i" {
            return pair[1].parse().ok();
        }
    }
    None
}

fn pick_url(urls: &UrlSet, original: bool) -> Option<&str> {
    if original {
        urls.original.as_deref().or(urls.regular.as_deref())
    } else {
        urls.regular.as_deref().or(urls.original.as_deref())
    }
}

fn filename_from_url(url: &Url) -> Option<String> {
    url.path_segments()?
        .next_back()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn tags_from_body(body: &IllustBody) -> Vec<String> {
    body.tags
        .as_ref()
        .map(|block| block.tags.iter().map(|t| t.tag.clone()).collect())
        .unwrap_or_default()
}

fn clip_tags(tags: &[String]) -> Vec<String> {
    tags.iter()
        .filter(|t| !t.is_empty())
        .take(TAG_LIMIT)
        .cloned()
        .collect()
}

fn pixiv_user_url(user_id: u64) -> Option<Url> {
    Url::parse(&format!("https://www.pixiv.net/users/{user_id}")).ok()
}

fn html_to_text(input: &str) -> String {
    let with_breaks = replace_breaks(input);
    let stripped = strip_tags(&with_breaks);
    decode_entities(&stripped).trim().to_string()
}

fn replace_breaks(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(i) = find_br(rest) {
        out.push_str(&rest[..i]);
        out.push('\n');
        rest = skip_tag(&rest[i..]);
    }
    out.push_str(rest);
    out
}

fn find_br(s: &str) -> Option<usize> {
    let mut rest = s;
    let mut offset = 0usize;
    while let Some(i) = rest.find('<') {
        if is_br_tag(&rest[i..]) {
            return Some(offset + i);
        }
        let skip = 1;
        offset += i + skip;
        rest = &rest[i + skip..];
    }
    None
}

fn is_br_tag(s: &str) -> bool {
    let Some(after) = s.strip_prefix('<') else {
        return false;
    };
    let after = after.trim_start();
    if after.len() < 2 || !after[..2].eq_ignore_ascii_case("br") {
        return false;
    }
    let rest = &after[2..];
    rest.is_empty()
        || rest.starts_with('/')
        || rest.starts_with('>')
        || rest.starts_with(char::is_whitespace)
}

fn skip_tag(s: &str) -> &str {
    match s.find('>') {
        Some(end) => &s[end + 1..],
        None => "",
    }
}

fn strip_tags(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        rest = skip_tag(&rest[i..]);
    }
    out.push_str(rest);
    out
}

fn decode_entities(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some((ch, skip)) = take_entity(after) {
            out.push(ch);
            rest = &after[skip..];
        } else {
            out.push('&');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn take_entity(after: &str) -> Option<(char, usize)> {
    let end = after.find(';')?;
    let body = &after[..end];
    let skip = end + 1;
    let ch = match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" | "#39" => '\'',
        "nbsp" => ' ',
        other => {
            if let Some(hex) = other
                .strip_prefix("#x")
                .or_else(|| other.strip_prefix("#X"))
            {
                char::from_u32(u32::from_str_radix(hex, 16).ok()?)?
            } else {
                let dec = other.strip_prefix('#')?;
                char::from_u32(dec.parse().ok()?)?
            }
        }
    };
    Some((ch, skip))
}

fn select_pages<T>(mut items: Vec<T>, range: Option<(i64, i64)>) -> Vec<T> {
    let Some((start, end)) = range else {
        return items;
    };
    if items.is_empty() || start > end {
        return Vec::new();
    }
    let start = usize::try_from(start.max(1)).unwrap_or(1);
    let end = usize::try_from(end.max(1)).unwrap_or(1);
    if start > items.len() {
        return Vec::new();
    }
    let end = end.min(items.len());
    items.drain(start - 1..end).collect()
}

#[derive(Debug, Deserialize)]
struct AjaxEnvelope<T> {
    #[serde(default)]
    error: bool,
    #[serde(default)]
    message: String,
    body: Option<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IllustBody {
    #[serde(default)]
    illust_title: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    user_name: Option<String>,
    #[serde(default)]
    user_id: Option<serde_json::Value>,
    #[serde(default)]
    illust_comment: Option<String>,
    #[serde(default)]
    x_restrict: Option<serde_json::Value>,
    #[serde(default)]
    ai_type: Option<serde_json::Value>,
    #[serde(default)]
    illust_type: Option<serde_json::Value>,
    #[serde(default)]
    page_count: Option<serde_json::Value>,
    #[serde(default)]
    urls: Option<UrlSet>,
    #[serde(default)]
    tags: Option<TagBlock>,
}

impl IllustBody {
    fn illust_type(&self) -> u8 {
        self.illust_type.as_ref().and_then(value_u64).unwrap_or(0) as u8
    }

    fn page_count(&self) -> u32 {
        self.page_count
            .as_ref()
            .and_then(value_u64)
            .unwrap_or(1)
            .max(1) as u32
    }

    fn user_id(&self) -> Option<u64> {
        self.user_id.as_ref().and_then(value_u64)
    }

    fn nsfw(&self) -> bool {
        self.x_restrict.as_ref().and_then(value_u64).unwrap_or(0) > 0
    }

    fn ai(&self) -> bool {
        self.ai_type.as_ref().and_then(value_u64).unwrap_or(0) == 2
    }
}

fn value_u64(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| value.as_str()?.parse().ok())
}

#[derive(Debug, Clone, Default, Deserialize)]
struct UrlSet {
    #[serde(default)]
    original: Option<String>,
    #[serde(default)]
    regular: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TagBlock {
    #[serde(default)]
    tags: Vec<TagItem>,
}

#[derive(Debug, Deserialize)]
struct TagItem {
    tag: String,
}

#[derive(Debug, Deserialize)]
struct PageItem {
    urls: UrlSet,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UgoiraMeta {
    #[serde(default)]
    original_src: Option<String>,
    #[serde(default)]
    src: Option<String>,
    #[serde(default)]
    frames: Vec<UgoiraFrameJson>,
}

#[derive(Debug, Deserialize)]
struct UgoiraFrameJson {
    file: String,
    delay: u32,
}

pub struct PixivAuth {
    client: OnceLock<reqwest::Client>,
}

impl PixivAuth {
    const fn new() -> Self {
        Self {
            client: OnceLock::new(),
        }
    }

    fn client(&self) -> Result<&reqwest::Client, AuthError> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let built = reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        let _ = self.client.set(built);
        self.client
            .get()
            .ok_or_else(|| AuthError::Failed("初始化 HTTP 客户端失败".into()))
    }

    async fn verify_cookie(&self, cookie: &str) -> Result<bool, AuthError> {
        let client = self.client()?;
        let resp = client
            .get("https://www.pixiv.net/ajax/user/extra")
            .header(USER_AGENT, BROWSER_UA)
            .header(REFERER, REFERER_PIXIV)
            .header(COOKIE, cookie)
            .send()
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        if !resp.status().is_success() {
            return Ok(false);
        }
        let env: AjaxEnvelope<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        Ok(!env.error)
    }
}

const METHODS: &[AuthMethod] = &[AuthMethod::Cookie];

#[async_trait]
impl AuthProvider for PixivAuth {
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
            prompt: "请发送 Pixiv 的 PHPSESSID（浏览器登录 pixiv.net 后从 Cookie 里复制）。含凭据的消息会在处理后删除。".into(),
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
            AuthInput::Poll => Err(AuthError::Failed("pixiv 登录不需要轮询".into())),
            AuthInput::Text(text) => {
                let cookie = normalize_cookie(&text);
                if cookie.len() < 12 {
                    return Err(AuthError::Failed("cookie 太短".into()));
                }
                if !self.verify_cookie(&cookie).await? {
                    return Err(AuthError::Failed("cookie 无效或已过期".into()));
                }
                session.method = Some(AuthMethod::Cookie);
                session.step = 1;
                Ok(AuthProgress::Completed(Credential {
                    extractor_id: "pixiv".into(),
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
        if self.verify_cookie(&cookie).await? {
            Ok(CredentialStatus::Valid)
        } else {
            Ok(CredentialStatus::Invalid)
        }
    }

    async fn refresh(&self, _credential: &Credential) -> Result<Option<Credential>, AuthError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn parse_artworks() {
        assert_eq!(
            parse_illust_id(&url("https://www.pixiv.net/artworks/12345678")),
            Some(12345678)
        );
    }

    #[test]
    fn parse_locale_artworks() {
        assert_eq!(
            parse_illust_id(&url("https://www.pixiv.net/en/artworks/99")),
            Some(99)
        );
        assert_eq!(
            parse_illust_id(&url("https://pixiv.net/zh/artworks/100/")),
            Some(100)
        );
    }

    #[test]
    fn parse_short_i() {
        assert_eq!(
            parse_illust_id(&url("https://www.pixiv.net/i/42")),
            Some(42)
        );
    }

    #[test]
    fn parse_member_illust() {
        assert_eq!(
            parse_illust_id(&url(
                "https://www.pixiv.net/member_illust.php?mode=medium&illust_id=777"
            )),
            Some(777)
        );
    }

    #[test]
    fn parse_invalid() {
        assert_eq!(parse_illust_id(&url("https://www.pixiv.net/users/1")), None);
        assert_eq!(
            parse_illust_id(&url("https://www.pixiv.net/artworks/abc")),
            None
        );
        assert!(!PixivExtractor.matches(&url("https://www.pixiv.net/users/1")));
        assert!(PixivExtractor.matches(&url("https://pixiv.net/artworks/1")));
    }

    #[test]
    fn clip_tags_limits() {
        assert!(clip_tags(&[]).is_empty());
        assert_eq!(
            clip_tags(&["猫".into(), "オリジナル".into()]),
            vec!["猫", "オリジナル"]
        );
        let many: Vec<String> = (0..25).map(|i| format!("t{i}")).collect();
        assert_eq!(clip_tags(&many).len(), TAG_LIMIT);
    }

    #[test]
    fn comment_strips_html() {
        assert_eq!(html_to_text("hello<br />world"), "hello\nworld");
        assert_eq!(html_to_text("A &amp; B <b>C</b>"), "A & B C");
        assert_eq!(html_to_text("  <p>x</p>  "), "x");
        assert!(html_to_text("   ").is_empty());
    }

    #[test]
    fn illust_flags_and_user_url() {
        let nsfw: IllustBody = serde_json::from_value(serde_json::json!({
            "xRestrict": 1,
            "aiType": 2,
            "userId": "42",
        }))
        .unwrap();
        assert!(nsfw.nsfw());
        assert!(nsfw.ai());
        assert_eq!(nsfw.user_id(), Some(42));
        assert_eq!(
            pixiv_user_url(42).unwrap().as_str(),
            "https://www.pixiv.net/users/42"
        );
        let safe: IllustBody = serde_json::from_value(serde_json::json!({
            "xRestrict": 0,
            "aiType": 1,
        }))
        .unwrap();
        assert!(!safe.nsfw());
        assert!(!safe.ai());
    }

    #[test]
    fn page_filter() {
        let items = vec!['a', 'b', 'c', 'd'];
        assert_eq!(select_pages(items.clone(), None), items);
        assert_eq!(select_pages(items.clone(), Some((2, 3))), vec!['b', 'c']);
        assert_eq!(select_pages(items.clone(), Some((1, 99))), items);
        assert!(select_pages(items.clone(), Some((10, 12))).is_empty());
        assert!(select_pages(items, Some((5, 1))).is_empty());
    }

    #[test]
    fn cookie_normalize() {
        assert_eq!(normalize_cookie(" abc123 "), "PHPSESSID=abc123");
        assert_eq!(normalize_cookie("PHPSESSID=abc123"), "PHPSESSID=abc123");
        assert_eq!(
            normalize_cookie("PHPSESSID=abc; other=1"),
            "PHPSESSID=abc; other=1"
        );
    }

    #[test]
    fn spoiler_option_unset_plus_minus() {
        use feedbot_core::options::{merge_layers, OptionValues};
        let spec = PixivExtractor.options();
        assert!(spec.get("sp").is_some());
        let unset = merge_layers(spec, &OptionValues::new(), &[]).unwrap();
        assert!(unset.get("sp").is_none());
        let plus = merge_layers(spec, &OptionValues::new(), &["+sp".into()]).unwrap();
        assert!(plus.flag("sp"));
        let minus = merge_layers(spec, &OptionValues::new(), &["-sp".into()]).unwrap();
        assert_eq!(minus.get("sp"), Some(&OptionValue::Flag(false)));
    }
}
