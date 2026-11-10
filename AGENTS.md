# AGENTS.md

Single-binary Rust service (edition 2024): a Rocket 0.5 HTTP API wrapping YouTube search via `innertube-rs`, with the `web/dist/` frontend embedded at build time. Hand-written Rust lives in two source files; `build.rs` generates the asset module. No library target, CI, README, or test suite. The API is a subset of the safe-yt-viewer API (`https://raw.githubusercontent.com/vrandomstuff/safe-yt-viewer/refs/heads/master/API.md`), which the top-of-file notes also point at.

## Commands

- `cargo check` — primary verification and the only real gate.
- `cargo test` — compiles the crate; there is no test module anywhere.
- `cargo run` — serves on Rocket's default `127.0.0.1:8000`. Ignition pre-warms the Innertube client, so it needs live network access; nothing is mocked or fixture-based, and manual `curl` checks hit real YouTube.
- `build.rs` scans `web/dist/` and writes `$OUT_DIR/webAssets.rs`. Rebuild the frontend (`pnpm build` in `web/`) and then run any cargo command to pick up changes; Cargo reruns the script automatically when `web/dist/` changes.
- Do **not** run `cargo fmt` / `rustfmt`: there is no rustfmt config, and its default would rewrite the required tab indentation to spaces.
- Ignore `*~`, `Cargo.toml~`, `.gitignore~`, `#*#`, and `#README.md#` — Emacs backups/locks, not source. `#README.md#` is an autosave of a README that is not in the tree.

## Layout

- `build.rs` — scans `web/dist/` at compile time and generates `$OUT_DIR/webAssets.rs` (one embedded `#[get]` handler per file plus `fileRoutes()`).
- `src/main.rs` — routes/handlers, cache construction and `.manage(...)`, the global `Innertube` cell, the warmup fairing, the `/api` index text, and the `webAssets` module that `include!`s the generated file.
- `src/wrappers.rs` — `Video`/`Channel` serde structs, `ytSearch`, `getChannelMetadata`, `searchVideoItemToVideo`. Put innertube calls and result flattening here; keep handlers in `main.rs`. It reaches the cache aliases and client through `crate::` (`mod wrappers;` is declared in `main.rs`).
- The struct definitions are duplicated in the `/api` index text; update both when a field changes.

## Conventions (non-default — follow these)

- The comment block at the top of `src/main.rs` is authoritative for style notes and the ordered TODO roadmap (m3u8 route, web frontend, SponsorBlock, DeArrow). Read it before picking work. Its "Output JSON for search endpoint" item is already done.
- **Tabs, not spaces**, and **camelCase** identifiers (`apiSearch`, `apiIndex`, `resultPage`, `searchCache`, `Video` fields like `channelAvatar`). `#![allow(nonstandard_style)]` exists specifically to permit this — don't "fix" names to snake_case.
- Rocket is 0.5, but the file intentionally uses `#[macro_use] extern crate rocket;` plus `#[launch]` and `rocket::build()`. Keep that style.
- `rocket` is built with only its default features (`json` is **not** enabled), so handlers serialize with `serde_json::to_string` and return `String`. Don't reach for `rocket::serde::json::Json<T>` unless you also add the `json` feature in `Cargo.toml`.
- The `/api` index text doubles as the API spec. Keep the unimplemented `/api/m3u8/...` routes listed in it, and add every new handler to both its `routes![...]` mount (`.mount("/")` or `.mount("/api")`) and that index text.
- Comment non-obvious library/serde/Rocket usage; the project explicitly asks for it.

## Architecture

- `build.rs` embeds every regular file under `web/dist/` with `include_bytes!`: `index.html` maps to `/`, every other file keeps its relative path, and `fileRoutes()` returns all generated routes as a `Vec<Route>` for `.mount("/", ...)`. Unknown extensions fall back to `application/octet-stream`. Generated asset routes are rejected at build time if they collide with `/health` or `/api/...`; a missing `web/dist` only emits a `cargo:warning` and serves no frontend assets.
- `innertube()` lazily creates one process-global `Innertube` in a `tokio::sync::OnceCell`; an `AdHoc::on_ignite` fairing pre-warms it at startup. Reuse this accessor; never construct `Innertube::new()` per request.
- `searchCache` is a `moka::future::Cache<(String, u64), Vec<Video>>`: 1-hour TTL, max 5000 entries, keyed by `(query, 1-based page)`. Walking to a deeper page also inserts the intermediate pages it passes.
- `channelCache` is a separate `Cache<String, Channel>`: 24-hour TTL, max 5000 entries. It is filled lazily by `getChannelMetadata` (which calls `get_channel_about`) — both by `/api/channelMetadata` and by the per-video avatar lookups during search, so results and metadata share entries.
- Each search hit triggers a `get_channel_about` for its channel (cached after the first), so a cold page is many sequential upstream calls and feels slow; that is the main cost in `ytSearch`.
- `/api/search` requires both `q` and `page` (`page` is a `u64` capped at 256; `>256` returns 400, missing/non-numeric/out-of-`u64`-range values fail to parse and get Rocket's 422). It follows `continuation_token` from page 1, discards channels/playlists, and returns a JSON array of `Video` (`id`, `thumbnail`, `channel`, `channelAvatar`, `channelName`, `title`). `channel` is the channel ID; `channelName` is the display name.
- `/api/channelMetadata?channelId=...` returns a `Channel` (`id`, `name`, `avatar`, `description`, `banner`); all but `id`/`name` are `Option`.
- `channelAvatar` is the channel's real avatar when the cached lookup succeeds, otherwise the `defaultAvatar` permalink; it is never empty. The thumbnail is chosen as the largest by area rather than trusting list order.
- `/health` returns 403 when the `User-Agent` contains `opencode` (via a custom `FromRequest` guard). That is deliberate, not a broken server; don't remove it or the guard to "fix" local testing.
- Error behavior is uniform across the JSON routes: both `apiSearch` and `apiGetChannelMetadata` return `Result<String, (Status, String)>`. Upstream/YouTube/innertube failures respond 503 `text/plain`, serialization failures 500. No handler or wrapper unwraps/expects on request or upstream data, so client input cannot panic the server.
- Expensive work logs stderr diagnostics as `[PROFILE]` / `[STARTUP]` lines; follow that pattern for new slow paths.
