/*
NOTES FOR AGENTS:
Use tabs not spaces
Use camelCase not snake_case
Do not remove routes from the api docs at /api unless you are removing a route (if a route is not implemented but in the docs keep it there for refrence when implementing)
Comment non obvious stuff for people who do not know the more advanced stuff about a certan library
 */

/*
TODO: (in order of most importent to least importent)
- Output JSON for search endpoint
- m3u8 route
- Web frontend
- SponsorBlock integration
- Optional DeArrow integration
 */
#![allow(nonstandard_style)]
#[macro_use] extern crate rocket;

use innertube_rs::{Innertube, SearchResultItem};
use tokio::sync::OnceCell;
use std::time::{Instant, Duration};
use moka::future::Cache;
use rocket::fairing::AdHoc;

// Global Innertube client container
static innertubeCache: OnceCell<Innertube> = OnceCell::const_new();

// In-memory cache for search results (expires after 10 minutes, max 500 entries)
type searchCache = Cache<(String, u8), Vec<SearchResultItem>>;

pub async fn innertube() -> Result<&'static Innertube, String> {
	innertubeCache
		.get_or_try_init(|| async {
			let start = Instant::now();
			let result = Innertube::new()
				.await
				.map_err(|e| e.to_string());

			eprintln!("[PROFILE] Innertube::new() initialized in: {:?}", start.elapsed());
			result
		})
		.await
}
// Web frontend (soon)
#[get("/")]
fn index() -> &'static str {
	"Hello, world!"
}

// API
#[get("/search?<q>&<page>")]
async fn apiSearch(
	q: String,
	page: u8,
	cache: &rocket::State<searchCache>
) -> Result<String, String> {
	let totalStart = Instant::now();

	let yt = innertube().await?;

	// Cache key includes both query and page
	let cacheKey = (q.clone(), page);

	// Check cache
	if let Some(cachedResults) = cache.get(&cacheKey).await {
		eprintln!(
			"[PROFILE] Cache HIT for query '{}' page {} in {:?}",
			q,
			page,
			totalStart.elapsed()
		);

		return Ok(format!(
			"¯\\_(ツ)_/¯ found {} cached items",
			cachedResults.len()
		));
	}

	let searchStart = Instant::now();

	let mut resultPage = yt
		.search(&q, None)
		.await
		.map_err(|e| e.to_string())?;

	// Navigate to requested page
	for walkingPage in 1..page {
		if let Some(token) = resultPage.continuation_token.as_deref() {
			resultPage = yt
				.search(&q, Some(token))
				.await
				.map_err(|e| e.to_string())?;
			cache.insert((q.clone(), walkingPage), resultPage.items.clone()).await;
		} else {
			break;
		}
	}

	// Cache only the requested page
	cache.insert(cacheKey, resultPage.items.clone()).await;

	eprintln!(
		"[PROFILE] yt.search() network time: {:?}",
		searchStart.elapsed()
	);

	eprintln!(
		"[PROFILE] Total handler time: {:?}",
		totalStart.elapsed()
	);

	Ok(format!(
		"¯\\_(ツ)_/¯ fetched {} items",
		resultPage.items.len()
	))
}

#[get("/")]
fn apiIndex() -> String {
	format!("
/api/m3u8/<video> - Get m3u8 master playlist for a video.
/api/m3u8/<video>/proxy/<token> - HLS segment CORS proxy.
/api/search?q=<search query>&page=(page) - Get videos for a search query.
	")
}

#[launch]
fn rocket() -> _ {
	let search_cache: searchCache = Cache::builder()
		.time_to_live(Duration::from_secs(600)) // 10 minutes cache TTL
		.max_capacity(500)
		.build();

	rocket::build()
		.manage(search_cache)
		.attach(AdHoc::on_ignite("Innertube Warmup", |rocket| Box::pin(async move {
			let start = Instant::now();
			let _ = innertube().await;
			eprintln!("[STARTUP] Innertube pre-warmed in {:?}", start.elapsed());
			rocket
		})))
		.mount("/", routes![index])
		.mount("/api", routes![apiIndex, apiSearch])
}
