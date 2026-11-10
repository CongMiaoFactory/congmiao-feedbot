//! Pixiv artwork extractor via the website ajax API and a PHPSESSID cookie.

use async_trait::async_trait;
use feedbot_core::auth::{
    AuthChallenge, AuthError, AuthInput, AuthMethod, AuthProgress, AuthProvider, AuthSession,
    Credential, CredentialStatus,
};
use feedbot_core::extract::{
    ExtractCtx, ExtractError, ExtractRequest, Extracted, Extractor, FeedEntry, HttpClient,
    HttpRequest, MediaItem, MediaKind, MediaSource, UgoiraFrame,
};
use feedbot_core::options::{OptionDef, OptionDefault, OptionKind, OptionSpec, OptionValue};
use feedbot_core::{Present, SiteMeta};
use http::header::{COOKIE, REFERER, USER_AGENT};
use http::{HeaderMap, HeaderValue};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

use feedbot_site_common::{
    BROWSER_UA, credential_text, last_path_segment, rewrite_host, select_pages,
};

const REFERER_PIXIV: &str = "https://www.pixiv.net/";
const TAG_LIMIT: usize = 20;
/// Newest works considered per subscription poll.
const FEED_LIMIT: usize = 30;

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
            desc: "页码或区间，例如 +p:3、+p:1-5（仅 illust/manga）",
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

pub struct PixivExtractor {
    auth: PixivAuth,
}

impl PixivExtractor {
    pub fn new(http: Arc<dyn HttpClient>) -> Self {
        Self {
            auth: PixivAuth { http },
        }
    }
}

const META: SiteMeta = SiteMeta {
    id: "pixiv",
    name: "Pixiv",
    aliases: &["px"],
    priority: 80,
    kinds: &["插画", "漫画", "动图 ugoira"],
    examples: &["/artworks/{id}", "/i/{id}", "member_illust.php?illust_id="],
    feed_examples: &["pixiv.net/users/{id}"],
};

#[async_trait]
impl Extractor for PixivExtractor {
    fn meta(&self) -> SiteMeta {
        META
    }
    fn matches(&self, url: &Url) -> bool {
        is_pixiv_host(url) && parse_illust_id(url).is_some()
    }
    fn rewrite_url(&self, url: &Url) -> Option<Url> {
        rewrite_host(url, &["www.pixiv.net"], "pixiv.net")
    }
    fn present(&self) -> Present {
        Present::Gallery
    }
    fn options(&self) -> &'static OptionSpec {
        &SPEC
    }
    fn auth(&self) -> Option<&dyn AuthProvider> {
        Some(&self.auth)
    }
    fn feed_matches(&self, url: &Url) -> bool {
        is_pixiv_host(url) && parse_user_id(url).is_some()
    }

    async fn feed(
        &self,
        ctx: &ExtractCtx,
        req: &ExtractRequest,
    ) -> Result<Vec<FeedEntry>, ExtractError> {
        let user = parse_user_id(&req.url).ok_or(ExtractError::Unsupported)?;
        let cookie = cookie_from_req(req);
        let headers = pixiv_headers(cookie.as_deref())?;
        let url = ajax_url(&format!("user/{user}/profile/all"))?;
        let body: ProfileAll = ajax_body(ctx, url, headers, cookie.is_some()).await?;
        let mut ids: Vec<u64> = body
            .illusts
            .keys()
            .chain(body.manga.keys())
            .filter_map(|k| k.parse().ok())
            .collect();
        // Pixiv ids are monotonic, so the largest ids are the newest works.
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids.dedup();
        ids.truncate(FEED_LIMIT);
        ids.into_iter()
            .map(|id| {
                Ok(FeedEntry {
                    key: id.to_string(),
                    url: feedbot_site_common::parse_url(&format!(
                        "https://www.pixiv.net/artworks/{id}"
                    ))?,
                })
            })
            .collect()
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
        let filename = last_path_segment(&url);
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
    let resp = ctx.http.fetch(HttpRequest::get(url, headers)).await?;
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
    credential_text(req).map(normalize_cookie)
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

/// `/users/{id}`, `/en/users/{id}`, `member.php?id={id}`.
fn parse_user_id(url: &Url) -> Option<u64> {
    let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segs.last() == Some(&"member.php") {
        return url
            .query_pairs()
            .find(|(k, _)| k == "id")
            .and_then(|(_, v)| v.parse().ok());
    }
    segs.windows(2)
        .find(|pair| pair[0] == "users")
        .and_then(|pair| pair[1].parse().ok())
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

/// `ajax/user/{id}/profile/all` body. Both maps are `{ "<id>": null }`, or
/// `[]` when the user has no works of that type.
#[derive(Debug, Deserialize)]
struct ProfileAll {
    #[serde(default, deserialize_with = "id_map")]
    illusts: HashMap<String, serde_json::Value>,
    #[serde(default, deserialize_with = "id_map")]
    manga: HashMap<String, serde_json::Value>,
}

fn id_map<'de, D>(d: D) -> Result<HashMap<String, serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => HashMap::new(),
    })
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
    http: Arc<dyn HttpClient>,
}

