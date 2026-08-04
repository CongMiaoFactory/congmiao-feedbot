# AI Project Context

This document gives AI assistants and maintainers a stable mental model of Congmiao FeedBot. It describes the repository as implemented; consult the linked source files before making behavior-sensitive changes.

## 1. Purpose and Scope

Congmiao FeedBot is a self-hosted Telegram bot. A user sends or embeds a supported URL, and the bot:

1. recognizes the platform;
2. parses upstream metadata into a shared model;
3. selects and prepares requested media;
4. sends text, photos, video, animation, audio, or files through Telegram;
5. stores reusable Telegram `file_id` values and reply mappings in SQLite.

Supported platforms are X, YouTube, Pixiv, Bilibili, and NetEase Cloud Music. Platform support is implemented in `src/provider/` and registered centrally in `src/provider/mod.rs`.

## 2. Technology

- Language: Rust 2024, minimum version defined in `Cargo.toml`.
- Async runtime: Tokio.
- Telegram: Teloxide, polling by default and webhook when configured.
- HTTP: Reqwest with rustls.
- Persistence: SQLite through SQLx with compile-time embedded migrations.
- Media: FFmpeg, ffprobe, yt-dlp, `image`, and `zip`.
- Tests: Rust integration tests and Wiremock.
- Runtime packaging: multi-stage Docker image based on Debian Bookworm.

Do not duplicate package versions in AI documentation. `Cargo.toml` is authoritative.

## 3. Runtime Composition

`src/main.rs` builds the application in this order:

1. initialize tracing from `RUST_LOG`;
2. load `Config` from environment variables;
3. create the temporary directory;
4. connect SQLite and run migrations;
5. load persistent runtime credentials;
6. create the in-memory cache;
7. construct the five-provider registry;
8. construct QR login services;
9. construct the media HTTP client and `MediaProcessor`;
10. create the queue semaphore;
11. start Telegram polling or webhook handling.

The shared `BotState` in `src/telegram.rs` owns these services. Keep dependency construction centralized in `main.rs` unless a testability requirement justifies another pattern.

## 4. Request and Media Flow

Typical request flow:

