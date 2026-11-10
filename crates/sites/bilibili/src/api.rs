//! Request plumbing (headers, buvid, JSON envelope) and per-target extract methods.

use super::auth::{cookie_has_value, join_pairs, map_bili_code, merge_cookies};
use super::content::{article_from_api, audio_from_api, live_from_room};
use super::dynamic::{
    OpusProjection, apply_opus, dynamic_from_item, embedded_url, needs_opus, opus_from_item,
    source_id,
};
use super::links::{av_digits, is_bvid, parse_target};
use super::video::extracted_from_view;
use super::wbi::{self, WbiKey};
use super::{BilibiliExtractor, BilibiliTarget};
use feedbot_core::extract::{ExtractCtx, ExtractError, Extracted, HttpRequest, MediaSource};
use http::{HeaderMap, HeaderValue, header};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Instant;
use url::Url;

use feedbot_site_common::{BROWSER_UA, json_str, json_u64};

const ACCEPT: &str = "application/json, text/plain, */*";
const ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.9,en;q=0.8";
const OPUS_FEATURES: &str = "itemOpusStyle,opusBigCover,onlyfansVote,endFooterHidden,decorationCard,onlyfansAssetsV2,ugcDelete,onlyfansQaCard,commentsNewVersion";

impl BilibiliExtractor {
    pub(super) async fn extract_video(
        &self,
        ctx: &ExtractCtx,
        bvid: Option<&str>,
        aid: Option<&str>,
        cookie: Option<&str>,
        options: &feedbot_core::options::OptionValues,
    ) -> Result<Extracted, ExtractError> {
        let mut params = Vec::new();
        if let Some(v) = bvid.filter(|s| is_bvid(s)) {
            params.push(("bvid".into(), v.to_string()));
        } else if let Some(v) = bvid.and_then(av_digits).or(aid.map(str::to_string)) {
            params.push(("aid".into(), v));
        } else if let Some(v) = bvid {
            if is_bvid(v) {
                params.push(("bvid".into(), v.to_string()));
            } else if let Some(n) = av_digits(v) {
                params.push(("aid".into(), n));
            } else {
                return Err(ExtractError::Unsupported);
            }
        } else {
            return Err(ExtractError::Unsupported);
        }
        let view = self
            .api_json(
                ctx,
                &self.api_base,
                "/x/web-interface/view",
                &params,
                cookie,
            )
            .await?;
        let data = view
            .get("data")
            .cloned()
            .ok_or_else(|| ExtractError::Failed("视频详情缺少 data".into()))?;
        extracted_from_view(self, ctx, data, cookie, options).await
    }

    pub(super) async fn extract_bangumi(
        &self,
        ctx: &ExtractCtx,
        id: &str,
        cookie: Option<&str>,
        options: &feedbot_core::options::OptionValues,
    ) -> Result<Extracted, ExtractError> {
        let digits = id
            .get(2..)
            .filter(|s| s.chars().all(|c| c.is_ascii_digit()))
            .ok_or(ExtractError::Unsupported)?;
        let key = if id.len() >= 2 && id[..2].eq_ignore_ascii_case("ss") {
            "season_id"
        } else {
            "ep_id"
        };
        let root = self
            .api_json(
                ctx,
                &self.api_base,
                "/pgc/view/web/season",
                &[(key.into(), digits.into())],
                cookie,
            )
            .await?;
        let episodes = root
            .pointer("/result/episodes")
            .and_then(Value::as_array)
            .ok_or_else(|| ExtractError::Failed("番剧缺少分集".into()))?;
        let episode = if key == "ep_id" {
            episodes
                .iter()
                .find(|e| json_u64(e, "/id").map(|n| n.to_string()) == Some(digits.into()))
                .or_else(|| episodes.first())
        } else {
            episodes.first()
        }
        .ok_or_else(|| ExtractError::Failed("番剧没有分集".into()))?;
        let bvid = json_str(episode, "/bvid")
            .ok_or_else(|| ExtractError::Failed("番剧缺少 bvid".into()))?;
        self.extract_video(ctx, Some(bvid), None, cookie, options)
            .await
    }

