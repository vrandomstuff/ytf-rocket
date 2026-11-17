use crate::innertube;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use innertube_rs::{SearchResultItem, SearchVideoItem};
use rocket::http::Status;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

// `Hmac` is generic over the digest, so alias the SHA-256 instantiation once.
type HmacSha256 = Hmac<Sha256>;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Video {
	id: String,
	thumbnail: String,
	channel: String,
	channelAvatar: String,
	channelName: String,
	title: String
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Channel {
	id: String,
	name: String,
	avatar: Option<String>,
	description: Option<String>,
	banner: Option<String>
}

// innertube resolves unknown channels to this placeholder avatar, so reuse it
// instead of emitting an empty string (it is a stable permalink).
const defaultAvatar: &str = "https://yt3.googleusercontent.com/a/default-user=s100-c-k-c0x00ffffff-no-rj";

pub fn generateCorsUrl(url: String, keyString: String, rewriteContent: bool) -> Result<String, (Status, String)> {
	let urlCORSToken = CORSToken::new(url, keyString.clone(), rewriteContent)?;
	let urlTokenJson = serde_json::to_string(&urlCORSToken).map_err(|e| (Status::InternalServerError, e.to_string()))?;
	let urlToken = URL_SAFE_NO_PAD.encode(urlTokenJson);
	return Ok(format!("/api/v1/cors/{}", urlToken));
}

pub async fn getChannelMetadata(channelId: String, cache: crate::ChannelCache, keyString: String) -> Result<Channel, (Status, String)> {
	if let Some(cachedResults) = cache.get(&channelId).await {
		return Ok(cachedResults);
	}
	let yt = innertube().await.map_err(|e| (Status::ServiceUnavailable, e))?;

	let about = yt
		.get_channel_about(&channelId)
		.await
		.map_err(|e| (Status::ServiceUnavailable, e.to_string()))?;
	// `transpose` turns `Option<Result<T, E>>` into `Result<Option<T>, E>` so a
	// missing banner/avatar stays `None` while a failing token encode propagates.
	let banner = about.banner.map(|url| generateCorsUrl(url, keyString.clone(), false)).transpose()?;
	let avatar = about.avatar.map(|url| generateCorsUrl(url, keyString.clone(), false)).transpose()?;
	let channel = Channel {
		id: about.channel_id,
		name: about.title,
		avatar: avatar,
		description: about.description,
		banner: banner
	};
	cache.insert(channelId, channel.clone()).await;

	return Ok(channel);
}

// Flattens one innertube search hit into the `Video` shape the API/frontend will
// return. `SearchResultItem` is an enum (video/channel/playlist), so callers match
// on `SearchResultItem::Video` and pass the inner `SearchVideoItem` in here.
pub async fn searchVideoItemToVideo(item: &SearchVideoItem, cache: crate::ChannelCache, keyString: String) -> Result<Video, (Status, String)> {
	// YouTube lists thumbnails smallest-to-largest, but pick the largest explicitly
	// so the order of `thumbnails` can't silently degrade the image quality.
	let thumbnail = item
		.thumbnails
		.iter()
		.max_by_key(|thumb| thumb.width.saturating_mul(thumb.height))
		.map(|thumb| thumb.url.clone())
		.unwrap_or_default();
	let thumbnailUrl = generateCorsUrl(thumbnail, keyString.clone(), false)?;
	// `channel_id` can be empty for odd result types, so avoid emitting a bare URL.
	let channel = if item.channel_id.is_empty() { String::new() } else { item.channel_id.clone() };

	// The avatar is cosmetic, so a failed lookup (or a channel without one) falls
	// back to the placeholder innertube itself returns for unknown channels rather
	// than failing the whole search page.
	let channelAvatar = if channel.is_empty() {
		generateCorsUrl(defaultAvatar.to_string(), keyString.clone(), false)?
	} else {
		match getChannelMetadata(channel.clone(), cache, keyString.clone()).await {
			Ok(metadata) => metadata.avatar.unwrap_or_else(|| defaultAvatar.to_string()),
			Err((_status, _e)) => {
				generateCorsUrl(defaultAvatar.to_string(), keyString.clone(), false)?
			}
		}
	};

	return Ok(Video {
		id: item.video_id.clone(),
		thumbnail: thumbnailUrl,
		channel: channel,
		channelAvatar: channelAvatar,
		channelName: item.author.clone(),
		title: item.title.clone()
	});
}

pub async fn ytSearch(
	q: String,
	page: u64,
	cache: crate::SearchCache,
	channelCache: crate::ChannelCache,
	keyString: String
) -> Result<Vec<Video>, (Status, String)> {
	let yt = innertube().await.map_err(|e| (Status::ServiceUnavailable, e))?;

	// Cache key includes both query and page
	let cacheKey = (q.clone(), page);

	// Check cache
	if let Some(cachedResults) = cache.get(&cacheKey).await {
		return Ok(cachedResults);
	}

	let mut resultPage = yt.search(&q, None).await.map_err(|e| (Status::ServiceUnavailable, e.to_string()))?;

	// The cache stores `Video`s, so channels/playlists are dropped here. Taking a
	// slice keeps `resultPage` intact, since we still need its continuation token.
	let toVideos = async |items: &[SearchResultItem]| -> Result<Vec<Video>, (Status, String)> {
		let mut videos = Vec::new();
		for item in items {
			if let SearchResultItem::Video(video) = item {
				videos.push(searchVideoItemToVideo(video, channelCache.clone(), keyString.clone()).await?);
			}
		}
		return Ok(videos);
	};

	// Navigate to requested page
	for walkingPage in 1..page {
		let walkingCacheKey = (q.clone(), walkingPage + 1);
		if let Some(token) = resultPage.continuation_token.as_deref() {
			resultPage = yt.search(&q, Some(token)).await.map_err(|e| (Status::ServiceUnavailable, e.to_string()))?;
			cache.insert(walkingCacheKey, toVideos.clone()(&resultPage.items).await?).await;
		} else {
			break;
		}
	}

	let videos = toVideos(&resultPage.items).await?;
	cache.insert(cacheKey, videos.clone()).await;

	return Ok(videos);
}

// Fields are `pub` because `main.rs` reads `url` and `exp` when handling
// `/cors/<token>`; the serialized JSON shape (and its signature) is unchanged.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CORSToken {
	pub url: String,
	pub signature: String,
	pub exp: u64,
	pub rewriteResponseContent: bool
}

