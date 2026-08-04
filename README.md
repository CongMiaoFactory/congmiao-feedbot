# Congmiao FeedBot

面向个人和小团队自托管的 Telegram 多平台内容解析 Bot。把支持的平台链接发送给 Bot，Bot 会提取正文、作者、统计信息和媒体，并在 Telegram 的限制范围内发送图片、视频、音频或文件。

- 当前 Cargo 版本：`0.2.17`
- 默认接收模式：Telegram Polling
- 持久化：本地 SQLite
- 推荐部署：Docker Compose
- 项目地址：<https://github.com/CongMiaoFactory/congmiao-feedbot>

> Bilibili、Pixiv、FxTwitter、网易云等接口部分属于第三方 Web API，字段、限流和鉴权行为可能变化。程序会尽量兼容已知结构，并在接口不可用时降级；不能保证绕过平台的登录限制或风控策略。

## 功能概览

### 支持的平台

| 平台 | 支持内容 | 主要数据来源 |
| --- | --- | --- |
| X / Twitter | 单帖、引用帖、图片、视频 | FxTwitter API |
| YouTube | 视频、Shorts、直播回放 | YouTube Data API（可选）、oEmbed、yt-dlp |
| Pixiv | 插画、漫画、多页作品、ugoira | Pixiv Web API；Refresh Token 可选 |
| 哔哩哔哩 | BV/AV 视频、动态、Opus、转发、直播、音频、专栏 | Bilibili Web API |
| 网易云音乐 | 歌曲、专辑、歌单、MV | 独立 HTTP sidecar |

### Bilibili 动态

支持以下链接形式：

```text
https://www.bilibili.com/opus/123...
https://m.bilibili.com/opus/123...?unique_k=...
https://www.bilibili.com/dynamic/123...
https://t.bilibili.com/123...
```

动态解析支持：

- 纯文字动态
- 图片动态和完整 Opus 正文
- 转发动态及原动态媒体
- 投稿视频、番剧、专栏、音频、直播卡片
- `MAJOR_TYPE_LIVE_RCMD` 直播开播推荐卡
- 未知动态类型的文字、摘要和封面降级

Bilibili 请求层会：

- 使用浏览器请求头，避免默认机器人 `User-Agent` 触发 `-352`
- 在动态请求前补充 `buvid3` / `buvid4` 设备 Cookie
- 收到 `-352` 时刷新设备标识并仅重试一次
- 保留已有 `SESSDATA`、`bili_jct` 等账号 Cookie
- 在 Opus 完整正文不可用时回退到动态摘要和已有图片
- 为动态图片携带 Referer，并使用稳定缓存键复用已上传媒体

Bilibili 视频还支持 DASH 音视频选择、默认 480p 上限、H.264/AAC 优先、清晰度降级和 CDN 备用地址。

### 媒体处理

- 默认视频清晰度最高不超过 480p，可用 `/video 720p` 等命令上调。
- 下载支持超时、失败重试、HTTP Range 续传和 Bilibili CDN fallback。
- 超尺寸、超长宽比或不适合 Telegram Photo 的图片会生成合规预览。
- 回复图片预览发送 `/file`，可以取回对应原图文件。
- 超大视频会尝试使用 FFmpeg 压缩或降低清晰度。
- 成功上传的 Telegram `file_id` 和回复媒体映射会写入 SQLite，重复发送可减少重复上传。
- 长正文会使用 Telegram `expandable` blockquote 自动折叠；正文空间会根据标题、作者和统计信息动态分配，并尽量在段落或空白边界截断。
- 媒体组分批发送时，caption 只附加到最后一批，避免重复显示。
- 专辑和歌单最多展示前 30 首；直播默认发送信息和封面。

## 部署前准备

### 创建 Telegram Bot

