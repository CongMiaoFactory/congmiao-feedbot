use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use congmiao_feedbot::{
    Author, Config, ContentKind, MediaItem, MediaKind, ParsedContent, Platform,
    media::{MediaProcessor, telegram_photo_dimensions_valid},
};
use reqwest::Client;
use tokio::process::Command;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn config(temp_dir: PathBuf) -> Config {
    Config {
        telegram_token: "test".into(),
        telegram_api_url: None,
        telegram_request_timeout_secs: 600,
        media_download_timeout_secs: 120,
        media_download_retries: 3,
        media_photo_source_max_size_mb: 200,
        database_url: "sqlite::memory:".into(),
        fxtwitter_api_base: String::new(),
        pixiv_web_api_base: String::new(),
        netease_api_base: String::new(),
        netease_cookie: None,
        youtube_api_key: None,
        youtube_cookies_file: None,
        pixiv_refresh_token: None,
        bilibili_cookie: None,
        bilibili_cdn: congmiao_feedbot::BilibiliCdnPreference::BaseUrl,
        bilibili_passport_base: String::new(),
        bilibili_api_base: String::new(),
        bilibili_live_api_base: String::new(),
        bilibili_www_base: String::new(),
        ffmpeg_path: "ffmpeg".into(),
        ffprobe_path: "ffprobe".into(),
        yt_dlp_path: "yt-dlp".into(),
        temp_dir,
        upload_workers: 1,
        max_queue_size: 2,
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
fn validates_telegram_photo_dimensions() {
    assert!(telegram_photo_dimensions_valid(5000, 5000));
    assert!(telegram_photo_dimensions_valid(100, 2000));
    assert!(!telegram_photo_dimensions_valid(100, 2001));
    assert!(!telegram_photo_dimensions_valid(5001, 5000));
    assert!(!telegram_photo_dimensions_valid(0, 100));
}

#[tokio::test]
async fn interrupted_download_resumes_with_http_range() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let resumed = Arc::new(AtomicBool::new(false));
    let resumed_for_server = resumed.clone();
    let server = thread::spawn(move || {
        for request_number in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let length = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..length]);
            if request_number == 0 {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nhello",
                    )
                    .unwrap();
            } else {
                assert!(request.contains("range: bytes=5-"));
                resumed_for_server.store(true, Ordering::SeqCst);
                stream
                    .write_all(
                        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 5\r\nContent-Range: bytes 5-9/10\r\nConnection: close\r\n\r\nworld",
                    )
                    .unwrap();
            }
        }
    });
    let item = MediaItem {
        kind: MediaKind::Document,
        source_url: format!("http://{address}/resume"),
        fallback_urls: vec![],
        thumbnail_url: None,
        filename: "resume.bin".into(),
        mime_type: Some("application/octet-stream".into()),
        duration_secs: None,
        width: None,
        height: None,
        size: Some(10),
        headers: Default::default(),
        cache_key: "test:resume".into(),
        requires_download: true,
        secondary_url: None,
        secondary_fallback_urls: vec![],
    };
    let content = ParsedContent {
        platform: Platform::Bilibili,
        kind: ContentKind::Post,
        id: "resume".into(),
        canonical_url: "https://example.com/resume".into(),
        author: Author::default(),
        title: String::new(),
        text: String::new(),
        sensitive: false,
        stats: Default::default(),
        media: vec![item.clone()],
        collection_items: vec![],
    };
    let output_dir = tempfile::tempdir().unwrap();
    let mut test_config = config(output_dir.path().into());
    test_config.media_download_retries = 1;
    let processor = MediaProcessor::new(Client::new(), &test_config);
    let prepared = processor.prepare(&content, &item, 720).await.unwrap();
    server.join().unwrap();
    assert!(resumed.load(Ordering::SeqCst));
    assert_eq!(
        tokio::fs::read(&prepared.path).await.unwrap(),
        b"helloworld"
    );
    processor.cleanup(prepared).await;
}

