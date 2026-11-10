# AGENTS.md

## Project

CongMiaoFeedBot is a Rust 2024 Telegram bot that extracts media from Pixiv, X, YouTube, Bilibili, and NetEase Cloud Music links, sends it to Telegram, and pushes new items of subscribed sources. Users log in per account; credentials are stored encrypted in SQLite.

Before changing code, read `docs/AI_CONTEXT.md`. `README.md` is the source of truth for user-facing usage, `config/default.toml` for settings, and `Cargo.toml` for versions and the toolchain.

## Repository Map

Cargo workspace; dependencies point downwards only (`core` depends on nothing in the workspace).

- `src/main.rs`, `src/config.rs`: binary. Loads config and wires every service together. All concrete types are chosen here. New settings go into `config/default.toml`, not serde defaults.
- `crates/core`: domain types and traits only. Covers `Extractor` (`SiteMeta`, `rewrite_url`, `present`, and the `feed` contract), `ExtractorLookup`, `AuthProvider`, `HttpClient`, `Ytdlp`, `MediaProcessor`, the repository traits plus `Store`, `Job`, `Subscription`, the `+`/`-` option system (`options.rs`), and the platform-agnostic caption layouts (`present.rs`: `Present`, `Block`/`Span`, `MediaAction`).
- `crates/sites/<site>` (`feedbot-bilibili`, `-netease`, `-pixiv`, `-x`, `-youtube`): one crate per site, depending only on core and `feedbot-site-common`. The larger `bilibili` and `x` are split into modules by API surface. A site declares its `SiteMeta`, its host aliases (`rewrite_url`), and its caption layout (`present`).
- `crates/sites/common` (`feedbot-site-common`): helpers shared by sites (JSON/credential access, `rewrite_host`, `expand_shortlink`, QR rendering, `NoYtdlp`); with the `testing` feature, `FakeHttp` / `FakeYtdlp` for whole `extract` / `feed` runs in site tests.
- `crates/extractors`: `ExtractorRegistry` over the site crates, URL canonicalization (`registry.canonicalize`: lowercase host, the owning site's `rewrite_url`, tracking strip), and `ReqwestClient` (the only HTTP client). Look up user-typed platform names with `resolve_name` (alias and fuzzy); look up ids read from storage with `by_id` (exact).
- `crates/media`: `Downloader` (streamed, size-capped), `FfmpegCli` (timeouts, `kill_on_drop`), `YtdlpCli`.
- `crates/store`: `SqliteStore` implements every repository trait. Credentials are encrypted with AAD bound to extractor and scope.
- `crates/bot`: Telegram layer.
  - `handlers.rs`: routing and link messages.
  - `app.rs`: `App`, the shared state. Services sit at the top; `access` (admins, rate limit) and `pipeline` (worker settings and `MediaTools`) group the rest.
  - `commands/` (one file per command group), `login.rs`: commands and the login dialogue.
  - `queue.rs`: the persistent job queue.
  - `worker/`: `mod.rs` runs a job (extract, retries, `file_id` cache), `prepare.rs` downloads and transcodes (no Telegram), `deliver.rs` uploads, `presence.rs` shows chat actions and progress.
  - `caption/`: renders `core::present` blocks as Telegram HTML within the UTF-16 limits, and actions as buttons. `caption/tests/` pins each site's rendered caption.
  - `album.rs`: media-group planning.
  - `publisher.rs`, `telegraph.rs`: `OverflowSink` (chunked albums, or one Telegraph page for large photo sets).
- `crates/feed`: `FeedScheduler`. Polls subscriptions, re-checks credentials, and prunes old jobs. Depends only on core: finds extractors through `ExtractorLookup` and talks to the bot only through `WorkEnqueue` and `Publisher`.
- `migrations/`: SQLx migrations, embedded at build time. Never edit a migration that has been applied anywhere; add a new file.

## Architecture Rules

1. Extractors return `Extracted` / `MediaItem` / `FeedEntry`. Keep Telegram formatting out of extractors: a site picks a `Present` layout (or implements `Caption` for a new one); HTML rendering lives in `crates/bot/src/caption/`. Keep site-specific rules (hosts, short links, layouts) in the site crate, not in shared code.
2. Extractors and auth providers use the injected `ExtractCtx` / `ExtractorSites` (`HttpClient`, `Ytdlp`). Do not create reqwest clients, read environment variables, or spawn tools inside a site.
3. Services hold trait objects (`Arc<dyn Store>`, `Arc<dyn MediaProcessor>`, …), not concrete backends.
4. Treat upstream JSON as unstable: parse defensively and map failures to `ExtractError` / `AuthError`; `ExtractError::retryable()` decides whether the worker retries.
5. Keep retries, polls, and external processes bounded (attempt limits, timeouts, size caps).
6. Option layers: spec defaults < chat layer < user layer < inline tokens. Store only what a user set in a layer (`apply_to_layer`), never merged defaults.
7. Jobs and subscriptions persist their raw option text (`raw_args`) and are re-parsed on every run.
8. Escape Telegram HTML (the bot uses HTML parse mode). Respect the 1024 UTF-16 caption limit, the 4096 message limit, and media-group rules.
9. Never log or commit tokens, cookies, `master.key`, databases, or `.env`.
10. Make focused changes; do not add dependencies without need.
11. The Docker image ships a minimal ffmpeg: when ffmpeg or yt-dlp starts using a new codec, container, or filter, add it to the `configure` list in the `Dockerfile`.

## Development Workflow

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git diff --check
```

## Release

Do not change versions, commit, push, or tag unless explicitly asked.
