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

mod wrappers;

// `build.rs` generates this file in OUT_DIR from `web/dist/`. It contains one
// `#[get]` handler per frontend file plus `fileRoutes()`, which returns all of
// them as a `Vec<rocket::Route>` so they can be mounted in one call.
mod webAssets {
	include!(concat!(env!("OUT_DIR"), "/webAssets.rs"));
}

use wrappers::{Video, Channel, CORSToken, getChannelMetadata, ytSearch};
use innertube_rs::Innertube;
use tokio::sync::OnceCell;
use std::time::{Instant, Duration};
use moka::future::Cache;
use rocket::fairing::AdHoc;
use rocket::http::Status;
use rocket::request::Request;
use rocket::response::{Response, Responder};
use std::io::Cursor;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::{
	header::{
		HeaderMap, HeaderValue, ACCEPT_LANGUAGE, USER_AGENT
	},
	Client
};

// Global Innertube client container
static innertubeCache: OnceCell<Innertube> = OnceCell::const_new();
// In-memory cache for search results (expires after 1 hour, max 5000 entries).
// Stores the flattened `Video` shape so a cache hit can be serialized directly.
type SearchCache = Cache<(String, u64), Vec<Video>>;

pub async fn innertube() -> Result<&'static Innertube, String> {
	innertubeCache
		.get_or_try_init(|| async {
			let result = Innertube::new()
				.await
				.map_err(|e| e.to_string());

			return result;
		})
		.await
}
// TTL: 1 day Max: 5000
type ChannelCache = Cache<String, Channel>;

#[get("/channelMetadata?<channelId>")]
async fn apiGetChannelMetadata(channelId: String, cache: &rocket::State<ChannelCache>, keyString: &rocket::State<String>) -> Result<String, (Status, String)> {
	// Upstream/YouTube failures are reported as 503 instead of panicking; only a
	// serialization bug can produce a 500.
	let channel = getChannelMetadata(channelId, (**cache).clone(), (**keyString).clone()).await?;
	return serde_json::to_string(&channel)
		.map_err(|e| (Status::InternalServerError, e.to_string()))
}

// API
#[get("/search?<q>&<page>")]
async fn apiSearch(
	q: String,
	page: u64,
	cache: &rocket::State<SearchCache>,
	channelCache: &rocket::State<ChannelCache>,
	keyString: &rocket::State<String>
) -> Result<String, (Status, String)> {
	// Pages are 1-based; reject anything past 256 instead of walking an
	// unbounded number of upstream continuation requests.
	if page > 256 {
		return Err((Status::BadRequest, format!("page must be 1-256, got {}", page)));
	}

	let videos = ytSearch(q, page, (**cache).clone(), (**channelCache).clone(), (**keyString).clone()).await?;

	return serde_json::to_string(&videos)
		.map_err(|e| (Status::InternalServerError, e.to_string()))
}

// Rocket's built-in responders for `Vec<u8>` / `(Status, T)` have no slot for
// extra headers, and `/cors/<token>` needs to echo the upstream ones, so the
// body, headers and upstream status travel together here and become a `Response`
// in `respond_to`.
struct CorsProxyResponse {
	status: Status,
	body: Vec<u8>,
	headers: Vec<(String, String)>
}

impl<'r> Responder<'r, 'static> for CorsProxyResponse {
	fn respond_to(self, _request: &'r Request<'_>) -> rocket::response::Result<'static> {
		let mut response = Response::new();

		// Forward the upstream status verbatim. Using `set_status` here means a
		// 404/500 from YouTube reaches the browser as a 404/500 instead of a 200
		// carrying an error page, so `<img>`/hls.js can react to it.
		response.set_status(self.status);

		for (name, value) in self.headers {
			// `adjoin_raw_header` adds rather than replaces, so repeated upstream
			// headers (several `cache-control` values, for instance) all survive.
			// Owned `String`s satisfy `Into<Cow<'r, str>>` for any `'r`, so no
			// `&'static` data is needed to build headers whose names are only known
			// at runtime.
			response.adjoin_raw_header(name, value);
		}

		// `set_sized_body` also makes Rocket emit `Content-Length` for us, which is
		// why an upstream `content-length` must never be copied through.
		response.set_sized_body(self.body.len(), Cursor::new(self.body));

		return Ok(response)
	}
}

