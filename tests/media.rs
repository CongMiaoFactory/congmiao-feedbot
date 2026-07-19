use std::path::PathBuf;

use congmiao_feedbot::{
    Author, Config, ContentKind, MediaItem, MediaKind, ParsedContent, Platform,
    media::MediaProcessor,
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
        database_url: "sqlite::memory:".into(),
        redis_url: None,
        fxtwitter_api_base: String::new(),
        pixiv_web_api_base: String::new(),
        netease_api_base: String::new(),
        youtube_api_key: None,
        youtube_cookies_file: None,
        pixiv_refresh_token: None,
        bilibili_cookie: None,
        bilibili_api_base: String::new(),
        bilibili_live_api_base: String::new(),
        bilibili_www_base: String::new(),
        ffmpeg_path: "ffmpeg".into(),
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
    }
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
            "libx264",
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
        secondary_url: Some(format!("{}/a", server.uri())),
    };
    let content = ParsedContent {
        platform: Platform::Bilibili,
        kind: ContentKind::Video,
        id: "x".into(),
        canonical_url: "https://example.com".into(),
        author: Author::default(),
        title: String::new(),
        text: String::new(),
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
    };
    let content = ParsedContent {
        platform: Platform::Pixiv,
        kind: ContentKind::Artwork,
        id: "u".into(),
        canonical_url: "https://pixiv.net/artworks/u".into(),
        author: Author::default(),
        title: String::new(),
        text: String::new(),
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
