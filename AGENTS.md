# AGENTS.md

Single-binary Rust service (edition 2024): a Rocket 0.5 HTTP API wrapping YouTube search via `innertube-rs`. Everything lives in `src/main.rs` — no library target, frontend, CI, README, or test suite. The API is meant to be a subset of the safe-yt-viewer API (`https://raw.githubusercontent.com/vrandomstuff/safe-yt-viewer/refs/heads/master/API.md`), referenced in the top-of-file notes.

## Commands

- `cargo check` — primary verification and the only real gate; there are no tests.
- `cargo test` — compiles the crate; `src/main.rs` has no test module.
- `cargo run` — serves on Rocket's default `127.0.0.1:8000`. Startup pre-warms the YouTube client, so it needs live network access; nothing is mocked or fixture-based.
- Do **not** run `cargo fmt` / `rustfmt`: there is no rustfmt config, and its default would rewrite the required tab indentation to spaces.
- Ignore `*~`, `Cargo.toml~`, `.gitignore~`, and `#*#` files — Emacs backups/locks, not source.

## Conventions (non-default — follow these)

- The comment block at the top of `src/main.rs` is authoritative for style notes and the ordered TODO roadmap. Read it before picking work. Its "Output JSON for search endpoint" item is already done, so don't redo it.
- **Tabs, not spaces**, and **camelCase** identifiers (`apiSearch`, `apiIndex`, `resultPage`, `searchCache`, `Video` fields like `channelAvatar`). `#![allow(nonstandard_style)]` exists specifically to permit this — don't "fix" names to snake_case.
- Keep unimplemented routes (currently both `/api/m3u8/...` routes) listed in the `/api` index text; it doubles as the API spec. Every new handler must be added to both its `routes![...]` mount and that index text.
- Rocket is 0.5, but the file intentionally uses `#[macro_use] extern crate rocket;` plus `#[launch]` and `rocket::build()`. Keep that style.
- Comment non-obvious library/serde/Rocket usage; the project explicitly asks for it.

## Architecture

- `innertube()` lazily creates one process-global `Innertube` in a `tokio::sync::OnceCell`; an `AdHoc::on_ignite` fairing pre-warms it at startup. Reuse this accessor; never construct `Innertube::new()` per request.
- `searchCache` is a `moka::future::Cache<(String, u64), Vec<Video>>`: 1-hour TTL, max 5000 entries, keyed by `(query, 1-based page)`. Walking to a deeper page also inserts the intermediate pages it passes.
- `channelCache` is a separate `Cache<String, Channel>` with max 5000 entries and **no TTL**; `getChannelMetadata` calls `get_channel_about` and caches the flattened `Channel`.
- `/api/search` requires both `q` and `page` (`page` is a `u64` capped at 256; `>256` returns 400, non-numeric or out-of-`u64`-range values fail to parse as 422). It follows `continuation_token` from page 1, discards channels/playlists, and returns a JSON array of `Video` (`id`, `thumbnail`, `channel`, `channelAvatar`, `channelName`, `title`). `channel` is the channel ID; `channelName` is the display name.
- `/api/channelMetadata?channelId=...` returns a `Channel` (`id`, `name`, `avatar`, `description`, `banner`).
- `channelAvatar` in search results is the channel's real avatar when the cached lookup succeeds, and otherwise the default-user permalink (`defaultAvatar`); it is never empty. The thumbnail is chosen as the largest by area rather than trusting list order.
- `/health` returns 403 when the `User-Agent` contains `opencode`. That is deliberate, not a broken server.
- Error behavior is uniform across the JSON routes: both `apiSearch` and `apiGetChannelMetadata` return `Result<String, (Status, String)>`. Upstream/YouTube/innertube failures respond 503 `text/plain`, serialization failures 500, and `/api/search` answers 400 for `page > 256` (Rocket itself answers 422 for unparseable/missing params). No handler unwraps or expects on request or upstream data, so client input cannot panic the server.
- Expensive work logs stderr diagnostics as `[PROFILE]` / `[STARTUP]` lines; follow that pattern for new slow paths.