// Headers that must not be echoed back to the client. The upstream response
// describes YouTube's origin/connection, so forwarding these would either apply
// the wrong policy to *our* origin or make the browser reject the response:
// - `content-length` is recomputed by Rocket from the body we actually write,
// - the hop-by-hop headers describe the upstream TCP connection, not the response,
// - CORS/embedding headers are checked against our origin, not YouTube's, and an
//   upstream `access-control-allow-origin` can contradict our own CORS answers,
// - cookie headers would set/clear cookies for our origin,
// - HSTS/Alt-Svc/pinning/`clear-site-data` get remembered for our origin, so an
//   upstream one would silently change how the browser talks to us,
// - `x-content-type-options: nosniff` breaks loaded assets when the upstream sends
//   no `content-type` and the fallback below is `application/octet-stream`,
// - `vary`/`report-to`/`nel`/`link`/`refresh` change how our response is cached,
//   reported, preloaded or followed, and none of them describe the body,
// - `www-authenticate` is dropped because a login prompt for our origin makes no
//   sense: the proxy may return the upstream 401, but the credentials being asked
//   for are YouTube's, not ours.
// Deliberately *not* skipped: `content-type`, `content-encoding` (reqwest strips it
// when it decompresses), `content-range`/`accept-ranges` (needed for HLS seeking),
// `cache-control`/`etag`/`last-modified`/`expires` (the proxy sets no cache policy).
const proxySkippedHeaders: [&str; 45] = [
	"content-length",
	"transfer-encoding",
	"connection",
	"keep-alive",
	"proxy-connection",
	"upgrade",
	"te",
	"trailer",
	"proxy-authenticate",
	"proxy-authorization",
	"access-control-allow-origin",
	"access-control-allow-credentials",
	"access-control-allow-methods",
	"access-control-allow-headers",
	"access-control-expose-headers",
	"access-control-max-age",
	"timing-allow-origin",
	"cross-origin-embedder-policy",
	"cross-origin-opener-policy",
	"cross-origin-resource-policy",
	"vary",
	"set-cookie",
	"set-cookie2",
	"cookie",
	"strict-transport-security",
	"alt-svc",
	"public-key-pins",
	"public-key-pins-report-only",
	"expect-ct",
	"clear-site-data",
	"content-security-policy",
	"content-security-policy-report-only",
	"x-frame-options",
	"x-content-type-options",
	"x-xss-protection",
	"permissions-policy",
	"feature-policy",
	"referrer-policy",
	"origin-agent-cluster",
	"report-to",
	"reporting-endpoints",
	"nel",
	"www-authenticate",
	"link",
	"refresh"
];

#[get("/cors/<token>")]
async fn corsProxy(token: &str, keyString: &rocket::State<String>) -> Result<CorsProxyResponse, (Status, String)> {
	let decodedTokenAsBytes: Vec<u8> = URL_SAFE_NO_PAD.decode(&token).map_err(|e| (Status::BadRequest, e.to_string()))?;
	let decodedToken: String = String::from_utf8(decodedTokenAsBytes).map_err(|e| (Status::BadRequest, e.to_string()))?;
	let corsToken: CORSToken = serde_json::from_str(&decodedToken).map_err(|e| (Status::BadRequest, e.to_string()))?;

	if (!corsToken.verify(keyString.to_string())) {
		return Err((Status::Unauthorized, "The CORSToken has expired or is not valid".to_string()));
	}

	let mut headers = HeaderMap::new();

	headers.insert(
		USER_AGENT,
		HeaderValue::from_static(
			"Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36"
		),
	);

	headers.insert(
		ACCEPT_LANGUAGE,
		HeaderValue::from_static("en-US,en;q=0.9"),
	);

	headers.insert(
		"sec-ch-ua",
		HeaderValue::from_static(
			"\"Google Chrome\";v=\"156\", \
			 \"Chromium\";v=\"156\", \
			 \"Not_A Brand\";v=\"8\"",
		),
	);

	headers.insert(
		"sec-ch-ua-mobile",
		HeaderValue::from_static("?0"),
	);

	headers.insert(
		"sec-ch-ua-platform",
		HeaderValue::from_static("\"Windows\""),
	);

	let client = Client::builder()
		.default_headers(headers)
		.build()
		.map_err(|e| (Status::InternalServerError, e.to_string()))?;

	// `build()` returns a `Result`, so it needs `?` above; `send()`/`bytes()`
	// are both fallible upstream calls, hence the `map_err` on each.
	let response = client
		.get(corsToken.url) // already verified
		.send()
		.await
		.map_err(|e| (Status::BadGateway, e.to_string()))?;

	// Read the status before `bytes()` consumes the response. `Status::new` takes
	// any code (non-standard ones get a class-based reason phrase) and reqwest only
	// ever yields 100..=599, so the upstream code always maps to something sendable.
	let upstreamStatus = Status::new(response.status().as_u16());

	// Copy the upstream headers before `bytes()` consumes the response. Each
	// value is copied on its own because `HeaderMap::iter` yields one item per
	// value, which keeps repeated header names (several `cache-control` values,
	// for instance) intact.
	let mut responseHeaders: Vec<(String, String)> = response
		.headers()
		.iter()
		.filter(|(name, _)| !proxySkippedHeaders.contains(&name.as_str()))
		.filter_map(|(name, value)| {
			// Header values are ASCII by definition; skip a non-ASCII one rather
			// than failing the whole response over a single odd header.
			return value.to_str().ok().map(|value| (name.as_str().to_string(), value.to_string()))
		})
		.collect();

	if (!responseHeaders.iter().any(|(name, _)| name == "content-type")) {
		// The old `Vec<u8>` responder always answered with `application/octet-stream`,
		// so keep an equivalent fallback when the upstream sends no content type.
		responseHeaders.push(("content-type".to_string(), "application/octet-stream".to_string()));
	}

	let body = response
		.bytes()
		.await
		.map(|body| body.to_vec())
		.map_err(|e| (Status::BadGateway, e.to_string()))?;

	return Ok(CorsProxyResponse { status: upstreamStatus, body, headers: responseHeaders });
}