impl PixivAuth {
    async fn verify_cookie(&self, cookie: &str) -> Result<bool, AuthError> {
        let headers = pixiv_headers(Some(cookie)).map_err(|e| AuthError::Failed(e.to_string()))?;
        let url = ajax_url("user/extra").map_err(|e| AuthError::Failed(e.to_string()))?;
        let resp = self
            .http
            .fetch(HttpRequest::get(url, headers))
            .await
            .map_err(|e| AuthError::Failed(e.to_string()))?;
        if !(200..300).contains(&resp.status) {
            return Ok(false);
        }
        let env: AjaxEnvelope<serde_json::Value> =
            serde_json::from_slice(&resp.body).map_err(|e| AuthError::Failed(e.to_string()))?;
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
        assert!(
            !PixivExtractor::new(feedbot_site_common::testing::no_http())
                .matches(&url("https://www.pixiv.net/users/1"))
        );
        assert!(
            PixivExtractor::new(feedbot_site_common::testing::no_http())
                .matches(&url("https://pixiv.net/artworks/1"))
        );
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
        use feedbot_core::options::{OptionValues, merge_layers};
        let spec = PixivExtractor::new(feedbot_site_common::testing::no_http()).options();
        assert!(spec.get("sp").is_some());
        let unset = merge_layers(spec, &OptionValues::new(), &[]).unwrap();
        assert!(unset.get("sp").is_none());
        let plus = merge_layers(spec, &OptionValues::new(), &["+sp".into()]).unwrap();
        assert!(plus.flag("sp"));
        let minus = merge_layers(spec, &OptionValues::new(), &["-sp".into()]).unwrap();
        assert_eq!(minus.get("sp"), Some(&OptionValue::Flag(false)));
    }

    mod flow {
        use super::*;
        use feedbot_core::options::OptionValues;
        use feedbot_site_common::testing::{FakeHttp, request};
        use serde_json::{Value, json};

        const DETAIL: &str = "www.pixiv.net/ajax/illust/7";
        const PAGES: &str = "www.pixiv.net/ajax/illust/7/pages";

        fn ok(body: Value) -> Value {
            json!({ "error": false, "message": "", "body": body })
        }

        fn page(n: u32) -> Value {
            json!({ "urls": {
                "original": format!("https://i.pximg.net/img-original/7_p{n}.png"),
                "regular": format!("https://i.pximg.net/img-master/7_p{n}_master1200.jpg")
            } })
        }

        fn options(ext: &PixivExtractor, tokens: &[&str]) -> OptionValues {
            let tokens: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
            ext.resolve_options(&OptionValues::new(), &tokens).unwrap()
        }

        #[tokio::test]
        async fn manga_pages_with_range_and_original() {
            let http = Arc::new(
                FakeHttp::new()
                    .json(
                        DETAIL,
                        ok(json!({
                            "illustTitle": "標題", "userName": "作者", "userId": "9",
                            "illustComment": "a<br />b", "xRestrict": 1, "aiType": 2,
                            "illustType": 1, "pageCount": 3,
                            "tags": { "tags": [{ "tag": "原创" }] }
                        })),
                    )
                    .json(PAGES, ok(json!([page(0), page(1), page(2)]))),
            );
            let ext = PixivExtractor::new(http.clone());
            let req = request(
                "pixiv",
                "https://www.pixiv.net/artworks/7",
                options(&ext, &["+p:2-3", "+q:original"]),
                Some("abc"),
            );
            let extracted = ext.extract(&http.ctx(), &req).await.unwrap();

            assert_eq!(extracted.title.as_deref(), Some("標題"));
            assert_eq!(extracted.description.as_deref(), Some("a\nb"));
            assert_eq!(extracted.tags, ["原创"]);
            assert!(extracted.nsfw && extracted.ai);
            assert_eq!(
                extracted.author_url.unwrap().as_str(),
                "https://www.pixiv.net/users/9"
            );
            let ids: Vec<&str> = extracted.items.iter().map(|i| i.id.as_str()).collect();
            assert_eq!(ids, ["7_p1", "7_p2"]);
            let MediaSource::Direct { url, headers } = &extracted.items[0].source else {
                panic!("{:?}", extracted.items[0].source);
            };
            assert_eq!(url.as_str(), "https://i.pximg.net/img-original/7_p1.png");
            assert_eq!(headers[REFERER], REFERER_PIXIV);
            assert_eq!(http.last(DETAIL).headers[COOKIE], "PHPSESSID=abc");
        }

