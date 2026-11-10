# AI Project Context

A stable mental model of CongMiaoFeedBot as implemented. Read the referenced source before making behavior-sensitive changes; `Cargo.toml` is authoritative for versions.

## 1. Runtime composition (`src/main.rs`)

1. Load `AppConfig` from `config/default.toml` plus `FEEDBOT__*` env overrides. The toml file is the only place defaults live; every key except the bot token must be listed there, and a test checks this. `AppConfig::validate` rejects values that would stall jobs or hammer upstream APIs (zero permits, attempts, timeouts, intervals; `album_max` outside 2–10; credential checks or subscription intervals shorter than `feed.tick_secs`). Init tracing.
2. Resolve the master key: config/env, otherwise load or create `master.key` next to the DB.
3. `SqliteStore::connect` runs the embedded migrations. It is used as `Arc<dyn Store>`.
4. Build one `ReqwestClient` (`Arc<dyn HttpClient>`) and one `YtdlpCli` (`Arc<dyn Ytdlp>`). Both go into `ExtractorSites` (registry) and `ExtractCtx` (extract calls).
5. `Downloader::build` gets its own client: connect and read timeouts, no total deadline, plus a byte cap. `FfmpegCli` gets a wall-clock timeout.
6. `JobQueue::channel` returns a persistent queue plus its receiver. The worker pool starts with `limits.max_jobs` slots. It takes a slot before it receives a job, so the bounded channel fills up when all slots are busy and `submit` reports a full queue. `recover_unfinished` runs in its own task and re-queues jobs from a previous run.
7. `App` bundles the shared services (`bot`, `registry`, `store`, `jobs`) with two groups: `access` (admin ids, per-user limiter) and `pipeline` (extract context and permits, retry limit, album size, overflow sink, and `MediaTools` for download/transcode, cloned into each item task).
8. `FeedScheduler` gets the store, the queue (as `WorkEnqueue`), the registry (as `ExtractorLookup`, exact id lookup), a `TelegramPublisher`, and `FeedSettings`. The feed crate depends only on core.
9. The Teloxide dispatcher runs over polling or webhook (`telegram.mode` enum). A single signal task (Ctrl-C or SIGTERM) cancels the background loops and shuts the dispatcher down.

## 2. Link flow

```text
message ─ handlers::handle_url_message
  ├ extract URLs (entities first; Telegram URL entities without a scheme get https://)
  ├ raw_args = text minus the links as written
  ├ de-duplicate by registry.canonicalize (site rewrite_url: youtu.be, x.com, b23.tv/BV…, www.)
  ├ keep links that registry.match_url accepts (groups: unsupported links are ignored silently)
  ├ per-user governor limit, checked per link
  └ JobQueue::submit → jobs row (state=queued, raw_args) → mpsc → worker
worker::process_job (span via .instrument)
  ├ advance(state) at each step. set_state returns false once the job is terminal
  │   (`/cancel` sets `cancelled`), and the run stops silently
  ├ options = merge_layers(spec, chat+user layers, inline tokens) → extractor.validate
  ├ extractor.extract  (always runs: access check + fresh caption)
  ├ overflow.publish_page (telegraph mode, > summary_threshold, all photos hotlinkable)
  │   → caption + page link, no download, nothing cached
  ├ cache hit (every item cached under source_key) → resend by file_id
  └ else prepare_all: per item download / remux / ugoira / yt-dlp / transcode.
      A failing item is skipped and reported; the job fails only if no item worked.
      deliver() plans albums/singles and sends through one InputFile path.
      Only a complete result set is written to the media cache.
```

Retries: `ExtractError::retryable()` or `MediaError::Download`, up to `limits.max_attempts`, with exponential sleep.

Captions: the extractor's `present()` picks a layout from `core::present` (`Gallery` for Pixiv, `Post { origin: "原帖" }` for X, `Clip { origin: "原片" }` for YouTube/Bilibili/NetEase, `Default` otherwise; `Custom` for a site-specific `Caption`). The layout yields `Block`s (lines of spans, expandable quotes) and `MediaAction`s; `crates/bot/src/caption` renders them as HTML within 1024 UTF-16 units, sends leftover text as follow-up messages (`ExpandableClip` adds `简介过长` instead), and turns actions into buttons on single-media posts.

Overflow (`crates/bot/src/telegraph.rs`): `OverflowSink::publish_page` runs before the cache check; published pages are reused per `source_key` while the item count is unchanged (`page_cache`). `TelegraphOverflow` hotlinks the original image URLs (Telegraph uploads are disabled). A host listed in `telegraph.image_proxies` is rewritten to its proxy. Otherwise an item whose media headers carry `Referer`/`Cookie` blocks the page, and the job uses albums. Telegraph calls go through the injected `HttpClient` (`HttpRequest::post`). Without `access_token`, one account is created per process (`OnceCell`); the token is never logged.

Restart: jobs that were `uploading` are failed (they may have been partly delivered) instead of re-run.

## 3. Options (`crates/core/src/options.rs`)

- Tokens: `+flag`, `-flag`, `+name:value`, `-name` (clear), `+list:a,b`, `-list:a`.
- Effective value: spec defaults < chat layer < user layer < inline tokens (`load_saved_options` + `merge_layers`).
- `/set` writes one layer with `apply_to_layer`: only the changed keys, no baked-in defaults. In private chats it writes the user layer. In groups it writes the chat layer, which needs chat admin rights or `telegram.admin_ids`.
- Cache keys use the fingerprint of options with `affects_output`.