```text
Telegram update
  -> command/link extraction and option parsing (`src/telegram.rs`)
  -> local user limit and parse cache (`src/cache.rs`)
  -> ProviderRegistry::find / parse_one (`src/provider/mod.rs`)
  -> platform Provider::parse (`src/provider/*.rs`)
  -> ParsedContent + MediaItem (`src/model.rs`)
  -> selection, caption, spoiler, and media-group planning (`src/telegram.rs`)
  -> MediaProcessor when direct upload is not possible (`src/media.rs`)
  -> Telegram send
  -> persist Telegram file_id and message/media mapping (`src/storage.rs`)
```

A provider must return normalized data and must not emit Telegram HTML. Telegram-specific escaping, caption folding, caption length limits, grouping, and upload behavior belong in `src/telegram.rs`.

### Shared model

`ParsedContent` contains platform, content kind, canonical URL, author, title, plain text, sensitivity, statistics, media, and optional collection entries.

`MediaItem` contains source and fallback URLs, optional thumbnail, file metadata, request headers, stable cache key, download requirement, and optional secondary stream. `secondary_url` is used for cases such as separate DASH video/audio tracks.

Stable `cache_key` values matter because SQLite maps them to reusable Telegram files and `/file` reply targets. Do not casually rename cache-key formats.

### Media processing

`src/media.rs` handles:

- direct versus local media preparation;
- bounded download retries and HTTP Range resume;
- fallback source URLs and request headers;
- DASH stream download and FFmpeg merge;
- image dimension validation and oversized-photo previews;
- Pixiv ugoira conversion;
- thumbnail preparation;
- temporary-file cleanup and Telegram size constraints.

Reuse this pipeline instead of implementing downloads inside a provider or Telegram command.

## 5. Provider Contract

Every provider implements:

```rust
pub trait Provider: Send + Sync {
    fn platform(&self) -> Platform;
    fn can_handle(&self, url: &str) -> bool;
    async fn parse(&self, request: &ParseRequest) -> ProviderResult<ParsedContent>;
}
```

Provider rules:

- `can_handle` must be strict enough not to claim unrelated domains or identifiers.
- Canonicalize short links only with bounded redirects and validate the final domain.
- Parse unstable upstream JSON defensively and tolerate optional fields.
- Return unknown content in a useful degraded form when possible.
- Map failures to `Unsupported`, `InvalidUrl`, `Unavailable`, `Authentication`, `RateLimited`, `Upstream`, `InvalidResponse`, or `Media`.
- Keep retries bounded and avoid synchronized retry storms.
- Put required HTTP headers on `MediaItem` so the shared downloader can reproduce authorized/hotlink-protected requests.

### Bilibili specifics

`src/provider/bilibili.rs` supports video, bangumi, dynamic/Opus, live, audio, and article targets. Dynamic cards can contain text, photos, Opus, forwards, videos, seasons, articles, audio, and live cards.

Bilibili API requests intentionally use browser-like headers. Device cookies (`buvid3` and `buvid4`) are fetched and merged with existing login cookies. API `-352` triggers at most one forced device-cookie refresh and retry. Preserve account cookies such as `SESSDATA` and `bili_jct`; never replace the entire cookie set with SPI cookies.

### NetEase specifics

NetEase parsing depends on an external API sidecar configured by `NETEASE_API_BASE`. Docker Compose provides this as `netease-api`. Login credentials can be persisted at runtime.

## 6. Telegram Behavior

`src/telegram.rs` is intentionally broad because it coordinates updates, commands, inline mode, sending, progress, retries, cache reuse, and reply mappings.

Important invariants:

- HTML parse mode requires all upstream text and attributes to be escaped.
- Media captions are bounded to 1024 characters; text messages to 4096.
- Long summaries use Telegram expandable blockquotes and retain title/author/stat metadata.
- A media group must contain 2 to 10 items; splitting must never leave a one-item group.
- For multiple media-group batches, caption appears only once on the final batch.
- Spoiler behavior combines upstream sensitivity, `MEDIA_SPOILER_MODE`, and the per-request `+sp` option.
- Default video quality is defined by `DEFAULT_VIDEO_QUALITY` in `src/model.rs`.
- Replying with `/file` relies on the persisted `(chat_id, message_id)` media mapping.

User-visible commands and flags are documented in `README.md`; inspect command parsing in `src/telegram.rs` before adding or renaming one.

## 7. Persistence and Credentials

SQLite is the only persistent application database. `Storage::connect` creates the parent directory, opens the database, and runs embedded migrations automatically.

Current persisted concerns:

- Telegram media cache: stable media key to Telegram `file_id`.
- Provider credentials: runtime login values.
- Telegram message/media mapping: enables reply-based original-file retrieval.

Migration policy:

- Add a new numbered file under `migrations/` for schema changes.
- Never edit or reorder an already-applied migration.
- Keep migrations compatible with existing persistent volumes.

`RuntimeCredentials` merges configured initial credentials with values stored by QR login. Secrets must not be printed in logs, test snapshots, README examples, or errors.

## 8. Configuration

`src/config.rs` is the parser and `.env.example` is the deployer-facing inventory. `README.md` explains each setting.

Only `TELEGRAM_BOT_TOKEN` (or legacy `TOKEN`) is mandatory. Major groups are:

- Telegram polling/webhook/local API;
- SQLite and temporary directories;
- upstream API base URLs;
- YouTube, Pixiv, Bilibili, and NetEase credentials;
- FFmpeg, ffprobe, and yt-dlp paths;
- queue, timeout, retry, upload, and local rate-limit controls;
- spoiler policy and administrator authorization.

When adding an environment variable, update all of:

1. `Config` and `Config::from_env`;
2. test `Config` constructors;
3. `.env.example`;
4. the configuration table or relevant section in `README.md`;
5. Docker/Compose only if container wiring is required.

## 9. Local Development

Prerequisites:

- Rust version compatible with `Cargo.toml`;
- FFmpeg and ffprobe;
- yt-dlp for YouTube media paths;
- a Telegram bot token for a live run;
- NetEase sidecar if testing NetEase integration manually.

Typical setup:

```bash
cp .env.example .env
# Fill TELEGRAM_BOT_TOKEN, then export variables from .env using your shell/tool.
cargo run --release
```

The program reads process environment variables directly; it does not parse `.env` itself.

Required validation before release or handoff:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --locked
git diff --check
```

Use a focused test command while iterating, for example:

```bash
cargo test --test core bilibili_dynamic
cargo test --test media interrupted_download
```

Wiremock tests should validate request path, relevant query parameters, headers, fallback behavior, and normalized output without contacting production services.

## 10. Deployment

### Docker Compose (recommended)

```bash
cp .env.example .env
# Set TELEGRAM_BOT_TOKEN and optional credentials.
docker compose pull
docker compose up -d
docker compose logs -f feedbot
```

Services:

- `feedbot`: main application image; mounts `feedbot-data` at `/app/data` and `./secrets` read-only at `/app/secrets`.
- `netease-api`: required NetEase API sidecar, reachable internally at `http://netease-api:3000`.
- `telegram-bot-api`: optional local Telegram API under Compose profile `telegram-local`.

The default SQLite path is `/app/data/feedbot.db` inside the container. Back up the `feedbot-data` volume before risky upgrades. The secrets directory may contain a YouTube cookies file but must not be committed.

Enable the local Telegram API when larger upload limits or local-mode behavior is needed:

```bash
docker compose --profile telegram-local up -d
```

Also configure `TELEGRAM_API_ID`, `TELEGRAM_API_HASH`, `TELEGRAM_API_URL`, and `LOCAL_MODE` as described in `README.md`.

### Docker image

The `Dockerfile` builds the Rust binary with `cargo build --release --locked`. The runtime image installs CA certificates, FFmpeg, Python, and yt-dlp. It creates `/app/data` and `/app/tmp`, exposes persistence through `/app/data`, and starts `congmiao-feedbot` directly.

Manual image workflow:

```bash
docker build -t congmiao-feedbot:local .
docker run --rm --env-file .env -v feedbot-data:/app/data congmiao-feedbot:local
```

NetEase requests need a reachable sidecar; Compose is preferred over a standalone container for that reason.

### Release binary

`.github/workflows/release.yml` runs for tags matching `v*`. It builds `x86_64-unknown-linux-musl`, packages the binary with `README.md`, `LICENSE`, and `.env.example`, generates SHA-256, and creates a GitHub Release.

A binary deployment must separately install FFmpeg, ffprobe, and yt-dlp, create writable data/temp directories, supply environment variables, and use a supervisor such as systemd. Exact commands are in `README.md`.

### Webhook

Polling is the default. Setting `WEBHOOK_URL` switches to webhook operation. The configured public URL must be HTTPS and route to `WEBHOOK_HOST:WEBHOOK_PORT`; reverse-proxy and firewall setup are operator responsibilities.

## 11. Release Procedure

Only release when explicitly requested:

1. ensure the working tree contains only intended changes;
2. update the package version in `Cargo.toml` and regenerate/update `Cargo.lock`;
3. update user-facing version examples if any;
4. run all validation commands;
5. commit the release change;
6. create an annotated `vX.Y.Z` tag;
7. push the branch and tag;
8. verify GitHub Actions and release artifacts.

Never create commits, tags, or pushes merely because implementation is complete.

## 12. Change Checklists

### Add or extend a provider

- Tighten URL recognition and canonicalization.
- Return normalized model values with stable cache keys.
- Preserve required media headers and fallback URLs.
- Handle authentication, rate limiting, deletion, and unknown JSON variants.
- Reuse shared media and credential systems.
- Add Wiremock integration tests.
- Update supported-feature documentation.

### Change Telegram output

- Escape all user/upstream values.
- Check both 1024 and 4096 limits.
- Test short, boundary, long, entity-heavy, and multibyte text.
- Verify single media, media groups, groups over ten, no-media content, inline mode, and reply mappings as applicable.

### Change media handling

- Keep retries and file sizes bounded.
- Preserve cleanup on success and error paths.
- Check direct URL, downloaded file, fallback URL, cached `file_id`, and local Bot API behavior.
- Add media integration tests using local fixtures/Wiremock, not production URLs.

### Change deployment/configuration

- Update `src/config.rs`, `.env.example`, `README.md`, and this document.
- Preserve the `/app/data` volume contract.
- Never bake credentials into an image or Compose file.
- Verify both fresh install and upgrade paths.

## 13. Sources of Truth

When documentation disagrees, use this priority:

1. executable behavior and tests;
2. `Cargo.toml`, `src/config.rs`, migrations, Docker files, and workflow files;
3. `README.md` for supported user procedures;
4. this AI context and `llms.txt` as navigation aids.

If behavior and README disagree, fix the discrepancy rather than teaching an AI to rely on stale information.