        #[tokio::test]
        async fn single_page_uses_detail_urls() {
            let http = Arc::new(FakeHttp::new().json(
                DETAIL,
                ok(json!({ "illustTitle": "t", "pageCount": 1, "urls": page(0)["urls"] })),
            ));
            let ext = PixivExtractor::new(http.clone());
            let req = request(
                "pixiv",
                "https://www.pixiv.net/i/7",
                options(&ext, &[]),
                None,
            );
            let extracted = ext.extract(&http.ctx(), &req).await.unwrap();
            let MediaSource::Direct { url, .. } = &extracted.items[0].source else {
                panic!();
            };
            assert!(url.as_str().ends_with("7_p0_master1200.jpg"));
            assert_eq!(http.urls().len(), 1);
        }

        #[tokio::test]
        async fn ugoira_frames() {
            let http = Arc::new(
                FakeHttp::new()
                    .json(DETAIL, ok(json!({ "illustType": 2 })))
                    .json(
                        "www.pixiv.net/ajax/illust/7/ugoira_meta",
                        ok(json!({
                            "src": "https://i.pximg.net/img-zip-ugoira/7_600x600.zip",
                            "originalSrc": "https://i.pximg.net/img-zip-ugoira/7_1920x1080.zip",
                            "frames": [{ "file": "000000.jpg", "delay": 80 }]
                        })),
                    ),
            );
            let ext = PixivExtractor::new(http.clone());
            let req = request(
                "pixiv",
                "https://www.pixiv.net/artworks/7",
                options(&ext, &[]),
                None,
            );
            let item = &ext.extract(&http.ctx(), &req).await.unwrap().items[0];
            assert_eq!(item.kind, MediaKind::Animation);
            let MediaSource::UgoiraZip { zip, frames, .. } = &item.source else {
                panic!("{:?}", item.source);
            };
            assert!(zip.as_str().ends_with("7_1920x1080.zip"));
            assert_eq!(frames[0].delay_ms, 80);
        }

        #[tokio::test]
        async fn rejected_without_login_asks_for_login() {
            let rejected = json!({ "error": true, "message": "尚无权限浏览该作品", "body": [] });
            for (status, body) in [(403, json!({})), (200, rejected)] {
                let http = Arc::new(FakeHttp::new().status(DETAIL, status, body.to_string()));
                let ext = PixivExtractor::new(http.clone());
                let url = "https://www.pixiv.net/artworks/7";
                let anon = request("pixiv", url, options(&ext, &[]), None);
                let err = ext.extract(&http.ctx(), &anon).await.unwrap_err();
                assert!(matches!(err, ExtractError::AuthRequired(_)), "{err:?}");
                let logged_in = request("pixiv", url, options(&ext, &[]), Some("abc"));
                let err = ext.extract(&http.ctx(), &logged_in).await.unwrap_err();
                assert!(matches!(err, ExtractError::Failed(_)), "{err:?}");
            }
            let http = Arc::new(FakeHttp::new().status(DETAIL, 404, "{}"));
            let ext = PixivExtractor::new(http.clone());
            let req = request(
                "pixiv",
                "https://www.pixiv.net/artworks/7",
                OptionValues::new(),
                None,
            );
            let err = ext.extract(&http.ctx(), &req).await.unwrap_err();
            assert!(err.to_string().contains("不存在"), "{err}");
        }

        #[tokio::test]
        async fn user_feed_newest_first() {
            let http = Arc::new(FakeHttp::new().json(
                "www.pixiv.net/ajax/user/9/profile/all",
                ok(json!({ "illusts": { "5": null, "12": null }, "manga": { "8": null } })),
            ));
            let ext = PixivExtractor::new(http.clone());
            let req = request(
                "pixiv",
                "https://www.pixiv.net/users/9",
                OptionValues::new(),
                None,
            );
            let entries = ext.feed(&http.ctx(), &req).await.unwrap();
            let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
            assert_eq!(keys, ["12", "8", "5"]);
            assert_eq!(entries[0].url.as_str(), "https://www.pixiv.net/artworks/12");
        }
    }
}
