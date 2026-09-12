# CongMiaoFeedBot

Rust Telegram bot that extracts media from website links and pushes subscribed feeds.

Requires **ffmpeg** / **ffprobe** on `PATH`, and **yt-dlp** for YouTube.

## Run

```bash
export FEEDBOT__TELEGRAM__TOKEN=123456:ABC...
# Optional: 32-byte master key (64 hex chars). If unset, a key is generated
# at data/master.key (next to the SQLite file) and reused on later starts.
# export FEEDBOT__SECRETS__MASTER_KEY=$(openssl rand -hex 32)
# Optional: yt-dlp binary if not on PATH
# export FEEDBOT__YTDLP__BIN=yt-dlp

cargo run -p feedbot
```

Local development uses long polling (`telegram.mode = "polling"`). Production can switch to webhook:

```bash
export FEEDBOT__TELEGRAM__MODE=webhook
export FEEDBOT__TELEGRAM__WEBHOOK__PUBLIC_URL=https://example.com
export FEEDBOT__TELEGRAM__WEBHOOK__SECRET_TOKEN=...
```

## Commands

- `/help` / `/help pixiv` / `/help x` / `/help youtube` — command list and per-platform `+`/`-` option table (generated from extractor specs)
- `/login pixiv` — private-chat login (PHPSESSID cookie)
- `/login x` — private-chat login (`auth_token` / `ct0` cookie)
- `/login youtube` — private-chat login (YouTube Cookie / Netscape cookies.txt). Aliases: `yt`, `ytb`, `ytbm`
- `/login bili` — private-chat Bilibili QR login. Aliases: `bilibili`, `b站`
- `/login netease` — private-chat NetEase Cloud Music QR login (needs sidecar). Aliases: `ncm`, `163`, `网易云`
- `/set pixiv +q:original` — per-user/per-chat default options
- Send a Pixiv, X, YouTube, Bilibili, or NetEase link to extract:
  - `https://www.pixiv.net/artworks/...`
  - `https://x.com/{user}/status/{id}`
  - `https://youtube.com/watch?v=...` / `https://youtu.be/{id}` — video (`+q:1080|720|480`)
  - `https://music.youtube.com/watch?v=...` — audio with cover
  - live streams send the cover image; ended VODs send video
  - Bilibili `BV`/`AV` / bangumi / live cover / opus / audio / column (`+q:1080|720|480`)
  - NetEase song (audio + cover), MV, album/playlist cover + track list

Age-gated or bot-walled YouTube URLs need `/login youtube`. Bilibili `-101` and NetEase login-gated APIs need the matching `/login`.

## NetEase sidecar

QR login and extract talk to [ncm-api](https://github.com/moefurina/ncm-api) (Docker image `moefurina/ncm-api`) at `http://127.0.0.1:3000` by default:

```toml
[netease]
api_base = "http://127.0.0.1:3000"
```

If the sidecar is down, the bot reports `未连接网易云 API` rather than treating cookies as expired.

QR login is private-chat only. Scan the PNG, confirm in the app, or send `取消` / `/cancel`.

## Adding a site

1. Create `crates/extractors/src/sites/<name>.rs` implementing `Extractor` (declare `options()`, optional `auth()`)
2. Register it in `crates/extractors/src/registry.rs`

Do not change the command layer, help text, option parser, credential storage, or cache key logic.