    pub(super) async fn extract_live(
        &self,
        ctx: &ExtractCtx,
        room: &str,
        cookie: Option<&str>,
    ) -> Result<Extracted, ExtractError> {
        let root = self
            .api_json(
                ctx,
                &self.live_api_base,
                "/xlive/web-room/v1/index/getInfoByRoom",
                &[("room_id".into(), room.into())],
                cookie,
            )
            .await?;
        live_from_room(room, &root, &media_headers(cookie)?)
    }

    pub(super) async fn extract_dynamic(
        &self,
        ctx: &ExtractCtx,
        id: &str,
        cookie: Option<&str>,
        options: &feedbot_core::options::OptionValues,
    ) -> Result<Extracted, ExtractError> {
        let root = self
            .api_json(
                ctx,
                &self.api_base,
                "/x/polymer/web-dynamic/v1/detail",
                &[
                    ("id".into(), id.into()),
                    ("features".into(), OPUS_FEATURES.into()),
                ],
                cookie,
            )
            .await?;
        let item = root
            .pointer("/data/item")
            .filter(|v| !v.is_null())
            .cloned()
            .ok_or_else(|| ExtractError::Failed("动态不存在".into()))?;
        let mut extracted = dynamic_from_item(id, &item, &media_headers(cookie)?)?;
        if needs_opus(&item)
            && let Ok(opus) = self.fetch_opus(ctx, source_id(&item, id), cookie).await
        {
            apply_opus(&mut extracted, opus);
        }
        if extracted.items.is_empty()
            && let Some(url) = embedded_url(
                item.pointer("/modules/module_dynamic/major")
                    .unwrap_or(&Value::Null),
            )
            && let Ok(parsed) = Url::parse(&url)
            && let Some(target) = parse_target(&parsed)
            && let Ok(mut nested) = self.extract_nested(ctx, target, cookie, options).await
        {
            if extracted.title.as_deref().is_none_or(str::is_empty) {
                extracted.title = nested.title.take();
            }
            extracted.items.append(&mut nested.items);
        }
        Ok(extracted)
    }

    pub(super) async fn extract_nested(
        &self,
        ctx: &ExtractCtx,
        target: BilibiliTarget,
        cookie: Option<&str>,
        options: &feedbot_core::options::OptionValues,
    ) -> Result<Extracted, ExtractError> {
        match target {
            BilibiliTarget::Video(id) => {
                self.extract_video(ctx, Some(&id), None, cookie, options)
                    .await
            }
            BilibiliTarget::Bangumi(id) => self.extract_bangumi(ctx, &id, cookie, options).await,
            BilibiliTarget::Live(room) => self.extract_live(ctx, &room, cookie).await,
            BilibiliTarget::Audio(id) => self.extract_audio(ctx, &id, cookie).await,
            BilibiliTarget::Article(id) => self.extract_article(ctx, &id, cookie).await,
            BilibiliTarget::Dynamic(_) => Err(ExtractError::Unsupported),
        }
    }

    pub(super) async fn fetch_opus(
        &self,
        ctx: &ExtractCtx,
        id: &str,
        cookie: Option<&str>,
    ) -> Result<OpusProjection, ExtractError> {
        let root = self
            .api_json(
                ctx,
                &self.api_base,
                "/x/polymer/web-dynamic/v1/opus/detail",
                &[
                    ("id".into(), id.into()),
                    ("features".into(), OPUS_FEATURES.into()),
                    ("timezone_offset".into(), "-480".into()),
                ],
                cookie,
            )
            .await?;
        let item = root
            .pointer("/data/item")
            .ok_or_else(|| ExtractError::Failed("Opus 详情缺少 item".into()))?;
        Ok(opus_from_item(item))
    }

    pub(super) async fn extract_audio(
        &self,
        ctx: &ExtractCtx,
        id: &str,
        cookie: Option<&str>,
    ) -> Result<Extracted, ExtractError> {
        let sid = id.trim_start_matches(|c: char| !c.is_ascii_digit());
        let song = self
            .api_json(
                ctx,
                &self.www_base,
                "/audio/music-service-c/web/song/info",
                &[("sid".into(), sid.into())],
                cookie,
            )
            .await?;
        let stream = self
            .api_json(
                ctx,
                &self.www_base,
                "/audio/music-service-c/web/url",
                &[
                    ("sid".into(), sid.into()),
                    ("quality".into(), "2".into()),
                    ("privilege".into(), "2".into()),
                ],
                cookie,
            )
            .await?;
        audio_from_api(sid, &song, &stream, &media_headers(cookie)?)
    }

