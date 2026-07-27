# Congmiao FeedBot

Telegram 多平台内容解析 Bot。把链接发给 Bot，它会提取正文、作者、统计信息和媒体，并在 Telegram 限制内尽量直接转发图片、视频、音频。

适合个人/小团队自托管：默认 Polling，不需要公网域名；数据落在本地 SQLite；Docker 一键启动。

## 能做什么

| 平台 | 支持内容 | 数据来源 |
| --- | --- | --- |
| X / Twitter | 单帖、引用帖、图片、视频 | FxTwitter v2 API |
| YouTube | 视频、Shorts、直播回放 | Data API（可选）+ oEmbed + yt-dlp |
| Pixiv | 插画、漫画、多页、ugoira | Pixiv Web API（免费）/ App API（可选） |
| 哔哩哔哩 | BV/AV 视频、动态/Opus、直播封面、音频 | Bilibili Web API |
| 网易云音乐 | 歌曲、专辑、歌单、MV | 独立 HTTP sidecar |

补充说明：

- 默认视频清晰度：最高不超过 480p，优先 H.264/AAC；可用 `/video 720p` 等上调。
- Bilibili 会在下载前按码率、时长和 Telegram 上传上限自动降到 480p/360p。
- 媒体下载支持超时配置、失败重试、HTTP Range 续传、备用 CDN。
- 超尺寸图片按文件发送（不缩放）；超大视频会尝试 FFmpeg 压缩。
- 成功上传后的 Telegram `file_id` 写入 SQLite，重复发送可直接复用。
- 较长简介使用 Telegram 可展开引用；媒体 caption 仍受 1024 字限制。
- 专辑/歌单只展示封面和前 30 首；直播只展示信息和封面。

## 部署方式怎么选

| 方式 | 适合谁 | 需要什么 |
| --- | --- | --- |
| **Docker Compose（推荐）** | 大多数自托管用户 | Docker / Docker Compose |
| Linux x64 二进制 | 不想用 Docker 的 Linux 服务器 | FFmpeg、yt-dlp、网易云 sidecar |
| 源码编译 | 开发者 / 需要改代码 | Rust 1.90+、FFmpeg、yt-dlp |

默认通信模式是 **Polling**：

- 不需要公网 IP、域名、HTTPS 证书
- 不需要开放入站端口
- 群聊自动解析时，还要在 BotFather 用 `/setprivacy` 关闭 Privacy Mode

只有你明确配置 `WEBHOOK_URL` 时才会切到 Webhook。

## 1. 准备 Telegram Bot