#[tokio::test]
async fn oversized_content_length_stops_without_retries() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_for_server = requests.clone();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    requests_for_server.fetch_add(1, Ordering::SeqCst);
                    let mut request = [0_u8; 1024];
                    let _ = stream.read(&mut request);
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 52428801\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("测试服务器错误: {error}"),
            }
        }
    });
    let item = MediaItem {
        kind: MediaKind::Document,
        source_url: format!("http://{address}/too-large"),
        fallback_urls: vec![],
        thumbnail_url: None,
        filename: "too-large.bin".into(),
        mime_type: Some("application/octet-stream".into()),
        duration_secs: None,
        width: None,
        height: None,
        size: None,
        headers: Default::default(),
        cache_key: "test:too-large".into(),
        requires_download: true,
        secondary_url: None,
        secondary_fallback_urls: vec![],
    };
    let content = ParsedContent {
        platform: Platform::Bilibili,
        kind: ContentKind::Post,
        id: "too-large".into(),
        canonical_url: "https://example.com/too-large".into(),
        author: Author::default(),
        title: String::new(),
        text: String::new(),
        sensitive: false,
        stats: Default::default(),
        media: vec![item.clone()],
        collection_items: vec![],
    };
    let output_dir = tempfile::tempdir().unwrap();
    let processor = MediaProcessor::new(Client::new(), &config(output_dir.path().into()));
    let error = processor.prepare(&content, &item, 720).await.unwrap_err();
    server.join().unwrap();
    assert!(error.to_string().contains("50MB"));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn oversized_photo_dimensions_generate_preview_and_preserve_original_for_file_mode() {
    if Command::new("ffmpeg")
        .arg("-version")
        .output()
        .await
        .is_err()
    {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let photo = fixture.path().join("tall.jpg");
    let generated = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=100x2102",
            "-frames:v",
            "1",
        ])
        .arg(&photo)
        .output()
        .await
        .unwrap();
    assert!(generated.status.success());
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tall.jpg"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(tokio::fs::read(&photo).await.unwrap()),
        )
        .mount(&server)
        .await;
    let item = MediaItem {
        kind: MediaKind::Photo,
        source_url: format!("{}/tall.jpg", server.uri()),
        fallback_urls: vec![],
        thumbnail_url: None,
        filename: "tall.jpg".into(),
        mime_type: Some("image/jpeg".into()),
        duration_secs: None,
        width: None,
        height: None,
        size: None,
        headers: Default::default(),
        cache_key: "test:tall-photo".into(),
        requires_download: true,
        secondary_url: None,
        secondary_fallback_urls: vec![],
    };
    let content = ParsedContent {
        platform: Platform::X,
        kind: ContentKind::Post,
        id: "tall".into(),
        canonical_url: "https://example.com/tall".into(),
        author: Author::default(),
        title: String::new(),
        text: String::new(),
        sensitive: false,
        stats: Default::default(),
        media: vec![item.clone()],
        collection_items: vec![],
    };
    let output_dir = tempfile::tempdir().unwrap();
    let processor = MediaProcessor::new(Client::new(), &config(output_dir.path().into()));
    let prepared = processor.prepare(&content, &item, 720).await.unwrap();
    assert!(prepared.is_preview);
    assert_eq!(prepared.item.mime_type.as_deref(), Some("image/jpeg"));
    assert!(
        prepared
            .item
            .cache_key
            .ends_with(":telegram-photo-preview-v1")
    );
    let preview = image::open(&prepared.path).unwrap();
    assert!(telegram_photo_dimensions_valid(
        preview.width(),
        preview.height()
    ));
    assert_eq!(preview.height(), 2102);
    assert!(preview.width() > 100);
    assert!(tokio::fs::metadata(&prepared.path).await.unwrap().len() < 9 * 1024 * 1024);
    processor.cleanup(prepared).await;

    let original = processor
        .prepare_original(&content, &item, 720)
        .await
        .unwrap();
    assert!(!original.is_preview);
    let original_image = image::open(&original.path).unwrap();
    assert_eq!(
        (original_image.width(), original_image.height()),
        (100, 2102)
    );
    processor.cleanup(original).await;
}