impl CORSToken {
	pub fn new(toSign: String, secret: String, rewriteContent: bool) -> Result<CORSToken, (Status, String)> {
		let expiresAt = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() + 25 * 60 * 60 * 1000) as u64; // 25 hours
		let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|e| (Status::InternalServerError, e.to_string()))?;

		mac.update(toSign.as_bytes());
		mac.update(&[u8::from(rewriteContent)]);
		mac.update(&expiresAt.to_ne_bytes());
		let result = mac.finalize();

		let tag = result.into_bytes();

		return Ok(CORSToken {
			url: toSign,
			signature: hex::encode(tag),
			exp: expiresAt,
			rewriteResponseContent: rewriteContent
		});
	}

	pub fn verify(&self, secret: String) -> bool {
		let now = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()) as u64;
		if (now > self.exp) {
			return false;
		}

		let tag = hex::decode(self.signature.clone()).unwrap_or("invalid tag".as_bytes().to_vec());

		let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("Invalid key");

		// Must feed the MAC the exact same fields, in the same order, as `new`;
		// a missing one here makes every token fail verification.
		mac.update(self.url.as_bytes());
		mac.update(&[u8::from(self.rewriteResponseContent)]);
		mac.update(&self.exp.to_ne_bytes());

		return mac.verify_slice(&tag).is_ok();
	}
}
