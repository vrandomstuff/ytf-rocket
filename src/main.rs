/*
NOTES FOR AGENTS:
Use tabs not spaces
Use camelCase not snake_case
Do not remove routes from the api docs at /api unless you are removing a route (if a route is not implemented but in the docs keep it there for refrence when implementing)
Comment non obvious stuff for people who do not know the more advanced stuff about a certan library
The API is a subset of https://raw.githubusercontent.com/vrandomstuff/safe-yt-viewer/refs/heads/master/API.md since that is what i am familler with since i made it
 */

/*
TODO: (in order of most importent to least importent)
- m3u8 route
- Web frontend
- SponsorBlock integration
- Optional DeArrow integration
 */
#![allow(nonstandard_style)]
#![allow(unused_parens)]
#[macro_use] extern crate rocket;

use serde::{Serialize, Deserialize};
use innertube_rs::{Innertube, SearchResultItem, SearchVideoItem};
use tokio::sync::OnceCell;
use std::time::{Instant, Duration};
use moka::future::Cache;
use rocket::fairing::AdHoc;
use rocket::http::Status;

// Global Innertube client container
static innertubeCache: OnceCell<Innertube> = OnceCell::const_new();
// In-memory cache for search results (expires after 1 hour, max 5000 entries).
// Stores the flattened `Video` shape so a cache hit can be serialized directly.
type SearchCache = Cache<(String, u64), Vec<Video>>;

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

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Video {
	id: String,
	thumbnail: String,
	channel: String,
	channelAvatar: String,
	channelName: String,
	title: String
}
#[derive(Serialize, Deserialize, Debug, Clone)]
struct Channel {
	id: String,
	name: String,
	avatar: Option<String>,
	description: Option<String>,
	banner: Option<String>
}
// TTL: 1 day Max: 5000
type ChannelCache = Cache<String, Channel>;

// innertube resolves unknown channels to this placeholder avatar, so reuse it
// instead of emitting an empty string (it is a stable permalink).
const defaultAvatar: &str = "https://yt3.googleusercontent.com/a/default-user=s100-c-k-c0x00ffffff-no-rj";

async fn getChannelMetadata(channelId: String, cache: ChannelCache) -> Result<Channel, String> {
	if let Some(cachedResults) = cache.get(&channelId).await {
		return Ok(cachedResults)
	}
	let yt = innertube().await?;

	let about = yt
		.get_channel_about(&channelId)
		.await
		.map_err(|e| e.to_string())?;

	let channel = Channel {
		id: about.channel_id,
		name: about.title,
		avatar: about.avatar,
		description: about.description,
		banner: about.banner
	};
	cache.insert(channelId, channel.clone()).await;

	return Ok(channel)
}

#[get("/channelMetadata?<channelId>")]
async fn apiGetChannelMetadata(channelId: String, cache: &rocket::State<ChannelCache>) -> Result<String, (Status, String)> {
	// Upstream/YouTube failures are reported as 503 instead of panicking; only a
	// serialization bug can produce a 500.
	let channel = getChannelMetadata(channelId, (**cache).clone())
		.await
		.map_err(|e| (Status::ServiceUnavailable, e))?;
	return serde_json::to_string(&channel)
		.map_err(|e| (Status::InternalServerError, e.to_string()))
}

// Flattens one innertube search hit into the `Video` shape the API/frontend will
// return. `SearchResultItem` is an enum (video/channel/playlist), so callers match
// on `SearchResultItem::Video` and pass the inner `SearchVideoItem` in here.
async fn searchVideoItemToVideo(item: &SearchVideoItem, cache: ChannelCache) -> Video {
	// YouTube lists thumbnails smallest-to-largest, but pick the largest explicitly
	// so the order of `thumbnails` can't silently degrade the image quality.
	let thumbnail = item
		.thumbnails
		.iter()
		.max_by_key(|thumb| thumb.width.saturating_mul(thumb.height))
		.map(|thumb| thumb.url.clone())
		.unwrap_or_default();

	// `channel_id` can be empty for odd result types, so avoid emitting a bare URL.
	let channel = if item.channel_id.is_empty() {
		String::new()
	} else {
		item.channel_id.clone()
	};

	// The avatar is cosmetic, so a failed lookup (or a channel without one) falls
	// back to the placeholder innertube itself returns for unknown channels rather
	// than failing the whole search page.
	let channelAvatar = if channel.is_empty() {
		defaultAvatar.to_string()
	} else {
		match getChannelMetadata(channel.clone(), cache).await {
			Ok(metadata) => metadata.avatar.unwrap_or_else(|| defaultAvatar.to_string()),
			Err(e) => {
				eprintln!("[PROFILE] channel metadata lookup failed for '{}': {}", channel, e);
				defaultAvatar.to_string()
			}
		}
	};

	return Video {
		id: item.video_id.clone(),
		thumbnail: thumbnail,
		channel: channel,
		channelAvatar: channelAvatar,
		channelName: item.author.clone(),
		title: item.title.clone()
	};
}

