use std::{path::PathBuf, time::Duration};

use congmiao_feedbot::{
    Config, ContentKind, ParseOptions, ParseRequest, Platform, Provider, ProviderRegistry,
    RuntimeCredentials,
    cache::AppCache,
    login::{LoginPoll, LoginService},
    provider::{BilibiliProvider, NeteaseProvider, PixivProvider, XProvider},
    storage::Storage,
    telegram::{caption, extract_urls, media_has_spoiler},
};
use reqwest::Client;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

fn config() -> Config {
    Config {
        telegram_token: "test".into(),
        telegram_api_url: None,
        telegram_request_timeout_secs: 600,
        media_download_timeout_secs: 120,
        media_download_retries: 3,
        database_url: "sqlite::memory:".into(),
        fxtwitter_api_base: "https://api.fxtwitter.com".into(),
        pixiv_web_api_base: "https://www.pixiv.net".into(),
        netease_api_base: "http://127.0.0.1:3000".into(),
        netease_cookie: None,
        youtube_api_key: None,
        youtube_cookies_file: None,
        pixiv_refresh_token: None,
        bilibili_cookie: None,
        bilibili_cdn: congmiao_feedbot::BilibiliCdnPreference::BaseUrl,
        bilibili_passport_base: "https://passport.bilibili.com".into(),
        bilibili_api_base: "https://api.bilibili.com".into(),
        bilibili_live_api_base: "https://api.live.bilibili.com".into(),
        bilibili_www_base: "https://www.bilibili.com".into(),
        ffmpeg_path: "ffmpeg".into(),
        ffprobe_path: "ffprobe".into(),
        yt_dlp_path: "missing-yt-dlp".into(),
        temp_dir: PathBuf::from("tmp"),
        upload_workers: 1,
        max_queue_size: 10,
        request_limit_count: 0,
        request_limit_ttl: 60,
        local_bot_api: false,
        webhook_url: None,
        webhook_host: "127.0.0.1".into(),
        webhook_port: 8080,
        admin_user_id: None,
        media_spoiler_mode: congmiao_feedbot::MediaSpoilerMode::Auto,
    }
}

#[test]
fn registry_routes_all_supported_platforms() {
    let registry = ProviderRegistry::new(&config()).unwrap();
    let cases = [
        ("https://x.com/user/status/1234567890123456789", Platform::X),
        ("https://youtu.be/dQw4w9WgXcQ", Platform::YouTube),
        ("https://www.pixiv.net/artworks/123456", Platform::Pixiv),
        (
            "https://www.bilibili.com/video/BV1xx411c7mD",
            Platform::Bilibili,
        ),
        ("https://music.163.com/#/song?id=123", Platform::Netease),
    ];
    for (url, platform) in cases {
        assert_eq!(registry.find(url).unwrap().platform(), platform);
    }
    assert!(registry.find("https://example.com").is_none());
}

#[test]
fn extracts_and_deduplicates_urls() {
    let text = "看看 https://x.com/a/status/123456789012 和 https://x.com/a/status/123456789012，另有 BV1xx411c7mD。";
    assert_eq!(
        extract_urls(text),
        vec!["https://x.com/a/status/123456789012", "BV1xx411c7mD"]
    );
}

#[test]
fn caption_is_escaped_and_bounded() {
    let mut content = congmiao_feedbot::ParsedContent {
        platform: Platform::X,
        kind: ContentKind::Post,
        id: "1".into(),
        canonical_url: "https://x.com/a/status/1?a=1&b=2".into(),
        author: congmiao_feedbot::Author {
            name: "<Alice>".into(),
            ..Default::default()
        },
        title: "<b>not html</b>".into(),
        text: "x".repeat(5000),
        sensitive: false,
        stats: Default::default(),
        media: vec![],
        collection_items: vec![],
    };
    let value = caption(&content, 1024);
    assert!(value.chars().count() <= 1024);
    assert!(!value.contains("<Alice>"));
    assert!(value.contains("https://x.com/a/status/1"));
    assert!(value.contains("x：&lt;b&gt;not html&lt;/b&gt;"));
    assert!(value.contains("<blockquote expandable>"));
    assert!(value.contains("作者："));

    content.text = "short description".into();
    let value = caption(&content, 1024);
    assert!(value.contains("<blockquote>short description</blockquote>"));
    assert!(!value.contains("<blockquote expandable>"));
}