## 4. Subscriptions (`crates/feed`)

- `/sub <link> [+opts]` accepts links where `registry.match_feed` succeeds (`Extractor::feed_matches`). It stores the canonical URL and `raw_args`, and is due immediately.
- Every tick runs `store.due(now)`, then `Extractor::feed` (newest first, bounded by `poll_timeout`), then `SeenRepo::insert_if_new` for each entry.
  - First poll (`cursor` is NULL): only fills the seen set, then sets `cursor = "ready"`.
  - Later polls: at most `push_limit` new entries are enqueued oldest-first with the subscription's `raw_args`. Older unseen entries are marked seen and skipped.
- Failures back off exponentially. After `max_fail_count` failures in a row the subscription is disabled and the chat is notified.
- Credential checks use `due_for_check(now - credential_check_secs)` and skip rows whose `CredentialStatus::needs_login()` (expired / invalid). A check that is inconclusive (`Unknown` or an error) keeps the status but bumps `last_checked_at`. When a credential starts needing login and cannot be refreshed, one notification goes to the owner (global credentials: `admin_ids`).
- If an entry cannot be enqueued (queue full), it and the remaining unpushed entries are removed from the seen set again (`forget_seen`). The poll then counts as failed, and those entries are retried on the next poll.
- Finished jobs older than `job_retention_days` are pruned hourly.
- Bilibili (`crates/sites/bilibili/src/feed.rs`): `space.bilibili.com/{mid}` polls the space dynamic feed; `/{mid}/video` keeps only `DYNAMIC_TYPE_AV`. The entry key is always the dynamic id. Video dynamics are pushed as `www.bilibili.com/video/{bvid}`, others as `t.bilibili.com/{id}`. Entries are ordered by `pub_ts`, so a pinned dynamic does not count as the newest.
- X (`crates/sites/x/src/timeline.rs`): `x.com/{user}[/media]` needs a login (`Extractor::feed_requires_login`; `/sub` refuses without a credential for the subscriber). The screen name is resolved to a user id once (in-memory cache), then one `UserMedia` page is read. Every `tweet_results.result` in the response is collected regardless of layout; only the user's own non-retweet posts with media are kept, ordered by tweet id (snowflake ids are time-ordered).

## 5. Credentials and login

- Lookup order: user → global (`CredentialRepo::resolve`). `Scope::Chat` credentials are legacy: they are never created or resolved. `/login` always writes the user scope; `/login <p> global` (operators only) writes the global scope. `/logout` follows the same rule.
- Payloads are encrypted with ChaCha20-Poly1305; the AAD binds extractor, scope and scope id, so a row cannot be decrypted under another owner.
- The login dialogue lives in `InMemStorage<LoginState>`. Every command, and every new `/login`, aborts the current session's `CancellationToken`. A QR poller re-checks the token after each poll before it touches the dialogue or saves anything. Poll errors are reported once; the login stops after `QR_POLL_MAX_ERRORS` errors in a row.

## 6. Storage notes

- Timestamps are RFC 3339 text and are compared with `julianday()`.
- `set_platform` replaces a layer inside a transaction.
- Migrations: `0001` schema, `0002` multi-item media cache, `0003` `raw_args` on jobs and subscriptions plus a credential check index, `0004` converts the old cancelled-as-failed rows to the `cancelled` state, `0005` drops the legacy `options_json` columns and the `chat_settings` table, `0006` adds the login-paused state of subscriptions, `0007` caches published Telegraph pages.

## 7. Known gaps

- Tests live next to the code. Whole `extract`/`feed` runs use `feedbot_site_common::testing` (`FakeHttp` answers canned JSON by `host/path`, `FakeYtdlp`; enabled by the `testing` feature in each site's dev-dependencies); nothing talks to the real sites, so upstream changes show up only in production.
- CI (`.github/workflows/ci.yml`) runs fmt, clippy and tests, builds the image for amd64 only, and runs `scripts/image-smoke.sh`, which repeats the ffmpeg calls of `crates/media/src/ffmpeg.rs` and yt-dlp's merge/extract steps inside the image. Keep that script in step with `ffmpeg.rs`. The `Dockerfile` pins yt-dlp and QuickJS-ng (selected through `ytdlp.js_runtime`) with SHA-256 sums (update both sums with the version). It builds ffmpeg from source with `--disable-everything` plus an explicit component list (x264, dav1d, aac, mjpeg, mp4/matroska/concat, …). A new codec, container, or filter in `crates/media/src/ffmpeg.rs` or the yt-dlp arguments must be added to that `configure` list, or it fails only inside the image.
- X calls rely on GraphQL query ids copied from gallery-dl, set in `[x]` of the config; a 404 means they rotated. The Bilibili space feed reads only the first page of `/x/polymer/web-dynamic/v1/feed/space`, WBI-signed (`bilibili/wbi.rs`, keys from `/x/web-interface/nav`, cached for an hour, refreshed on -352). It sends no `dm_*` browser-fingerprint parameters; if -352 persists in production, those are the next thing to add.
- The Telegraph hotlink rule (a `Referer`/`Cookie` media header means it needs a proxy) has only been exercised with fakes.