#[tokio::test]
async fn oversized_photo_source_uses_upstream_preview() {
    if Command::new("ffmpeg")
        .arg("-version")
        .output()
        .await
        .is_err()
    {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let thumbnail = fixture.path().join("thumbnail.jpg");
    let generated = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=green:s=64x64",
            "-frames:v",
            "1",
        ])
        .arg(&thumbnail)
        .output()
        .await
        .unwrap();
    assert!(generated.status.success());
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/original.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0_u8; 1024 * 1024 + 1]))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/thumbnail.jpg"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(tokio::fs::read(&thumbnail).await.unwrap()),
        )
        .mount(&server)
        .await;
    let item = MediaItem {
        kind: MediaKind::Photo,
        source_url: format!("{}/original.jpg", server.uri()),
        fallback_urls: vec![],
        thumbnail_url: Some(format!("{}/thumbnail.jpg", server.uri())),
        filename: "original.jpg".into(),
        mime_type: Some("image/jpeg".into()),
        duration_secs: None,
        width: None,
        height: None,
        size: None,
        headers: Default::default(),
        cache_key: "test:large-photo-source".into(),
        requires_download: true,
        secondary_url: None,
        secondary_fallback_urls: vec![],
    };
    let content = ParsedContent {
        platform: Platform::Pixiv,
        kind: ContentKind::Artwork,
        id: "large".into(),
        canonical_url: "https://example.com/large".into(),
        author: Author::default(),
        title: String::new(),
        text: String::new(),
        sensitive: false,
        stats: Default::default(),
        media: vec![item.clone()],
        collection_items: vec![],
    };
    let output_dir = tempfile::tempdir().unwrap();
    let mut test_config = config(output_dir.path().into());
    test_config.media_photo_source_max_size_mb = 1;
    let processor = MediaProcessor::new(Client::new(), &test_config);
    let prepared = processor.prepare(&content, &item, 720).await.unwrap();
    assert!(prepared.is_preview);
    assert_eq!(
        (prepared.item.width, prepared.item.height),
        (Some(64), Some(64))
    );
    assert!(
        prepared
            .item
            .cache_key
            .ends_with(":telegram-photo-preview-v1")
    );
    processor.cleanup(prepared).await;
}

