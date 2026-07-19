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
- `/login bili`：管理员私聊扫码登录 Bilibili，并立即更新持久化 Cookie。
- `/login netease`：管理员私聊扫码登录网易云音乐，并立即更新持久化 Cookie。
- 启用 BotFather Inline Mode 后，可通过 `@botname <url>` 生成解析预览。

默认视频选择 H.264/AAC 且不高于 720p。超过 Telegram 大小限制时会用 FFmpeg 压缩；仍超限则退回文本和原链接。成功上传的 `file_id` 存入 SQLite，重复发送不再下载。

## 快速启动：Docker Compose（推荐）

Docker 镜像发布到 `ghcr.io/congmiaofactory/congmiao-feedbot`，不需要在服务器上编译 Rust。

如果 GHCR Package 保持为 Private，先使用具有 `read:packages` 权限的 GitHub PAT 登录；Package 设为 Public 后可跳过此步：

```bash
export GHCR_USER=你的GitHub用户名
export GHCR_TOKEN=你的GitHub_PAT
echo "$GHCR_TOKEN" | docker login ghcr.io -u "$GHCR_USER" --password-stdin
```

```bash
git clone https://github.com/CongMiaoFactory/congmiao-feedbot.git
cd congmiao-feedbot
cp .env.example .env
mkdir -p secrets
nano .env
```

至少填写：

```env
TELEGRAM_BOT_TOKEN=1234567890:xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
```

然后拉取镜像并启动：

```bash
docker compose pull
docker compose up -d
docker compose logs -f feedbot
```

更新：

```bash
git pull
docker compose pull
docker compose up -d
```

启用 Redis：

```bash
sed -i 's|# REDIS_URL=|REDIS_URL=redis://redis:6379|' .env
docker compose --profile redis up -d
```

Compose 会自动启动网易云 API sidecar，并在容器内将 `NETEASE_API_BASE` 设为 `http://netease-api:3000`。可通过 `NETEASE_API_IMAGE` 覆盖 sidecar 镜像。

### 在 Compose 中启用本地 Telegram Bot API