    pub(super) async fn extract_article(
        &self,
        ctx: &ExtractCtx,
        id: &str,
        cookie: Option<&str>,
    ) -> Result<Extracted, ExtractError> {
        let cv = id.trim_start_matches(|c: char| !c.is_ascii_digit());
        let root = self
            .api_json(
                ctx,
                &self.api_base,
                "/x/article/viewinfo",
                &[
                    ("id".into(), cv.into()),
                    ("mobi_app".into(), "pc".into()),
                    ("from".into(), "web".into()),
                ],
                cookie,
            )
            .await?;
        article_from_api(cv, &root, &media_headers(cookie)?)
    }

    pub(super) async fn api_json(
        &self,
        ctx: &ExtractCtx,
        base: &str,
        path: &str,
        params: &[(String, String)],
        cookie: Option<&str>,
    ) -> Result<Value, ExtractError> {
        self.request_json(ctx, base, path, params, cookie, false)
            .await
    }

    /// [`Self::api_json`] with a WBI-signed query.
    pub(super) async fn wbi_json(
        &self,
        ctx: &ExtractCtx,
        base: &str,
        path: &str,
        params: &[(String, String)],
        cookie: Option<&str>,
    ) -> Result<Value, ExtractError> {
        self.request_json(ctx, base, path, params, cookie, true)
            .await
    }

    /// One retry after a -352 risk check, with a fresh buvid (and WBI key).
    async fn request_json(
        &self,
        ctx: &ExtractCtx,
        base: &str,
        path: &str,
        params: &[(String, String)],
        cookie: Option<&str>,
        signed: bool,
    ) -> Result<Value, ExtractError> {
        self.ensure_buvid(ctx, false).await;
        for attempt in 0..2 {
            let headers = self.request_headers(cookie)?;
            let url = if signed {
                let mixin = self.wbi_mixin(ctx, cookie, attempt > 0).await?;
                let mut url = build_url(base, path, &[])?;
                url.set_query(Some(&wbi::signed_query(params, &mixin, wbi::now_secs())));
                url
            } else {
                build_url(base, path, params)?
            };
            let resp = ctx.http.fetch(HttpRequest::get(url, headers)).await?;
            let value: Value = serde_json::from_slice(&resp.body)
                .map_err(|e| ExtractError::Failed(e.to_string()))?;
            let code = value
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            if code == -352 && attempt == 0 {
                self.ensure_buvid(ctx, true).await;
                continue;
            }
            return map_bili_code(code, value);
        }
        Err(ExtractError::Failed(
            "Bilibili 风控校验失败，请稍后重试".into(),
        ))
    }

    /// Cached WBI mixin key; refetched after [`wbi::KEY_TTL`] or when `force`.
    async fn wbi_mixin(
        &self,
        ctx: &ExtractCtx,
        cookie: Option<&str>,
        force: bool,
    ) -> Result<String, ExtractError> {
        if !force {
            let guard = self.wbi_key.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(key) = guard
                .as_ref()
                .filter(|k| k.fetched.elapsed() < wbi::KEY_TTL)
            {
                return Ok(key.mixin.clone());
            }
        }
        let url = build_url(&self.api_base, "/x/web-interface/nav", &[])?;
        let resp = ctx
            .http
            .fetch(HttpRequest::get(url, self.request_headers(cookie)?))
            .await?;
        let mixin = serde_json::from_slice::<Value>(&resp.body)
            .ok()
            .and_then(|nav| wbi::mixin_from_nav(&nav))
            .ok_or_else(|| ExtractError::Failed("Bilibili 未返回 WBI 签名密钥".into()))?;
        *self.wbi_key.lock().unwrap_or_else(|e| e.into_inner()) = Some(WbiKey {
            mixin: mixin.clone(),
            fetched: Instant::now(),
        });
        Ok(mixin)
    }