#[tokio::test]
async fn x_provider_deserializes_post_and_media() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/2/status/123456789012"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"code":200,"status":{"id":"123456789012","url":"https://x.com/a/status/123456789012","text":"hello","possibly_sensitive":true,"likes":7,"author":{"id":"9","name":"Alice","screen_name":"a"},"media":{"photos":[{"type":"photo","url":"https://img.test/a.jpg","width":100,"height":80}],"videos":[]}}}))).mount(&server).await;
    let mut c = config();
    c.fxtwitter_api_base = server.uri();
    let provider = XProvider::new(Client::new(), &c);
    let parsed = provider
        .parse(&ParseRequest {
            url: "https://x.com/a/status/123456789012".into(),
            options: ParseOptions::default(),
        })
        .await
        .unwrap();
    assert_eq!(parsed.author.name, "Alice");
    assert_eq!(parsed.stats.likes, Some(7));
    assert_eq!(parsed.media.len(), 1);
    assert!(parsed.sensitive);
    assert!(media_has_spoiler(&c, &parsed));
}

#[tokio::test]
async fn x_provider_selects_h264_source_at_or_below_requested_quality() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2/status/123456789012"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 200,
            "status": {
                "id": "123456789012",
                "url": "https://x.com/a/status/123456789012",
                "author": {"id": "9", "name": "Alice", "screen_name": "a"},
                "media": {"photos": [], "videos": [{
                    "type": "video",
                    "url": "https://video.twimg.com/vid/avc1/1920x1080/high.mp4",
                    "duration": 15,
                    "width": 1920,
                    "height": 1080,
                    "formats": [
                        {"container": "mp4", "codec": "h264", "bitrate": 832000, "url": "https://video.twimg.com/vid/avc1/640x360/low.mp4"},
                        {"container": "mp4", "codec": "h264", "bitrate": 2176000, "url": "https://video.twimg.com/vid/avc1/1280x720/medium.mp4"},
                        {"container": "mp4", "codec": "h264", "bitrate": 5000000, "url": "https://video.twimg.com/vid/avc1/1920x1080/high.mp4"},
                        {"container": "m3u8", "url": "https://video.twimg.com/video.m3u8"}
                    ]
                }]}
            }
        })))
        .mount(&server)
        .await;
    let mut c = config();
    c.fxtwitter_api_base = server.uri();
    let provider = XProvider::new(Client::new(), &c);
    let parsed = provider
        .parse(&ParseRequest {
            url: "https://x.com/a/status/123456789012".into(),
            options: ParseOptions::default(),
        })
        .await
        .unwrap();
    assert!(parsed.media[0].source_url.contains("1280x720"));
    assert_eq!(parsed.media[0].width, Some(1280));
    assert_eq!(parsed.media[0].height, Some(720));
    assert_eq!(parsed.media[0].duration_secs, Some(15));
    assert_eq!(parsed.media[0].size, Some(4_080_000));
    assert!(parsed.media[0].cache_key.ends_with(":720p"));

    let parsed = provider
        .parse(&ParseRequest {
            url: "https://x.com/a/status/123456789012".into(),
            options: ParseOptions {
                quality: Some(480),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    assert!(parsed.media[0].source_url.contains("640x360"));
    assert!(parsed.media[0].cache_key.ends_with(":480p"));
}

#[tokio::test]
async fn pixiv_provider_supports_multiple_pages() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/ajax/illust/123")).and(query_param("lang", "zh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"error":false,"body":{"title":"work","userId":"5","userName":"Artist","pageCount":2,"xRestrict":1,"tags":{"tags":[{"name":"R-18"},{"tag":"插画"}]}}}))).mount(&server).await;
    Mock::given(method("GET")).and(path("/ajax/illust/123/pages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"error":false,"body":[{"urls":{"original":"https://i.pximg.net/1.jpg"}},{"urls":{"original":"https://i.pximg.net/2.jpg"}}]}))).mount(&server).await;
    let mut c = config();
    c.pixiv_web_api_base = server.uri();
    let parsed = PixivProvider::new(Client::new(), &c)
        .parse(&ParseRequest {
            url: "https://www.pixiv.net/artworks/123".into(),
            options: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(parsed.media.len(), 2);
    assert_eq!(parsed.author.name, "Artist");
    assert!(parsed.text.contains("#R-18"));
    assert!(parsed.text.contains("#插画"));
    assert!(parsed.sensitive);
    assert!(media_has_spoiler(&c, &parsed));
}

#[tokio::test]
async fn pixiv_provider_supports_anonymous_ugoira_metadata() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ajax/illust/456"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "error": false,
            "body": {
                "title": "animated work",
                "userId": "5",
                "userName": "Artist",
                "illustType": 2,
                "urls": {"original": "https://i.pximg.net/frame.jpg"}
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/ajax/illust/456/ugoira_meta"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "error": false,
            "body": {
                "originalSrc": "https://i.pximg.net/frames.zip",
                "frames": [{"file": "000000.jpg", "delay": 100}]
            }
        })))
        .mount(&server)
        .await;
    let mut c = config();
    c.pixiv_web_api_base = server.uri();
    let parsed = PixivProvider::new(Client::new(), &c)
        .parse(&ParseRequest {
            url: "https://www.pixiv.net/artworks/456".into(),
            options: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(parsed.media.len(), 1);
    assert_eq!(parsed.media[0].kind, congmiao_feedbot::MediaKind::Animation);
    assert_eq!(parsed.media[0].source_url, "https://i.pximg.net/frames.zip");
    assert!(
        parsed.media[0]
            .secondary_url
            .as_deref()
            .unwrap()
            .starts_with("ugoira:")
    );
}

#[tokio::test]
async fn netease_provider_parses_song_through_sidecar() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/song/detail")).and(query_param("cookie", "MUSIC_U=test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"songs":[{"name":"Song","dt":1000,"ar":[{"name":"Singer"}],"al":{"name":"Album","picUrl":"https://img/cover.jpg"}}]}))).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/song/url/v1"))
        .and(query_param("cookie", "MUSIC_U=test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"data":[{"url":"https://audio/song.mp3","size":100}]}),
        ))
        .mount(&server)
        .await;
    let mut c = config();
    c.netease_api_base = server.uri();
    let credentials = RuntimeCredentials::memory(&c);
    credentials
        .set_netease("MUSIC_U=test".into())
        .await
        .unwrap();
    let parsed = NeteaseProvider::new_with_credentials(Client::new(), &c, credentials)
        .parse(&ParseRequest {
            url: "https://music.163.com/#/song?id=123".into(),
            options: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(parsed.title, "Song");
    assert_eq!(parsed.author.name, "Singer");
    assert_eq!(parsed.media.len(), 1);
    assert_eq!(parsed.media[0].filename, "Song.mp3");
}

#[tokio::test]
async fn bilibili_provider_parses_video_and_dash_streams() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/x/web-interface/view"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"code":0,"data":{"bvid":"BV1xx411c7mD","cid":42,"title":"Bili video","desc":"desc","pic":"https://img/cover.jpg","duration":12,"owner":{"mid":1,"name":"UP"},"dimension":{"width":1280,"height":720},"stat":{"view":100,"like":5}}}))).mount(&server).await;
    Mock::given(method("GET")).and(path("/x/player/playurl"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"code":0,"data":{"dash":{"duration":12,"video":[{"baseUrl":"https://media/hevc.m4s","width":1280,"height":720,"bandwidth":40000000,"codecid":12},{"baseUrl":"https://media/video-720.m4s","width":1280,"height":720,"bandwidth":40000000,"codecid":7},{"baseUrl":"https://media/video-480.m4s","backupUrl":["https://backup/video-480.m4s"],"width":854,"height":480,"bandwidth":1000000,"codecid":7}],"audio":[{"baseUrl":"https://media/audio.m4s","backupUrl":["https://backup/audio.m4s"],"bandwidth":128000}]}}}))).mount(&server).await;
    let mut c = config();
    c.bilibili_api_base = server.uri();
    c.bilibili_live_api_base = server.uri();
    c.bilibili_www_base = server.uri();
    let parsed = BilibiliProvider::new(Client::new(), &c)
        .parse(&ParseRequest {
            url: "BV1xx411c7mD".into(),
            options: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(parsed.title, "Bili video");
    assert_eq!(parsed.author.name, "UP");
    assert_eq!(parsed.media[0].source_url, "https://media/video-480.m4s");
    assert_eq!(
        parsed.media[0].fallback_urls,
        ["https://backup/video-480.m4s"]
    );
    assert_eq!(parsed.media[0].width, Some(854));
    assert_eq!(parsed.media[0].height, Some(480));
    assert_eq!(
        parsed.media[0].secondary_url.as_deref(),
        Some("https://media/audio.m4s")
    );
    assert_eq!(
        parsed.media[0].secondary_fallback_urls,
        ["https://backup/audio.m4s"]
    );

    c.bilibili_cdn = congmiao_feedbot::BilibiliCdnPreference::BackupUrl;
    let backup_first = BilibiliProvider::new(Client::new(), &c)
        .parse(&ParseRequest {
            url: "BV1xx411c7mD".into(),
            options: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        backup_first.media[0].source_url,
        "https://backup/video-480.m4s"
    );
    assert_eq!(
        backup_first.media[0].fallback_urls,
        ["https://media/video-480.m4s"]
    );

    c.bilibili_cdn =
        congmiao_feedbot::BilibiliCdnPreference::Mirror("upos-sz-mirrorali.bilivideo.com");
    let mirrored = BilibiliProvider::new(Client::new(), &c)
        .parse(&ParseRequest {
            url: "BV1xx411c7mD".into(),
            options: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        mirrored.media[0].source_url,
        "https://upos-sz-mirrorali.bilivideo.com/video-480.m4s"
    );
    assert_eq!(
        mirrored.media[0].fallback_urls,
        [
            "https://media/video-480.m4s",
            "https://backup/video-480.m4s"
        ]
    );
    assert_eq!(
        mirrored.media[0].secondary_url.as_deref(),
        Some("https://upos-sz-mirrorali.bilivideo.com/audio.m4s")
    );
}

#[tokio::test]
async fn sqlite_cache_and_local_rate_limit_work() {
    let dir = tempfile::tempdir().unwrap();
    let db = format!(
        "sqlite://{}?mode=rwc",
        dir.path()
            .join("test.db")
            .display()
            .to_string()
            .replace('\\', "/")
    );
    let storage = Storage::connect(&db).await.unwrap();
    storage
        .put_file_id("key", "photo", "file", Some("unique"))
        .await
        .unwrap();
    assert_eq!(
        storage
            .get_file_id("key", "photo")
            .await
            .unwrap()
            .as_deref(),
        Some("file")
    );
    let cache = AppCache::new();
    assert!(cache.allow("u", 2, Duration::from_secs(60)).await);
    assert!(cache.allow("u", 2, Duration::from_secs(60)).await);
    assert!(!cache.allow("u", 2, Duration::from_secs(60)).await);
}

#[tokio::test]
async fn qr_login_persists_bilibili_and_netease_cookies() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/x/passport-login/web/qrcode/generate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "data": {"qrcode_key": "bili-key", "url": "https://example.com/bili-login"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/x/passport-login/web/qrcode/poll"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("set-cookie", "SESSDATA=session; Path=/; HttpOnly")
                .append_header("set-cookie", "bili_jct=csrf; Path=/")
                .set_body_json(serde_json::json!({
                    "code": 0,
                    "data": {"code": 0, "message": ""}
                })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/login/qr/key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 200, "data": {"unikey": "netease-key"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/login/qr/create"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 200, "data": {"qrurl": "https://example.com/netease-login"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/login/qr/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 803, "cookie": "MUSIC_U=music-session; __csrf=music-csrf"
        })))
        .mount(&server)
        .await;

    let storage = Storage::connect("sqlite::memory:").await.unwrap();
    let mut c = config();
    c.bilibili_passport_base = server.uri();
    c.netease_api_base = server.uri();
    let credentials = RuntimeCredentials::load(storage.clone(), &c).await.unwrap();
    let service = LoginService::new(&c, credentials.clone()).unwrap();

    let bili = service.bilibili_challenge().await.unwrap();
    assert!(bili.image.starts_with(&[0x89, b'P', b'N', b'G']));
    assert_eq!(
        service.poll_bilibili(&bili.key).await.unwrap(),
        LoginPoll::Success
    );
    assert!(
        credentials
            .bilibili()
            .await
            .unwrap()
            .contains("SESSDATA=session")
    );
    assert!(storage.get_credential("bilibili").await.unwrap().is_some());

    let netease = service.netease_challenge().await.unwrap();
    assert!(netease.image.starts_with(&[0x89, b'P', b'N', b'G']));
    assert_eq!(
        service.poll_netease(&netease.key).await.unwrap(),
        LoginPoll::Success
    );
    assert!(
        credentials
            .netease()
            .await
            .unwrap()
            .contains("MUSIC_U=music-session")
    );
    assert!(storage.get_credential("netease").await.unwrap().is_some());
}