1. 在 Telegram 中打开 [@BotFather](https://t.me/BotFather)。
2. 发送 `/newbot`，保存得到的 `TELEGRAM_BOT_TOKEN`。
3. 如果要在群聊中自动识别普通消息里的链接，执行 `/setprivacy` 并选择 `Disable`。
4. 如需 Inline Mode，可执行 `/setinline`。
5. 如果需要扫码登录 Bilibili 或网易云，准备管理员的 Telegram 数字用户 ID。

默认使用 Polling，因此不需要公网 IP、域名、HTTPS 证书或入站端口。只有设置 `WEBHOOK_URL` 后才会切换到 Webhook。

### 外部依赖

| 依赖 | 用途 | Docker 镜像是否自带 |
| --- | --- | --- |
| FFmpeg / ffprobe | 图片预览、视频压缩、媒体探测 | 是 |
| yt-dlp | YouTube 下载或回退解析 | 是 |
| 网易云 sidecar | 网易云音乐解析 | Compose 会自动启动 |
| 本地 Telegram Bot API | 突破官方 Bot API 的文件上传限制 | 仅使用 `telegram-local` profile 时启动 |

## 快速开始：Docker Compose

### 1. 获取项目并配置 Token

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

建议同时配置管理员：

```env
ADMIN_USER_ID=你的Telegram数字用户ID
```

### 2. 启动

```bash
docker compose pull
docker compose up -d
docker compose logs -f feedbot
```

Compose 默认启动两个服务：

- `feedbot`：主 Bot
- `netease-api`：网易云音乐解析 sidecar

默认数据卷：

- `feedbot-data`：SQLite 数据库、Telegram `file_id` 缓存和登录凭证
- `./secrets` → `/app/secrets:ro`：YouTube cookies 等可选密钥文件

### 3. 更新

```bash
git pull
docker compose pull
docker compose up -d
```

如果只想重新创建主 Bot：

```bash
docker compose up -d --force-recreate feedbot
```

### 4. 常用运维命令

```bash
# 查看全部服务状态
docker compose ps

# 查看主 Bot 日志
docker compose logs -f feedbot

# 查看网易云 sidecar 日志
docker compose logs -f netease-api

# 重启主 Bot
docker compose restart feedbot

# 停止服务
docker compose down
```

### 私有 GHCR 镜像

如果仓库或 GHCR 包是 Private，先登录：

```bash
export GHCR_USER=你的GitHub用户名
export GHCR_TOKEN=具备read:packages权限的PAT
echo "$GHCR_TOKEN" | docker login ghcr.io -u "$GHCR_USER" --password-stdin
docker compose pull
docker compose up -d
```

也可以通过 `FEEDBOT_IMAGE` 指定其他镜像：

```bash
FEEDBOT_IMAGE=ghcr.io/your-org/congmiao-feedbot:latest docker compose up -d
```

## 大文件：本地 Telegram Bot API

官方 Telegram Bot API 的普通 Bot 上传限制通常约为 50 MB。需要处理更大视频时，可以启动 Compose 中的本地 Bot API 服务。

### 配置步骤

1. 到 [my.telegram.org/apps](https://my.telegram.org/apps) 创建应用，获取 `api_id` 和 `api_hash`。
2. 在 `.env` 中填写：

```env
TELEGRAM_API_ID=12345678
TELEGRAM_API_HASH=你的api_hash
TELEGRAM_API_PORT=8081
TELEGRAM_API_URL=http://telegram-bot-api:8081
LOCAL_MODE=true
TELEGRAM_REQUEST_TIMEOUT_SECS=600
```

3. 启动 profile：

```bash
docker compose --profile telegram-local up -d telegram-bot-api feedbot
docker compose logs -f telegram-bot-api feedbot
```

本地 Bot API 只绑定宿主机 `127.0.0.1:8081`，数据保存在 `telegram-bot-api-data` 卷。恢复官方 API 时，清空 `TELEGRAM_API_URL`，将 `LOCAL_MODE=false`，再执行：

```bash
docker compose --profile telegram-local up -d feedbot
```

## Linux x86_64 二进制

适合不使用 Docker 的 Linux x86_64 服务器。

### 安装依赖

```bash
sudo apt-get update
sudo apt-get install -y ca-certificates ffmpeg python3 python3-pip curl
sudo pip3 install --break-system-packages -U yt-dlp
```

网易云功能需要单独运行 sidecar，例如：

```bash
docker run -d \
  --name netease-api \
  --restart unless-stopped \
  -p 127.0.0.1:3000:3000 \
  moefurina/ncm-api:latest
```

### 下载并启动

```bash
VERSION=v0.2.17
curl -fLO "https://github.com/CongMiaoFactory/congmiao-feedbot/releases/download/${VERSION}/congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz"
curl -fLO "https://github.com/CongMiaoFactory/congmiao-feedbot/releases/download/${VERSION}/congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz.sha256"
sha256sum -c "congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz.sha256"

mkdir -p /opt/congmiao-feedbot
tar -xzf "congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz" -C /opt/congmiao-feedbot
cd /opt/congmiao-feedbot
cp .env.example .env
chmod +x congmiao-feedbot
```

`.env` 至少填写：

```env
TELEGRAM_BOT_TOKEN=你的Token
NETEASE_API_BASE=http://127.0.0.1:3000
DATABASE_URL=sqlite://data/feedbot.db?mode=rwc
TEMP_DIR=/opt/congmiao-feedbot/tmp
```

启动：

```bash
set -a
source .env
set +a
./congmiao-feedbot
```

### systemd 服务

创建用户和目录：

```bash
sudo useradd --system --home /opt/congmiao-feedbot --shell /usr/sbin/nologin feedbot
sudo chown -R feedbot:feedbot /opt/congmiao-feedbot
```

写入 `/etc/systemd/system/congmiao-feedbot.service`：

```ini
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

启用并查看日志：

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now congmiao-feedbot
sudo journalctl -u congmiao-feedbot -f
```

## 源码运行

要求：

- Rust `1.90` 或更高版本
- FFmpeg 和 `ffprobe`
- `yt-dlp`
- 如需网易云功能，另行运行网易云 sidecar

```bash
git clone https://github.com/CongMiaoFactory/congmiao-feedbot.git
cd congmiao-feedbot
cp .env.example .env
# 编辑 .env，填写 TELEGRAM_BOT_TOKEN
set -a
source .env
set +a
cargo run --release
```

如果本机没有 `cargo`，请先安装 Rust：

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

## 配置参考

所有变量都可以放在 `.env` 中。`docker-compose.yml` 还支持少量只用于 Compose 的变量。

### 必填和存储

| 变量 | 说明 | 默认值 |
| --- | --- | --- |
| `TELEGRAM_BOT_TOKEN` | BotFather Token；也兼容旧变量 `TOKEN` | 无，必填 |
| `DATABASE_URL` | SQLite 数据库 URL | `sqlite://data/feedbot.db?mode=rwc` |
| `TEMP_DIR` | 临时文件目录 | `tmp` |
| `ADMIN_USER_ID` | 允许执行 `/login` 的 Telegram 数字用户 ID | 空 |

### Telegram 和网络模式

| 变量 | 说明 | 默认值 |
| --- | --- | --- |
| `WEBHOOK_URL` | 填写完整公网 HTTPS 地址后启用 Webhook；留空使用 Polling | 空 |
| `WEBHOOK_HOST` | Webhook 监听 IP | `0.0.0.0` |
| `WEBHOOK_PORT` | Webhook 监听端口 | `8080` |
| `TELEGRAM_API_URL` | 自建 Telegram Bot API 服务根地址 | 空，使用官方 API |
| `LOCAL_MODE` | 按本地 Bot API 大文件上限处理 | `false` |
| `TELEGRAM_REQUEST_TIMEOUT_SECS` | Telegram 请求超时时间 | `600` |

Webhook 模式需要同时保证：

- `WEBHOOK_URL` 是 Telegram 可访问的公网 HTTPS 地址
- 反向代理将请求转发到 `WEBHOOK_HOST:WEBHOOK_PORT`
- 防火墙允许对应入站端口

### 媒体处理

| 变量 | 说明 | 默认值 |
| --- | --- | --- |
| `MEDIA_DOWNLOAD_TIMEOUT_SECS` | 单次上游媒体下载超时 | `120` |
| `MEDIA_DOWNLOAD_RETRIES` | 下载失败重试次数；支持 Range 续传 | `3` |
| `MEDIA_PHOTO_SOURCE_MAX_SIZE_MB` | 生成图片预览时允许读取的源文件上限 | `200` |
| `MEDIA_SPOILER_MODE` | `auto`、`always` 或 `off` | `auto` |
| `FFMPEG_PATH` | FFmpeg 可执行文件路径 | `ffmpeg` |
| `FFPROBE_PATH` | ffprobe 可执行文件路径 | `ffprobe` |
| `YT_DLP_PATH` | yt-dlp 可执行文件路径 | `yt-dlp` |
| `UPLOAD_WORKERS` | 并发媒体处理数 | `4` |
| `MAX_QUEUE_SIZE` | 同时进入媒体发送流程的任务数 | `200` |

`MEDIA_SPOILER_MODE=auto` 只对上游标记为敏感的 X/Pixiv 内容自动遮罩；`always` 遮罩所有图片和视频；`off` 关闭自动遮罩。消息中使用 `+sp` 可以单次强制遮罩。

### 平台 API 和凭证

| 变量 | 说明 | 默认值 |
| --- | --- | --- |
| `FXTWITTER_API_BASE` | FxTwitter API 根地址 | `https://api.fxtwitter.com` |
| `PIXIV_WEB_API_BASE` | Pixiv Web API 根地址 | `https://www.pixiv.net` |
| `PIXIV_REFRESH_TOKEN` | 可选 Pixiv App API 凭证 | 空 |
| `YOUTUBE_API_KEY` | 可选 YouTube Data API Key | 空 |
| `YOUTUBE_COOKIES_FILE` | yt-dlp Netscape cookies 文件 | 空 |
| `NETEASE_API_BASE` | 网易云 sidecar 根地址 | `http://127.0.0.1:3000`（源码默认） |
| `NETEASE_COOKIE` | 网易云初始 Cookie；推荐扫码登录 | 空 |
| `BILIBILI_COOKIE` | Bilibili 初始 Cookie；推荐扫码登录 | 空 |
| `BILIBILI_CDN` | 视频 CDN 策略 | `baseUrl` |

Bilibili API 地址可按需覆盖：

| 变量 | 默认值 |
| --- | --- |
| `BILIBILI_PASSPORT_BASE` | `https://passport.bilibili.com` |
| `BILIBILI_API_BASE` | `https://api.bilibili.com` |
| `BILIBILI_LIVE_API_BASE` | `https://api.live.bilibili.com` |
| `BILIBILI_WWW_BASE` | `https://www.bilibili.com` |

### Bilibili CDN

`BILIBILI_CDN` 控制视频主地址和备用地址的选择，不影响动态详情 API：

```env
BILIBILI_CDN=baseUrl    # 默认，优先 API 返回的主地址
BILIBILI_CDN=backupUrl  # 优先 API 返回的备用地址
BILIBILI_CDN=ali       # 阿里镜像
BILIBILI_CDN=cos       # 腾讯云镜像
BILIBILI_CDN=hw        # 华为镜像
BILIBILI_CDN=akamai    # 海外 Akamai 镜像
```

还支持 `alib`、`alio1`、`cosb`、`coso1`、`hwb`、`hwo1`、`08c`、`08h`、`08ct`、`tf_hw`、`tf_tx`、`aliov`、`cosov`、`hwov` 和 `hk_bcache`。镜像失败后仍会继续尝试原始地址和 API 备用地址。

### Compose 专用变量

| 变量 | 说明 | 默认值 |
| --- | --- | --- |
| `FEEDBOT_IMAGE` | 主 Bot 镜像 | `ghcr.io/congmiaofactory/congmiao-feedbot:latest` |
| `NETEASE_API_IMAGE` | 网易云 sidecar 镜像 | `moefurina/ncm-api:latest` |
| `NETEASE_ENABLE_GENERAL_UNBLOCK` | 网易云 sidecar 通用解锁 | `true` |
| `NETEASE_ENABLE_FLAC` | 网易云 sidecar FLAC 支持 | `true` |
| `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` | 本地 Bot API 应用凭证 | 空 |
| `TELEGRAM_API_PORT` | 本地 Bot API 宿主机端口 | `8081` |
| `TELEGRAM_BOT_API_IMAGE` | 本地 Bot API 镜像 | `aiogram/telegram-bot-api:latest` |

### 用户限流

```env
REQUEST_LIMIT_COUNT=0   # 0 表示关闭
REQUEST_LIMIT_TTL=86400 # 窗口秒数
```

限流按用户（群聊中没有用户信息时按聊天）计数，使用本地内存，不会跨进程共享。

## 登录与凭证

推荐使用 Bot 私聊扫码，不要手抄或提交 Cookie。

1. 启动 Bot 后私聊发送 `/login`。
2. 如果没有设置 `ADMIN_USER_ID`，Bot 会返回你的数字用户 ID。
3. 将该 ID 写入 `.env` 的 `ADMIN_USER_ID`，重启 Bot。
4. 私聊发送 `/login bili` 或 `/login netease`。
5. 使用对应客户端扫码并确认；成功后凭证会写入 SQLite，并立即对当前进程生效。

注意：

- `/login` 只能在私聊使用，避免二维码泄露。
- 只有 `ADMIN_USER_ID` 对应的用户可以更新公共 Bot 凭证。
- Bilibili 登录 Cookie 会与自动获取的 `buvid3` / `buvid4` 合并，不会覆盖账号 Cookie。
- 如果 `-101`、登录失效或受限内容无法解析，重新执行 `/login bili`。
- 不要把 `.env`、`secrets/`、cookies、Token 或数据库文件提交到 Git。

YouTube cookies 文件示例：

```env
YOUTUBE_COOKIES_FILE=/app/secrets/youtube-cookies.txt
```

将 Netscape 格式的文件放在宿主机 `./secrets/youtube-cookies.txt`，Compose 会以只读方式挂载。

## 使用方法

### 直接解析

在私聊或群聊中发送一个或多个支持的链接即可：

```text
https://www.bilibili.com/opus/1231302450179211264?unique_k=2333
https://www.pixiv.net/artworks/123456789
```

也可以回复一条包含链接的消息，再发送命令或参数。

### 命令和参数

| 用法 | 作用 |
| --- | --- |
| `/start`、`/help` | 显示帮助 |
| `/parse <url>` | 显式解析链接 |
| `/video <url>` | 按默认最高 480p 解析视频 |
| `/video 720p <url>` | 设置视频清晰度上限 |
| `/cover <url>` | 只发送封面 |
| `/file <url>` | 按文件发送媒体，避免作为 Telegram Photo/Video 发送 |
| `/login` | 查看登录权限提示或默认开始 Bilibili 登录 |
| `/login bili` | Bilibili 扫码登录 |
| `/login netease` | 网易云扫码登录 |

清晰度支持 `360p`、`480p`、`720p`、`1080p`；最终质量还会受源站、Telegram 上限和本地 Bot API 配置影响。

### `+sp` 和 `+pN`

- `+sp`：本次强制使用 spoiler，例如 `https://... +sp`。
- `+p2`：只发送第 2 页或第 2 张媒体，例如 `https://www.pixiv.net/artworks/123456+p2`。
- 回复 Bot 的图片预览后发送 `/file`：取回该预览对应的原图文件。
- 参数可以组合：`https://www.pixiv.net/artworks/123456+p2+sp`。
- `+pN` 对只提供单个媒体的内容没有额外效果。

### Inline Mode

在 BotFather 执行 `/setinline` 后，可以在任意聊天输入：

```text
@你的Bot https://www.bilibili.com/opus/123...
```

Inline 结果依赖已经缓存或可直接使用的媒体 `file_id`；不能上传的媒体可能只显示文本结果。

## 故障排查

### 通用检查顺序

```bash
# Docker
docker compose ps
docker compose logs --tail=200 feedbot

# systemd
sudo systemctl status congmiao-feedbot
sudo journalctl -u congmiao-feedbot -n 200 --no-pager
```

### Bilibili `-352`

如果看到类似：

```text
上游请求受限，请稍后重试: Bilibili: -352
```

常见原因是旧版本使用了机器人 `User-Agent`，或者当前设备 Cookie 被 Bilibili 风控标记。当前实现的处理顺序是：

1. 动态请求使用浏览器 UA、`Origin`、`Referer` 和 Accept 头。
2. 首个动态请求前获取 `buvid3` / `buvid4`。
3. 第一次收到 `-352` 时刷新设备 Cookie。
4. 自动重试一次。
5. 仍然失败时提示稍后重试或通过 `/login bili` 更新登录状态。

处理步骤：

```bash
# Docker 部署先拉取包含修复的镜像
docker compose pull
docker compose up -d --force-recreate feedbot

docker compose logs -f feedbot
```

然后：

1. 确认使用的是当前修复版本，而不是旧容器或旧二进制。
2. 私聊发送 `/login bili`，重新扫码。
3. 确认服务器没有把 `api.bilibili.com` 替换成会删除请求头的代理。
4. 降低短时间内的并发请求，等待几分钟后再试。

当前实现可以解析以下带移动端域名和查询参数的链接，查询参数不会影响动态 ID：

```text
https://m.bilibili.com/opus/1231302450179211264?&unique_k=2333
```

### Bilibili 其他错误

| 错误 | 含义 | 处理 |
| --- | --- | --- |
| `-101` | 需要登录或登录失效 | `/login bili`，检查 `ADMIN_USER_ID` |
| `-352` | 风控校验失败 | 使用包含浏览器 Header 和设备 Cookie 修复的版本；必要时重新登录 |
| HTTP `412` / `429` | 请求频率或风控限制 | 等待后重试，减少并发，不要连续重启刷请求 |
| 内容不存在或 `item=null` | 删除、权限限制或 ID 无效 | 确认链接和登录账号可见性 |
| 视频能解析但下载失败 | CDN 或网络问题 | 尝试 `BILIBILI_CDN=backupUrl`、`ali`、`cos` 或 `hw` |

### 其他常见问题

| 现象 | 可能原因 | 处理 |
| --- | --- | --- |
| Bot 无响应 | 容器未启动或 Token 错误 | 检查 `docker compose ps` 和日志 |
| 群聊不自动解析 | Bot Privacy Mode 开启 | BotFather `/setprivacy` → `Disable` |
| 网易云失败 | sidecar 未启动或地址错误 | 检查 `netease-api` 日志和 `NETEASE_API_BASE` |
| YouTube 解析失败 | API quota 或需要登录 | 配置 `YOUTUBE_API_KEY` 或 `YOUTUBE_COOKIES_FILE` |
| Pixiv 受限 | Web API 或作品权限问题 | 配置 `PIXIV_REFRESH_TOKEN`，确认账号可见 |
| 图片只发送预览 | 原图超过 Telegram Photo 限制 | 回复预览发送 `/file` |
| 大视频发送失败 | 超过官方 Bot API 上限 | 启动本地 Telegram Bot API，并设置 `LOCAL_MODE=true` |
| Webhook 不工作 | URL、反代或端口不匹配 | 检查 HTTPS、公网可达性和 `WEBHOOK_PORT` |

## 数据、安全和备份

SQLite 数据库默认位于：

```text
data/feedbot.db
```

数据库包含：

- Telegram 媒体 `file_id` 缓存
- 回复媒体与原始媒体的映射
- Bilibili、网易云扫码登录后持久化的 Cookie
- 其他本地缓存数据

备份 Docker 数据卷前先停止 Bot，避免复制过程中数据库正在写入：

```bash
docker compose stop feedbot
# 按你的备份工具导出 feedbot-data 卷
docker compose start feedbot
```

安全建议：

- `.env` 权限建议设置为 `600`。
- `secrets/` 只读挂载，不要放入公开仓库。
- 不要在公开日志或 issue 中粘贴 `SESSDATA`、`bili_jct`、Telegram Token、Refresh Token 或 cookies。
- `ADMIN_USER_ID` 必须填写可信管理员的数字 ID。
- Webhook 模式建议在反向代理层启用 HTTPS 和访问日志脱敏。

## 开发与测试

安装 Rust 1.90+ 后执行：

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --locked
```

测试说明：

- Bilibili 动态、Opus、转发、内嵌视频和 `-352` 重试使用 Wiremock，不依赖实时接口。
- 媒体测试会覆盖图片预览、Range 续传、FFmpeg DASH 合并等行为。
- 真实平台接口可能变化，发布前仍建议用实际账号验证登录内容和媒体下载。

代码结构：

```text
src/
├── provider/       平台识别、解析和 Bilibili API 适配
├── telegram.rs     Telegram 命令、媒体发送和 Inline Mode
├── media.rs        下载、预览、压缩和媒体校验
├── credentials.rs  SQLite/内存凭证管理
├── login.rs        Bilibili/网易云扫码登录
├── storage.rs      SQLite 数据和媒体映射
├── cache.rs        内存缓存和用户限流
└── model.rs        Provider 与 Telegram 之间的公共模型
```

### AI 辅助开发

仓库提供了面向 AI 编码工具和大语言模型的项目上下文：

- [`AGENTS.md`](AGENTS.md)：编码代理必须遵守的架构规则、测试要求和安全边界。
- [`llms.txt`](llms.txt)：适合支持 `llms.txt` 的工具快速发现项目入口。
- [`docs/AI_CONTEXT.md`](docs/AI_CONTEXT.md)：完整调用链、模块职责、Provider 约定、存储、配置、部署和发布说明。

AI 文档用于帮助理解仓库，实际行为仍以源码、测试和部署配置为准。

## 发布

GitHub Actions 会在推送 `v*` 标签时构建 Linux x86_64 musl 二进制，并创建 GitHub Release，附件包括压缩包和 SHA-256 文件。

维护者发布流程：

```bash
# 修改 Cargo.toml 和 Cargo.lock 中的版本，并同步 README 示例
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --locked

git add Cargo.toml Cargo.lock README.md
git commit -m "chore: release vX.Y.Z"
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin main
git push origin vX.Y.Z
```

发布工作流文件：`.github/workflows/release.yml`。

## 许可证

主项目使用 `GPL-3.0-only`。网易云 sidecar 是独立服务，其镜像和源代码许可证以对应上游项目为准。