    pub(super) async fn ensure_buvid(&self, ctx: &ExtractCtx, force: bool) {
        {
            let guard = self.device_cookie.lock().unwrap_or_else(|e| e.into_inner());
            if !force
                && guard
                    .as_deref()
                    .is_some_and(|c| cookie_has_value(c, "buvid3"))
            {
                return;
            }
        }
        let Ok(url) = Url::parse(&format!("{}/x/frontend/finger/spi", self.api_base)) else {
            return;
        };
        let Ok(headers) = browser_headers(None) else {
            return;
        };
        let Ok(resp) = ctx.http.fetch(HttpRequest::get(url, headers)).await else {
            return;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&resp.body) else {
            return;
        };
        let Some(buvid3) = json_str(&value, "/data/b_3").filter(|s| !s.is_empty()) else {
            return;
        };
        let buvid4 = json_str(&value, "/data/b_4").filter(|s| !s.is_empty());
        let mut pairs = BTreeMap::new();
        pairs.insert("buvid3".into(), buvid3.to_string());
        if let Some(v) = buvid4 {
            pairs.insert("buvid4".into(), v.to_string());
        }
        let joined = join_pairs(&pairs);
        if let Ok(mut guard) = self.device_cookie.lock() {
            *guard = Some(joined);
        }
    }

    pub(super) fn request_headers(&self, account: Option<&str>) -> Result<HeaderMap, ExtractError> {
        let device = self
            .device_cookie
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let merged = merge_cookies(account, device.as_deref());
        browser_headers(merged.as_deref())
    }
}

pub(super) fn with_headers(source: MediaSource, headers: HeaderMap) -> MediaSource {
    match source {
        MediaSource::Direct { url, .. } => MediaSource::Direct { url, headers },
        MediaSource::Separated { video, audio, .. } => MediaSource::Separated {
            video,
            audio,
            headers,
        },
        other => other,
    }
}

pub(super) fn media_headers(cookie: Option<&str>) -> Result<HeaderMap, ExtractError> {
    browser_headers(cookie)
}

pub(super) fn browser_headers(cookie: Option<&str>) -> Result<HeaderMap, ExtractError> {
    let mut headers = HeaderMap::new();
    headers.insert(header::USER_AGENT, HeaderValue::from_static(BROWSER_UA));
    headers.insert(header::ACCEPT, HeaderValue::from_static(ACCEPT));
    headers.insert(
        header::ACCEPT_LANGUAGE,
        HeaderValue::from_static(ACCEPT_LANGUAGE),
    );
    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://www.bilibili.com"),
    );
    headers.insert(
        header::REFERER,
        HeaderValue::from_static("https://www.bilibili.com/"),
    );
    if let Some(c) = cookie.filter(|s| !s.is_empty()) {
        let value = HeaderValue::from_str(c)
            .map_err(|_| ExtractError::Failed("cookie 含有非法字符".into()))?;
        headers.insert(header::COOKIE, value);
    }
    Ok(headers)
}