// debug endpoints
#[get("/debugGetVideoCache")]
fn debugGetVideoCache(cache: &rocket::State<SearchCache>) -> Result<String, (Status, String)> {
	#[cfg(debug_assertions)]
	{
		let contents: Vec<Video> = cache
			.iter()
			.flat_map(|(_key, videos)| videos)
			.collect();

		return serde_json::to_string(&contents)
			.map_err(|e| (Status::InternalServerError, e.to_string()));
	}
	#[cfg(not(debug_assertions))]
	{
		let _ = cache;
		return Ok("Only in debug mode".to_string());
	}
}

#[get("/debugGetChannelCache")]
fn debugGetChannelCache(cache: &rocket::State<ChannelCache>) -> Result<String, (Status, String)> {
	#[cfg(debug_assertions)]
	{
		let contents: Vec<_> = cache
			.iter()
			.map(|(_key, channel)| channel)
			.collect();

		return serde_json::to_string(&contents)
			.map_err(|e| (Status::InternalServerError, e.to_string()));
	}
	#[cfg(not(debug_assertions))]
	{
		let _ = cache;
		return Ok("Only in debug mode".to_string());
	}
}

#[get("/debugGenerateCorsToken?<toSign>")]
fn debugGenerateCorsToken(toSign: String, keyString: &rocket::State<String>) -> Result<String, (Status, String)> {
	#[cfg(debug_assertions)]
	{
		let token = CORSToken::new(toSign, keyString.to_string())?;
		return serde_json::to_string(&token)
			.map_err(|e| (Status::InternalServerError, e.to_string()));
	}
	#[cfg(not(debug_assertions))]
	{
		let _ = keyString;
		return Ok("Only in debug mode".to_string());
	}
}

#[get("/")]
fn apiIndex() -> &'static str {
	"
/api/m3u8/<video> - Get m3u8 master playlist for a video.
/api/cors/<token> - CORS proxy, token is a base64 encoded json object see the CORSToken struct. The upstream body, status code and response headers are returned (headers minus hop-by-hop, CORS, cookie, caching-policy and browser-policy ones, see proxySkippedHeaders). Only proxy-level failures use this server's own codes (400 bad token, 401 expired/bad signature, 502 upstream transport error).
/api/search?q=<search query>&page=(page 1-256) - Get array of videos for a search query.
/api/channelMetadata?channelId=<channel id> - Get channel metadata for channelId
/api/debugGetVideoCache - Get the contents of the video cache DEBUG MODE ONLY
/api/debugGetChannelCache - Get the contents of the channel cache DEBUG MODE ONLY

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
#[derive(Serialize, Deserialize, Debug, Clone)]
struct CORSToken {
	url: String,
	signature: String
	exp: u64
}
	"
}

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

	let mut keyBytes = [0u8; 32];

	getrandom::fill(&mut keyBytes).unwrap();

	let keyString = hex::encode(keyBytes);

	rocket::build()
		.manage(searchCache)
		.manage(channelCache)
		.manage(keyString)
		.attach(AdHoc::on_ignite("Innertube Warmup", |rocket| Box::pin(async move {
			let start = Instant::now();
			let _ = innertube().await;
			eprintln!("[STARTUP] Innertube pre-warmed in {:?}", start.elapsed());
			rocket
		})))
		.mount("/", webAssets::fileRoutes())
		.mount("/", routes![health])
		.mount("/api/v1", routes![apiIndex, apiSearch, apiGetChannelMetadata, corsProxy, debugGetVideoCache, debugGetChannelCache, debugGenerateCorsToken]) // increment the v number when making breaking changes in a clone of this line and add wrappers for the old versions to work
}