先到 [my.telegram.org/apps](https://my.telegram.org/apps) 创建应用，取得 `api_id` 和 `api_hash`，然后修改 `.env`：

```env
TELEGRAM_BOT_TOKEN=从BotFather取得的BotToken
TELEGRAM_API_ID=12345678
TELEGRAM_API_HASH=你的api_hash
TELEGRAM_API_URL=http://telegram-bot-api:8081
LOCAL_MODE=true
TELEGRAM_REQUEST_TIMEOUT_SECS=600
```

使用 `telegram-local` profile 启动：

```bash
docker compose --profile telegram-local pull
docker compose --profile telegram-local up -d
docker compose logs -f telegram-bot-api feedbot
```

`telegram-bot-api` 仅将 `8081` 端口绑定到宿主机的 `127.0.0.1`，feedbot 通过 Compose 内部服务名访问。数据保存在 `telegram-bot-api-data` volume。恢复官方 Telegram Bot API 时，清空 `TELEGRAM_API_URL`、设置 `LOCAL_MODE=false`，再执行普通的 `docker compose up -d`。

本地 Bot API 处理大媒体时可能超过 teloxide 默认的短请求时限，项目默认将 Telegram 请求超时设为 600 秒，可用 `TELEGRAM_REQUEST_TIMEOUT_SECS` 调整。

## Linux x64 Release 启动

GitHub Release 提供静态链接的 `x86_64-unknown-linux-musl` 程序。运行时仍需要 FFmpeg 和 yt-dlp。

```bash
sudo apt-get update
sudo apt-get install -y ffmpeg python3 python3-pip curl
sudo pip3 install --break-system-packages -U yt-dlp

VERSION=v0.2.4
curl -LO "https://github.com/CongMiaoFactory/congmiao-feedbot/releases/download/${VERSION}/congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz"
curl -LO "https://github.com/CongMiaoFactory/congmiao-feedbot/releases/download/${VERSION}/congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz.sha256"
sha256sum -c "congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz.sha256"

mkdir congmiao-feedbot
tar -xzf "congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz" -C congmiao-feedbot
cd congmiao-feedbot
cp .env.example .env
nano .env
chmod +x congmiao-feedbot
set -a
source .env
set +a
./congmiao-feedbot
```

如果 GitHub 仓库是 Private，上述两条 `curl` 命令需增加仓库读取令牌：

```bash
curl -fL -H "Authorization: Bearer ${GITHUB_TOKEN}" -O "https://github.com/CongMiaoFactory/congmiao-feedbot/releases/download/${VERSION}/congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz"
curl -fL -H "Authorization: Bearer ${GITHUB_TOKEN}" -O "https://github.com/CongMiaoFactory/congmiao-feedbot/releases/download/${VERSION}/congmiao-feedbot-${VERSION}-linux-x86_64.tar.gz.sha256"
```

非 Docker 方式需要自行启动网易云 sidecar，并保持：

```env
NETEASE_API_BASE=http://127.0.0.1:3000
```

### systemd 守护进程

将解压后的目录移到 `/opt/congmiao-feedbot`，然后创建 `/etc/systemd/system/congmiao-feedbot.service`：

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

```bash
sudo useradd --system --home /opt/congmiao-feedbot --shell /usr/sbin/nologin feedbot
sudo chown -R feedbot:feedbot /opt/congmiao-feedbot
sudo systemctl daemon-reload
sudo systemctl enable --now congmiao-feedbot
sudo journalctl -u congmiao-feedbot -f
```

未设置 `WEBHOOK_URL` 时默认使用 Polling，不需要公网端口或 HTTPS。在群聊中自动解析时，还需在 BotFather 中通过 `/setprivacy` 关闭 Privacy Mode。

## 从源码运行

要求 Rust 1.90+、FFmpeg 和 yt-dlp。

```bash
cp .env.example .env
# 编辑 .env，至少填写 TELEGRAM_BOT_TOKEN
set -a && source .env && set +a
cargo run --release
```

## 凭证

- `ADMIN_USER_ID`：唯一允许执行 `/login` 的 Telegram 数字用户 ID。未配置时私聊发送 `/login`，Bot 会回复当前用户 ID。
- Bilibili：推荐私聊发送 `/login bili`，使用客户端扫码确认。无需手动填写 Cookie。
- 网易云音乐：推荐私聊发送 `/login netease`，使用客户端扫码确认。无需手动填写 Cookie。
- 扫码取得的 Cookie 存入 SQLite `provider_credentials`，重启后仍然有效；Provider 会立即读取新凭证，无需重启 Bot。
- `BILIBILI_COOKIE`、`NETEASE_COOKIE`：仅作为首次启动或扫码不可用时的可选回退值。
- `YOUTUBE_COOKIES_FILE`：yt-dlp Netscape cookie 文件路径。
- `YOUTUBE_API_KEY`：使用官方 Data API 获取统计信息；未设置时走免费回退。
- `PIXIV_REFRESH_TOKEN`：配置后优先使用认证 App API；未配置时使用免费 Pixiv Web API。两种路径都可将 ugoira 帧包转换为 MP4。
- `MEDIA_SPOILER_MODE`：媒体遮罩模式，默认 `auto`。`auto` 会自动遮罩 X/Pixiv 标记为敏感或 R18 的图片、视频和动图；`always` 遮罩所有图片/视频；`off` 关闭遮罩。敏感内容即使使用 `/file` 也会按可遮罩的媒体格式发送。

不要提交 `.env`、cookies 或 token。所有环境变量参见 `.env.example`。

### 扫码登录流程

1. 私聊 Bot 发送 `/login`。若尚未设置管理员，复制 Bot 回复的数字用户 ID。
2. 在 `.env` 填写 `ADMIN_USER_ID=<数字ID>`，只需配置一次并重启 Bot。
3. 私聊发送 `/login bili` 或 `/login netease`。
4. 使用对应手机客户端扫描二维码并确认；Bot 会回复“Cookie 已持久化并立即生效”。

群聊中的 `/login` 不会展示二维码，非管理员也不能更新公共 Bot 的平台凭证。Cookie 失效后重新扫码即可覆盖旧值。

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