pub(super) fn build_url(
    base: &str,
    path: &str,
    params: &[(String, String)],
) -> Result<Url, ExtractError> {
    let mut url =
        Url::parse(&format!("{base}{path}")).map_err(|e| ExtractError::Failed(e.to_string()))?;
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use crate::BilibiliExtractor;
    use feedbot_core::extract::{ExtractError, Extractor, MediaSource};
    use feedbot_core::options::OptionValues;
    use feedbot_site_common::testing::{FakeHttp, request};
    use serde_json::json;
    use std::sync::Arc;

    const SPI: &str = "api.bilibili.com/x/frontend/finger/spi";
    const NAV: &str = "api.bilibili.com/x/web-interface/nav";
    const SPACE: &str = "api.bilibili.com/x/polymer/web-dynamic/v1/feed/space";

    fn base() -> FakeHttp {
        FakeHttp::new()
            .json(
                SPI,
                json!({ "code": 0, "data": { "b_3": "B3", "b_4": "B4" } }),
            )
            .json(
                NAV,
                json!({ "code": -101, "data": { "wbi_img": {
                "img_url": "https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png",
                "sub_url": "https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png"
            } } }),
            )
    }

    fn extractor(http: &Arc<FakeHttp>) -> BilibiliExtractor {
        BilibiliExtractor::new(Default::default(), http.clone(), None)
    }

    #[tokio::test]
    async fn video_with_quality_and_login() {
        let http = Arc::new(
            base()
                .json("api.bilibili.com/x/web-interface/view", json!({ "code": 0, "data": {
                    "bvid": "BV1xx411c7mD", "cid": 5, "title": "标题",
                    "owner": { "mid": 2, "name": "up" }
                } }))
                .json("api.bilibili.com/x/player/playurl", json!({ "code": 0, "data": { "dash": {
                    "video": [
                        { "baseUrl": "https://upos.example/1080.m4s", "height": 1080, "codecid": 7 },
                        { "baseUrl": "https://upos.example/720.m4s", "height": 720, "codecid": 7 }
                    ],
                    "audio": [{ "baseUrl": "https://upos.example/a.m4s" }]
                } } })),
        );
        let ext = extractor(&http);
        let options = ext
            .resolve_options(&OptionValues::new(), &["+q:720".into()])
            .unwrap();
        let req = request(
            "bilibili",
            "https://www.bilibili.com/video/BV1xx411c7mD",
            options,
            Some("SESSDATA=s"),
        );
        let extracted = ext.extract(&http.ctx(), &req).await.unwrap();

        assert_eq!(extracted.title.as_deref(), Some("标题"));
        let MediaSource::Separated { video, headers, .. } = &extracted.items[0].source else {
            panic!("{:?}", extracted.items[0].source);
        };
        assert!(video.as_str().ends_with("720.m4s"));
        assert!(headers.contains_key("cookie"));
        let play = http.last("api.bilibili.com/x/player/playurl");
        assert!(play.url.query().unwrap().contains("cid=5"));
        assert!(play.url.query().unwrap().contains("qn=64"));
        let cookie = play.headers["cookie"].to_str().unwrap();
        assert!(
            cookie.contains("SESSDATA=s") && cookie.contains("buvid3=B3"),
            "{cookie}"
        );
    }

    #[tokio::test]
    async fn space_feed_is_signed_and_key_cached() {
        let http = Arc::new(base().json(
            SPACE,
            json!({ "code": 0, "data": { "items": [
            { "id_str": "300", "type": "DYNAMIC_TYPE_WORD",
              "modules": { "module_author": { "pub_ts": 3 } } }
        ] } }),
        ));
        let ext = extractor(&http);
        let req = request(
            "bilibili",
            "https://space.bilibili.com/2",
            OptionValues::new(),
            None,
        );
        for _ in 0..2 {
            let entries = ext.feed(&http.ctx(), &req).await.unwrap();
            assert_eq!(entries[0].url.as_str(), "https://t.bilibili.com/300");
        }
        let query = http.last(SPACE).url.query().unwrap().to_string();
        assert!(
            query.starts_with("features=itemOpusStyle&host_mid=2&wts="),
            "{query}"
        );
        assert!(query.contains("&w_rid="), "{query}");
        let paths: Vec<String> = http.urls().iter().map(|u| u.path().to_string()).collect();
        assert_eq!(paths.iter().filter(|p| p.ends_with("/nav")).count(), 1);
    }

    #[tokio::test]
    async fn risk_check_retries_once_with_fresh_keys() {
        let http = Arc::new(base().json(SPACE, json!({ "code": -352, "message": "风控校验失败" })));
        let ext = extractor(&http);
        let req = request(
            "bilibili",
            "https://space.bilibili.com/2/video",
            OptionValues::new(),
            None,
        );
        let err = ext.feed(&http.ctx(), &req).await.unwrap_err();
        assert!(
            matches!(&err, ExtractError::Failed(m) if m.contains("风控")),
            "{err:?}"
        );
        let paths: Vec<String> = http.urls().iter().map(|u| u.path().to_string()).collect();
        let count = |suffix: &str| paths.iter().filter(|p| p.ends_with(suffix)).count();
        assert_eq!((count("/spi"), count("/nav"), count("/space")), (2, 2, 2));
    }
}
