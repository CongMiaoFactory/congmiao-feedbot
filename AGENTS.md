# AGENTS.md

## Project

Congmiao FeedBot is a Rust 2024 Telegram bot that parses links from X, YouTube, Pixiv, Bilibili, and NetEase Cloud Music, then sends normalized text and media to Telegram.

Before changing code, read `docs/AI_CONTEXT.md`. Use `README.md` as the source of truth for user-facing usage and deployment, `.env.example` for environment variables, and `Cargo.toml` for the current version and toolchain requirements.

## Repository Map

- `src/main.rs`: process bootstrap and dependency wiring.
- `src/telegram.rs`: Telegram commands, routing, captions, uploads, media groups, and message-to-media mapping.
- `src/provider/`: platform detection and parsing. All providers implement `Provider` from `src/provider/mod.rs`.
- `src/model.rs`: shared normalized content and media types.
- `src/media.rs`: downloads, retries, image previews, DASH merging, FFmpeg/ffprobe, yt-dlp, and upload preparation.
- `src/login.rs`: QR-code login flows.
- `src/credentials.rs`: runtime credential merge and persistence.
- `src/storage.rs`: SQLite access and automatic migrations.
- `src/cache.rs`: in-memory parsing/rate-limit cache.
- `src/config.rs`: environment configuration.
- `migrations/`: SQLx SQLite migrations; never rewrite an applied migration.
- `tests/core.rs`: provider, routing, caption, login, cache, and storage integration tests.
- `tests/media.rs`: media processing tests.
- `docker-compose.yml`, `Dockerfile`: container deployment.
- `.github/workflows/release.yml`: `v*` tag release workflow.

## Architecture Rules

1. Normalize provider output into `ParsedContent` and `MediaItem`; keep Telegram formatting out of providers.
2. Reuse `MediaProcessor`, Telegram upload logic, SQLite `file_id` cache, and stable media cache keys.
3. Treat upstream JSON as unstable: parse defensively, support fallbacks, and map failures to `ProviderError`.
4. Keep retries bounded. Do not introduce unbounded API, login, or media retries.
5. Merge runtime cookies through `RuntimeCredentials`; do not overwrite existing login fields such as Bilibili `SESSDATA` or `bili_jct`.
6. Escape Telegram HTML and respect 1024-character captions, 4096-character messages, and media-group constraints.
7. Preserve secrets: never log tokens/cookies, commit `.env`, databases, QR credentials, or files under `secrets/`.
8. Make focused changes; do not refactor unrelated modules or add dependencies without need.

## Development Workflow

Use Rust 1.90 or newer. Prefer focused tests first, then run the full checks before handoff:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --locked
git diff --check
```

When adding a provider feature, add Wiremock coverage in `tests/core.rs`. When changing download, conversion, preview, or merging behavior, add coverage in `tests/media.rs`. Do not add a formatter or alternate test framework.

## Deployment

Recommended deployment is Docker Compose:

```bash
cp .env.example .env
# Set TELEGRAM_BOT_TOKEN and optional credentials in .env
docker compose up -d
```

Persistent state lives in the `feedbot-data` volume at `/app/data`; optional secrets are mounted read-only from `./secrets` to `/app/secrets`. NetEase requires the `netease-api` sidecar. Local Telegram Bot API is optional through the `telegram-local` Compose profile.

For exact Docker, binary, source, webhook, local Bot API, backup, and upgrade procedures, read `README.md` and `docs/AI_CONTEXT.md`.

## Release

Do not change versions, create commits, push, or tag unless explicitly requested. Releases require matching versions in `Cargo.toml` and `Cargo.lock`; pushing an annotated `vX.Y.Z` tag triggers `.github/workflows/release.yml` and publishes a Linux x86_64 musl archive plus SHA-256 checksum.