#[tokio::test]
async fn downloads_and_merges_dash_tracks_with_ffmpeg() {
    if Command::new("ffmpeg")
        .arg("-version")
        .output()
        .await
        .is_err()
    {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let video = fixture.path().join("video.mp4");
    let audio = fixture.path().join("audio.m4a");
    let v = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=320x240:d=0.2",
            "-an",
            "-c:v",
            "mpeg4",
        ])
        .arg(&video)
        .output()
        .await
        .unwrap();
    let a = Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.2",
            "-vn",
            "-c:a",
            "aac",
        ])
        .arg(&audio)
        .output()
        .await
        .unwrap();
    assert!(v.status.success() && a.status.success());
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(tokio::fs::read(&video).await.unwrap()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/a"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(tokio::fs::read(&audio).await.unwrap()),
        )
        .mount(&server)
        .await;
    let item = MediaItem {
        kind: MediaKind::Video,
        source_url: format!("{}/v", server.uri()),
        fallback_urls: vec![],
        thumbnail_url: None,
        filename: "merged.mp4".into(),
        mime_type: Some("video/mp4".into()),
        duration_secs: None,
        width: None,
        height: None,
        size: None,
        headers: Default::default(),
        cache_key: "test:merge".into(),
        requires_download: true,
        secondary_url: Some(format!("{}/missing-audio", server.uri())),
        secondary_fallback_urls: vec![format!("{}/a", server.uri())],
    };
    let content = ParsedContent {
        platform: Platform::Bilibili,
        kind: ContentKind::Video,
        id: "x".into(),
        canonical_url: "https://example.com".into(),
        author: Author::default(),
        title: String::new(),
        text: String::new(),
        sensitive: false,
        stats: Default::default(),
        media: vec![item.clone()],
        collection_items: vec![],
    };
    let output_dir = tempfile::tempdir().unwrap();
    let mut test_config = config(output_dir.path().into());
    test_config.media_download_retries = 0;
    let processor = MediaProcessor::new(Client::new(), &test_config);
    let prepared = processor.prepare(&content, &item, 720).await.unwrap();
    assert!(tokio::fs::metadata(&prepared.path).await.unwrap().len() > 0);
    assert_eq!(prepared.item.width, Some(320));
    assert_eq!(prepared.item.height, Some(240));
    assert_eq!(prepared.item.duration_secs, Some(1));
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,pix_fmt",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(&prepared.path)
        .output()
        .await
        .unwrap();
    let probe = String::from_utf8_lossy(&probe.stdout);
    assert!(probe.contains("codec_name=h264"));
    assert!(probe.contains("pix_fmt=yuv420p"));
    processor.cleanup(prepared).await;
}

#[tokio::test]
async fn converts_pixiv_ugoira_zip_to_mp4() {
    if Command::new("ffmpeg")
        .arg("-version")
        .output()
        .await
        .is_err()
    {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let first = fixture.path().join("000000.jpg");
    let second = fixture.path().join("000001.jpg");
    for (path, color) in [(&first, "red"), (&second, "blue")] {
        let output = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("color=c={color}:s=64x64"),
                "-frames:v",
                "1",
            ])
            .arg(path)
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
    }
    let archive = fixture.path().join("ugoira.zip");
    {
        use std::io::Write;
        let file = std::fs::File::create(&archive).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for path in [&first, &second] {
            zip.start_file(path.file_name().unwrap().to_string_lossy(), options)
                .unwrap();
            zip.write_all(&std::fs::read(path).unwrap()).unwrap();
        }
        zip.finish().unwrap();
    }
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/u.zip"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(tokio::fs::read(&archive).await.unwrap()),
        )
        .mount(&server)
        .await;
    let metadata =
        serde_json::json!([{"file":"000000.jpg","delay":80},{"file":"000001.jpg","delay":80}]);
    let item = MediaItem {
        kind: MediaKind::Animation,
        source_url: format!("{}/u.zip", server.uri()),
        fallback_urls: vec![],
        thumbnail_url: None,
        filename: "ugoira.mp4".into(),
        mime_type: Some("video/mp4".into()),
        duration_secs: None,
        width: Some(64),
        height: Some(64),
        size: None,
        headers: Default::default(),
        cache_key: "test:ugoira".into(),
        requires_download: true,
        secondary_url: Some(format!("ugoira:{metadata}")),
        secondary_fallback_urls: vec![],
    };
    let content = ParsedContent {
        platform: Platform::Pixiv,
        kind: ContentKind::Artwork,
        id: "u".into(),
        canonical_url: "https://pixiv.net/artworks/u".into(),
        author: Author::default(),
        title: String::new(),
        text: String::new(),
        sensitive: false,
        stats: Default::default(),
        media: vec![item.clone()],
        collection_items: vec![],
    };
    let output_dir = tempfile::tempdir().unwrap();
    let processor = MediaProcessor::new(Client::new(), &config(output_dir.path().into()));
    let prepared = processor.prepare(&content, &item, 720).await.unwrap();
    assert!(tokio::fs::metadata(&prepared.path).await.unwrap().len() > 0);
    processor.cleanup(prepared).await;
}
