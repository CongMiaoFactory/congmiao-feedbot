# Congmiao FeedBot

用 Rust 编写的 Telegram 信息解析 Bot。发送链接后，Bot 会解析正文、作者、统计数据和媒体，并在 Telegram 限制内转发媒体。

## 支持范围

| 来源 | 首版链接 | 数据来源 |
| --- | --- | --- |
| X / Twitter | 单帖、引用帖、图片、视频 | FxTwitter v2 API（免费、Base URL 可配置） |
| YouTube | 视频、Shorts、直播回放 | Data API（可选）、oEmbed、yt-dlp |
| Pixiv | 插画、漫画、多页作品、ugoira | Pixiv Web API（免费）/ App API（可选） |
| 哔哩哔哩 | BV/AV 视频、动态/Opus、直播信息 | 原生 Rust 请求 Bilibili Web API |
| 网易云音乐 | 歌曲、专辑、歌单、MV | 独立 `api-enhanced` HTTP sidecar |

专辑、歌单只显示封面和前 30 个曲目；不会批量下载。直播只显示直播信息和封面。Pixiv 多页作品按每组最多 10 张发送。

## Telegram 用法

- 直接发送一个或多个支持的 URL。
- `/parse <url>`：显式解析。
- `/video [360p|480p|720p|1080p] <url>`：指定视频上限，默认 720p。
- `/file <url>`：以文件形式发送媒体。
- `/cover <url>`：只发送封面（来源提供封面时）。
- 启用 BotFather Inline Mode 后，可通过 `@botname <url>` 生成解析预览。

默认视频选择 H.264/AAC 且不高于 720p。超过 Telegram 大小限制时会用 FFmpeg 压缩；仍超限则退回文本和原链接。成功上传的 `file_id` 存入 SQLite，重复发送不再下载。

## 本地运行

要求 Rust 1.90+、FFmpeg 和 yt-dlp。网易云功能要求可访问的 `api-enhanced` 服务。

```powershell
Copy-Item .env.example .env
# 编辑 .env，至少填写 TELEGRAM_BOT_TOKEN
$env:TELEGRAM_BOT_TOKEN="..."
$env:NETEASE_API_BASE="http://127.0.0.1:3000"
cargo run --release
```

未设置 `WEBHOOK_URL` 时使用 Polling；设置后监听 `WEBHOOK_HOST:WEBHOOK_PORT` 并向 Telegram 注册该地址。

## Docker Compose

```bash
cp .env.example .env
# 填入 TELEGRAM_BOT_TOKEN
docker compose up -d --build
```

启用 Redis：

```bash
REDIS_URL=redis://redis:6379 docker compose --profile redis up -d --build
```

`api-enhanced` 作为独立容器运行，版本可通过 `NETEASE_API_IMAGE` 覆盖。Bot 即使暂时无法连接 Redis 或某个 Provider，上述其他 Provider 仍可工作。

## 凭证

- `BILIBILI_COOKIE`：提高 B站清晰度并解析需要登录的内容。
- `YOUTUBE_COOKIES_FILE`：yt-dlp Netscape cookie 文件路径。
- `YOUTUBE_API_KEY`：使用官方 Data API 获取统计信息；未设置时走免费回退。
- `PIXIV_REFRESH_TOKEN`：配置后优先使用认证 App API；未配置时使用免费 Pixiv Web API。两种路径都可将 ugoira 帧包转换为 MP4。

不要提交 `.env`、cookies 或 token。所有环境变量参见 `.env.example`。

## 开发与验收

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --locked
```

网络 Provider 测试采用固定响应，不依赖生产服务；真实端到端测试需要自行提供 Bot token 和平台凭证。

## 许可证

主项目为 GPL-3.0-only。开发时使用的 `telegram-bili-feed-helper/` 上游参考 checkout 不纳入本仓库；`api-enhanced` 是通过 HTTP 调用的独立 MIT 服务。