// API
#[get("/search?<q>&<page>")]
async fn apiSearch(
	q: String,
	page: u64,
	cache: &rocket::State<SearchCache>,
	channelCache: &rocket::State<ChannelCache>
) -> Result<String, (Status, String)> {
	// Pages are 1-based; reject anything past 256 instead of walking an
	// unbounded number of upstream continuation requests.
	if page > 256 {
		return Err((Status::BadRequest, format!("page must be 1-256, got {}", page)));
	}

	let totalStart = Instant::now();

	let yt = innertube().await.map_err(|e| (Status::ServiceUnavailable, e))?;

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

		return serde_json::to_string(&cachedResults)
			.map_err(|e| (Status::InternalServerError, e.to_string()))
	}

	let searchStart = Instant::now();

	let mut resultPage = yt
		.search(&q, None)
		.await
		.map_err(|e| (Status::ServiceUnavailable, e.to_string()))?;

	// The cache stores `Video`s, so channels/playlists are dropped here. Taking a
	// slice keeps `resultPage` intact, since we still need its continuation token.
	let toVideos = async |items: &[SearchResultItem]| -> Vec<Video> {
		let mut videos = Vec::new();
		for item in items {
			if let SearchResultItem::Video(video) = item {
				videos.push(searchVideoItemToVideo(video, (**channelCache).clone()).await);
			}
		}
		videos
	};

	// Navigate to requested page
	for walkingPage in 1..page {
		let walkingCacheKey = (q.clone(), walkingPage);
		if let Some(token) = resultPage.continuation_token.as_deref() {
			resultPage = yt
				.search(&q, Some(token))
				.await
				.map_err(|e| (Status::ServiceUnavailable, e.to_string()))?;
			cache.insert(walkingCacheKey, toVideos(&resultPage.items).await).await;
		} else {
			break;
		}
	}

	let videos = toVideos(&resultPage.items).await;
	cache.insert(cacheKey, videos.clone()).await;

	eprintln!(
		"[PROFILE] yt.search() network time: {:?}",
		searchStart.elapsed()
	);

	eprintln!(
		"[PROFILE] Total handler time: {:?}",
		totalStart.elapsed()
	);

	return serde_json::to_string(&videos)
		.map_err(|e| (Status::InternalServerError, e.to_string()))
}
#[get("/")]
fn apiIndex() -> &'static str {
	"
/api/m3u8/<video> - Get m3u8 master playlist for a video.
/api/m3u8/<video>/proxy/<token> - HLS segment CORS proxy.
/api/search?q=<search query>&page=(page 1-256) - Get array of videos for a search query.
/api/channelMetadata?channelId=<channel id> - Get channel metadata for channelId

Structs that are given in responses:

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Video {
	id: String,
	thumbnail: String,
	channel: String,
	channelAvatar: String,
	channelName: String,
	title: String
}
#[derive(Serialize, Deserialize, Debug, Clone)]
struct Channel {
	id: String,
	name: String,
	avatar: Option<String>,
	description: Option<String>,
	banner: Option<String>
}
	"
}

// ADVANCED ROCKET CONCEPT: Custom Request Guard
// Instead of injecting `&Request` directly (which Rocket doesn't support as a guard), 
// we define a custom struct and implement `FromRequest`. Rocket automatically runs this 
// code before calling any handler that asks for `UserAgent`.
struct UserAgent(String);

#[rocket::async_trait]
impl<'r> rocket::request::FromRequest<'r> for UserAgent {
	type Error = std::convert::Infallible;

	async fn from_request(req: &'r rocket::Request<'_>) -> rocket::request::Outcome<Self, Self::Error> {
		let agent = req.headers().get_one("User-Agent").unwrap_or("Unknown").to_string();
		rocket::request::Outcome::Success(UserAgent(agent))
	}
}

#[get("/health")]
fn health(userAgent: UserAgent) -> Result<&'static str, Status> {
	if (userAgent.0.contains("opencode")) {
		return Err(Status::Forbidden);
	}
	return Ok("OK");
}

#[launch]
fn rocket() -> _ {
	let searchCache: SearchCache = Cache::builder()
		.time_to_live(Duration::from_hours(1))
		.max_capacity(5000)
		.build();
	let channelCache: ChannelCache = Cache::builder()
		.max_capacity(5000)
		.time_to_live(Duration::from_hours(24))
		.build();

	rocket::build()
		.manage(searchCache)
		.manage(channelCache)
		.attach(AdHoc::on_ignite("Innertube Warmup", |rocket| Box::pin(async move {
			let start = Instant::now();
			let _ = innertube().await;
			eprintln!("[STARTUP] Innertube pre-warmed in {:?}", start.elapsed());
			rocket
		})))
		.mount("/", routes![index, health])
		.mount("/api", routes![apiIndex, apiSearch, apiGetChannelMetadata])
}
