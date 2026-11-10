# CongMiaoFeedBot

Rust Telegram bot that extracts media from website links and pushes subscribed feeds.

Requires **ffmpeg** / **ffprobe** on `PATH`, and **yt-dlp** plus a JavaScript runtime for YouTube: **Deno** by default, or another one yt-dlp supports via `FEEDBOT__YTDLP__JS_RUNTIME` (e.g. `quickjs`, `node`). The Docker image bundles all of them.

All settings live in `config/default.toml` and can be overridden with
`FEEDBOT__<SECTION>__<KEY>` environment variables (e.g. `FEEDBOT__FEED__TICK_SECS`).

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

### Docker

The image (about 285 MB unpacked) bundles a minimal ffmpeg/ffprobe built from source, yt-dlp and QuickJS-ng, and runs as an unprivileged user with the database in the `/data` volume:

```bash
docker build -t feedbot .
docker run -d --name feedbot \
  -e FEEDBOT__TELEGRAM__TOKEN=123456:ABC... \
  -e FEEDBOT__SECRETS__MASTER_KEY=$(openssl rand -hex 32) \
  -v feedbot-data:/data \
  feedbot
```

- Keep the master key in your secret store. If it is unset, the key is generated as `/data/master.key` next to the database (see below).
- ffmpeg, yt-dlp and QuickJS-ng are pinned in the `Dockerfile` (`FFMPEG_VERSION`, `YTDLP_VERSION`, `QJS_VERSION`) and checked against SHA-256 sums. When YouTube breaks, update the version and both sums of yt-dlp, then rebuild.
- QuickJS keeps the image small, but YouTube's JavaScript challenges take a few seconds longer than with Deno. To use Deno instead, install it in the image and set `FEEDBOT__YTDLP__JS_RUNTIME=deno`.
- ffmpeg is compiled with only the components the bot and yt-dlp use; the first build takes about two extra minutes, later builds reuse the cached stage.
- Builds for `amd64` and `arm64`.
- The bot shuts down cleanly on SIGTERM (`docker stop`) and Ctrl-C.
- NetEase needs the `ncm-api` sidecar (below). Point `FEEDBOT__NETEASE__API_BASE` at it, e.g. on a shared Docker network.

### Credential encryption

Saved logins are encrypted with ChaCha20-Poly1305 using the master key and bound
to their platform and owner. With the auto-generated `data/master.key` the key
sits next to the database, so anyone who can read the data directory can decrypt
the credentials: that setup only protects a leaked database file on its own.
In production pass `FEEDBOT__SECRETS__MASTER_KEY` from a secret store and keep
it out of the data volume. Losing the key makes saved logins unreadable; users
then need to `/login` again.

Local development uses long polling (`telegram.mode = "polling"`). Production can switch to webhook:

```bash
export FEEDBOT__TELEGRAM__MODE=webhook
export FEEDBOT__TELEGRAM__WEBHOOK__PUBLIC_URL=https://example.com
export FEEDBOT__TELEGRAM__WEBHOOK__SECRET_TOKEN=...
```

Posts with more than `output.summary_threshold` items are sent as several albums by default. With `output.overflow = "telegraph"`, a set made only of photos is published as one telegra.ph page instead, and the chat gets the caption plus the page link. Nothing is downloaded or uploaded for such posts. With `+sp` the link preview is turned off.

Telegraph no longer accepts image uploads, so the page links the original image URLs. Hosts that need a Referer or cookie (Pixiv, Bilibili) only show through a proxy host. A post that cannot be shown this way, or a Telegraph error, falls back to albums. The proxy in this example is a third-party service:

```bash
export FEEDBOT__OUTPUT__OVERFLOW=telegraph
export FEEDBOT__TELEGRAPH__IMAGE_PROXIES=i.pximg.net=i.pixiv.re
# Optional: keep every page under one account (otherwise one is created per start)
export FEEDBOT__TELEGRAPH__ACCESS_TOKEN=...
```

## Commands

- `/help` / `/help pixiv` / `/help x` / `/help youtube` — command list and per-platform `+`/`-` option table (generated from extractor specs)
- `/login pixiv` — private-chat login (PHPSESSID cookie)
- `/login x` — private-chat login (`auth_token` / `ct0` cookie)
- `/login youtube` — private-chat login (YouTube Cookie / Netscape cookies.txt). Aliases: `yt`, `ytb`, `ytbm`
- `/login bili` — private-chat Bilibili QR login. Aliases: `bilibili`, `b站`
- `/login netease` — private-chat NetEase Cloud Music QR login (needs sidecar). Aliases: `ncm`, `163`, `网易云`
- `/login <platform> global` / `/logout <platform> global` — operators listed in `telegram.admin_ids` manage the shared fallback credential
- `/logout <platform>` / `/accounts` — remove / list your own saved logins
- `/set pixiv +q:original` — default options. In a private chat this sets your personal defaults; in a group it sets the group default (group admins only). Personal defaults win over group defaults; `-q` clears a value
- `/sub <link> [+options]` — subscribe the current chat to a user page / channel / playlist. The first poll only records existing items; afterwards new items are pushed through the normal link pipeline with the given options. In groups only admins can `/sub` / `/unsub`
  - Subscribable: Pixiv user pages, YouTube channels / playlists, NetEase playlists / albums, Bilibili spaces (`space.bilibili.com/{uid}` for all dynamics, `/{uid}/video` for uploads only; video dynamics are pushed as the video), X profiles (`x.com/{user}` or `/{user}/media`: the user's own posts with media, no retweets; requires `/login x` by the subscriber first, and polls use that login)
- `/subs` / `/unsub <id>` — list / remove subscriptions of this chat
- `/cancel` — leave a pending login and cancel your queued or running jobs
- Send a Pixiv, X, YouTube, Bilibili, or NetEase link to extract:
  - `https://www.pixiv.net/artworks/...`
  - `https://x.com/{user}/status/{id}`
  - `https://youtube.com/watch?v=...` / `https://youtu.be/{id}` — video (`+q:1080|720|480`)
  - `https://music.youtube.com/watch?v=...` — audio with cover
  - live streams send the cover image; ended VODs send video
  - Bilibili `BV`/`AV` / bangumi / live cover / opus / audio / column (`+q:1080|720|480`)
  - NetEase song (audio + cover), MV, album/playlist cover + track list

Subscribable sources: Pixiv `users/{id}`, YouTube `@handle` / `channel/{id}` / `playlist?list=`, NetEase playlist / album.

In groups the bot only reacts to links it can handle. Each user may submit
`limits.per_user_per_minute` links per minute.

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

1. Create a crate `crates/sites/<name>` (copy a small one such as `pixiv`) implementing `Extractor`: `meta()` (id, name, aliases, examples), `matches()`, optional `rewrite_url()` for host aliases, `present()` for the caption layout, `options()`, optional `auth()`, optional `feed_matches()` / `feed()` for subscriptions. Use the injected `HttpClient` / `Ytdlp` and the helpers in `feedbot-site-common`; do not build your own HTTP client
2. Add it to `[workspace.dependencies]`, to `crates/extractors/Cargo.toml`, and to `ExtractorRegistry::with_sites`
3. If none of the `Present` layouts fits, implement `Caption` in the site and return `Present::Custom`

Do not change the command layer, help text, option parser, credential storage, or cache key logic.