1. 在 Telegram 找 [@BotFather](https://t.me/BotFather)，发送 `/newbot`，拿到 `TELEGRAM_BOT_TOKEN`。
2. 建议再执行：
   - `/setprivacy` → Disable：允许群聊自动识别链接
   - `/setinline`：启用 Inline 预览（可选）
3. 记下你自己的 Telegram 数字用户 ID（后面配置管理员时用）：
   - 先把 Bot 跑起来
   - 私聊 Bot 发送 `/login`
   - Bot 会告诉你当前用户 ID

## 2. 推荐部署：Docker Compose

### 2.1 拉取项目并准备配置

```bash
git clone https://github.com/CongMiaoFactory/congmiao-feedbot.git
cd congmiao-feedbot
cp .env.example .env
mkdir -p secrets
```

编辑 `.env`，至少填写：

```env
TELEGRAM_BOT_TOKEN=1234567890:xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

可选但推荐：

```env
# 允许使用 /login 的管理员
ADMIN_USER_ID=你的Telegram数字ID

# 国内 Bilibili 下载不稳时可改
# BILIBILI_CDN=backupUrl
# 或 BILIBILI_CDN=ali / cos / hw
```

### 2.2 启动

如果 GHCR 镜像是 Private，先登录：

```bash
export GHCR_USER=你的GitHub用户名
export GHCR_TOKEN=具备read:packages权限的PAT
echo "$GHCR_TOKEN" | docker login ghcr.io -u "$GHCR_USER" --password-stdin
```

然后：

```bash
docker compose pull
docker compose up -d
docker compose logs -f feedbot
```

Compose 会同时启动：

- `feedbot`：主程序
- `netease-api`：网易云解析 sidecar（容器内地址自动设为 `http://netease-api:3000`）

数据目录：

- SQLite / 运行数据：Docker volume `feedbot-data`
- 可选密钥文件：宿主机 `./secrets` 挂载到 `/app/secrets`

### 2.3 更新

```bash
git pull
docker compose pull
docker compose up -d
```

### 2.4 常用运维命令

```bash
# 查看日志
docker compose logs -f feedbot

# 重启
docker compose restart feedbot

# 停止
docker compose down
```

## 3. 可选：本地 Telegram Bot API（上传更大文件）

官方 Bot API 对普通 Bot 的上传上限大约是 50MB。若你需要更大视频，可启用本地 `telegram-bot-api`（约 2GB）。

1. 到 [my.telegram.org/apps](https://my.telegram.org/apps) 创建应用，拿到 `api_id` / `api_hash`
2. 修改 `.env`：

```env
TELEGRAM_BOT_TOKEN=从BotFather拿到的Token
TELEGRAM_API_ID=12345678
TELEGRAM_API_HASH=你的api_hash
TELEGRAM_API_URL=http://telegram-bot-api:8081
LOCAL_MODE=true
TELEGRAM_REQUEST_TIMEOUT_SECS=600
```

3. 启动：

```bash
docker compose --profile telegram-local pull
docker compose --profile telegram-local up -d
docker compose logs -f telegram-bot-api feedbot
```

说明：

- 本地 Bot API 只监听 `127.0.0.1:8081`
- 数据保存在 `telegram-bot-api-data` volume
- 恢复官方 API：清空 `TELEGRAM_API_URL`，设 `LOCAL_MODE=false`，再 `docker compose up -d`

## 4. Linux 二进制部署

适合没有 Docker 的 Linux x86_64 机器。

### 4.1 安装运行依赖

```bash
sudo apt-get update
sudo apt-get install -y ffmpeg python3 python3-pip curl
sudo pip3 install --break-system-packages -U yt-dlp
```

网易云功能还需要单独启动 sidecar，例如：

```bash
docker run -d --name netease-api -p 3000:3000 moefurina/ncm-api:latest
```

### 4.2 下载并运行

```bash
VERSION=v0.2.13
curl -LO "https://github.com/CongMiaoFactory/congmiao-feedbot/releases/download/${VERSION}/congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz"
curl -LO "https://github.com/CongMiaoFactory/congmiao-feedbot/releases/download/${VERSION}/congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz.sha256"
sha256sum -c "congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz.sha256"

mkdir congmiao-feedbot
tar -xzf "congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz" -C congmiao-feedbot
cd congmiao-feedbot
cp .env.example .env
nano .env
```

`.env` 至少：

```env
TELEGRAM_BOT_TOKEN=你的Token
NETEASE_API_BASE=http://127.0.0.1:3000
```

启动：

```bash
chmod +x congmiao-feedbot
set -a
source .env
set +a
./congmiao-feedbot
```

若仓库/Release 为 Private：

```bash
curl -fL -H "Authorization: Bearer ${GITHUB_TOKEN}" -O "https://github.com/CongMiaoFactory/congmiao-feedbot/releases/download/${VERSION}/congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz"
curl -fL -H "Authorization: Bearer ${GITHUB_TOKEN}" -O "https://github.com/CongMiaoFactory/congmiao-feedbot/releases/download/${VERSION}/congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz.sha256"
```

### 4.3 systemd 守护

假设程序目录是 `/opt/congmiao-feedbot`：

```ini
# /etc/systemd/system/congmiao-feedbot.service
[Unit]
Description=Congmiao Telegram FeedBot
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=feedbot
Group=feedbot
WorkingDirectory=/opt/congmiao-feedbot
EnvironmentFile=/opt/congmiao-feedbot/.env
ExecStart=/opt/congmiao-feedbot/congmiao-feedbot
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```bash
sudo useradd --system --home /opt/congmiao-feedbot --shell /usr/sbin/nologin feedbot
sudo chown -R feedbot:feedbot /opt/congmiao-feedbot
sudo systemctl daemon-reload
sudo systemctl enable --now congmiao-feedbot
sudo journalctl -u congmiao-feedbot -f
```

## 5. 源码运行

```bash
cp .env.example .env
# 至少填写 TELEGRAM_BOT_TOKEN
set -a && source .env && set +a
cargo run --release
```

要求：

- Rust 1.90+
- FFmpeg / ffprobe
- yt-dlp
- 网易云 sidecar（如需网易云功能）

## 6. 配置说明

完整变量见 `.env.example`。这里只列自托管最常用项。

### 必填 / 强烈建议

| 变量 | 说明 | 默认 |
| --- | --- | --- |
| `TELEGRAM_BOT_TOKEN` | BotFather 发放的 Token | 无，必填 |
| `ADMIN_USER_ID` | 允许 `/login` 的管理员数字 ID | 空 |
| `DATABASE_URL` | SQLite 路径 | `sqlite://data/feedbot.db?mode=rwc` |
| `NETEASE_API_BASE` | 网易云 sidecar 地址 | Docker 内自动注入；二进制默认 `http://127.0.0.1:3000` |

### Telegram 与上传

| 变量 | 说明 | 默认 |
| --- | --- | --- |
| `WEBHOOK_URL` | 配置后切 Webhook；留空用 Polling | 空 |
| `WEBHOOK_HOST` / `WEBHOOK_PORT` | Webhook 监听地址 | `0.0.0.0` / `8080` |
| `TELEGRAM_API_URL` | 本地 Bot API 根地址 | 空（走官方） |
| `LOCAL_MODE` | 是否按本地 Bot API 的大文件上限工作 | `false` |
| `TELEGRAM_REQUEST_TIMEOUT_SECS` | 上传到 Telegram 的请求超时 | `600` |
| `MEDIA_DOWNLOAD_TIMEOUT_SECS` | 上游媒体单次下载超时 | `120` |
| `MEDIA_DOWNLOAD_RETRIES` | 下载失败重试次数（含 Range 续传） | `3` |
| `MEDIA_SPOILER_MODE` | `auto` / `always` / `off` | `auto` |

### 平台相关

| 变量 | 说明 |
| --- | --- |
| `BILIBILI_CDN` | Bilibili CDN 策略：`baseUrl`、`backupUrl`、`ali`、`cos`、`hw`、`akamai` 等 |
| `BILIBILI_COOKIE` / `NETEASE_COOKIE` | 可选初始 Cookie；更推荐 `/login` 扫码 |
| `YOUTUBE_API_KEY` | 可选，增强 YouTube 元数据 |
| `YOUTUBE_COOKIES_FILE` | yt-dlp Netscape cookie 文件 |
| `PIXIV_REFRESH_TOKEN` | 可选，走认证 App API |
| `FXTWITTER_API_BASE` | FxTwitter API 根地址 |
| `FFMPEG_PATH` / `FFPROBE_PATH` / `YT_DLP_PATH` | 外部工具路径 |
| `UPLOAD_WORKERS` | 并发媒体处理数 | 默认 `4` |
| `MAX_QUEUE_SIZE` | 排队上限 | 默认 `200` |
| `REQUEST_LIMIT_COUNT` | 用户限流次数；`0` 关闭 | 默认 `0` |

### Bilibili CDN 建议

```env
# 默认：API 主地址，再试 backupUrl
BILIBILI_CDN=baseUrl

# 更稳的通用选择
BILIBILI_CDN=backupUrl

# 指定运营商镜像
BILIBILI_CDN=ali
# BILIBILI_CDN=cos
# BILIBILI_CDN=hw
# 海外：akamai / aliov / cosov / hwov / hk_bcache
```

无论选哪个，失败后仍会继续尝试原始地址和 API 备用地址。

## 7. 登录与凭证

推荐用 Bot 私聊扫码，不要手抄 Cookie。

1. 私聊发送 `/login`，若未设置管理员，Bot 会返回你的数字 ID
2. 写入 `.env`：`ADMIN_USER_ID=你的ID`，重启一次
3. 私聊发送：
   - `/login bili`
   - `/login netease`
4. 用对应 App 扫码确认；成功后 Cookie 立即写入 SQLite，无需再重启

注意：

- 群聊中的 `/login` 不会展示二维码
- 非管理员不能更新公共 Bot 的平台凭证
- Cookie 失效后重新扫码即可覆盖
- 不要提交 `.env`、cookies、token

其他可选凭证：

- `YOUTUBE_COOKIES_FILE`：放到 `./secrets`，容器内例如 `/app/secrets/youtube-cookies.txt`
- `PIXIV_REFRESH_TOKEN`：需要时再配

## 8. 使用方法

在私聊或群聊中：

- 直接发送一个或多个支持的链接
- `/parse <url>`：显式解析
- `/video [360p|480p|720p|1080p] <url>`：指定视频清晰度上限，默认 480p
- `/file <url>`：按文件发送
- `/cover <url>`：只发封面
- 链接后加 `+sp`：本次强制遮罩，例如 `https://www.pixiv.net/artworks/123456 +sp`
- Pixiv 等多图链接后加 `+pN`：只发送第 N 页，例如 `https://www.pixiv.net/artworks/123456+p2` 或 `... 123456 +p2`
- 也可回复一条含链接的消息，单独发送 `+sp` / `+p2`
- 可组合：`https://www.pixiv.net/artworks/123456+p2+sp`
- 开启 Inline Mode 后：`@你的Bot <url>`

## 9. 自检与排障

### 启动成功的基本信号

日志中出现类似：

```text
初始化完成
```

并且私聊 Bot 发送任意支持链接后有回复。

### 常见问题

| 现象 | 可能原因 | 处理 |
| --- | --- | --- |
| Bot 无响应 | Token 错误 / 进程未启动 | 检查 `TELEGRAM_BOT_TOKEN` 与日志 |
| 群聊不自动解析链接 | Privacy Mode 仍开启 | BotFather `/setprivacy` → Disable |
| 网易云失败 | sidecar 未启动或地址错 | Docker 看 `netease-api`；二进制确认 `NETEASE_API_BASE` |
| Bilibili 下载中断/超时 | CDN 或网络不稳 | 设 `BILIBILI_CDN=backupUrl` 或 `ali/cos/hw`，适当增大 `MEDIA_DOWNLOAD_TIMEOUT_SECS` |
| 大视频发不出去 | 超过官方 50MB | 启用本地 Bot API，或依赖自动压缩/降清晰度 |
| 需要登录的内容失败 | Cookie 失效 | 私聊重新 `/login bili` 或 `/login netease` |
| YouTube 受限 | 需要登录 cookie | 配置 `YOUTUBE_COOKIES_FILE` |

### 开发自检

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --locked
```

网络相关测试使用 mock，不依赖真实平台服务。

## 10. 架构速览

- 主程序：Rust + Tokio + Teloxide
- 持久化：SQLite（`file_id` 缓存、扫码 Cookie、限流等）
- 本地内存缓存：解析结果短时缓存
- 外部工具：FFmpeg / ffprobe / yt-dlp
- 网易云：独立 sidecar，HTTP 调用

## 许可证

主项目为 GPL-3.0-only。网易云 sidecar（`api-enhanced` 一类镜像）是独立服务，许可证以其上游为准。
