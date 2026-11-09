use innertube_rs::{SearchResultItem, SearchVideoItem};
use serde::{Serialize, Deserialize};
use rocket::http::Status;
use crate::innertube;

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

pub async fn getChannelMetadata(channelId: String, cache: crate::ChannelCache) -> Result<Channel, String> {
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

// Flattens one innertube search hit into the `Video` shape the API/frontend will
// return. `SearchResultItem` is an enum (video/channel/playlist), so callers match
// on `SearchResultItem::Video` and pass the inner `SearchVideoItem` in here.
pub async fn searchVideoItemToVideo(item: &SearchVideoItem, cache: crate::ChannelCache) -> Video {
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

pub async fn ytSearch(
	q: String,
	page: u64,
	cache: crate::SearchCache,
	channelCache: crate::ChannelCache
) -> Result<Vec<Video>, (Status, String)> {
	let yt = innertube().await.map_err(|e| (Status::ServiceUnavailable, e))?;

	// Cache key includes both query and page
	let cacheKey = (q.clone(), page);

	// Check cache
	if let Some(cachedResults) = cache.get(&cacheKey).await {
		return Ok(cachedResults);
	}

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
				videos.push(searchVideoItemToVideo(video, channelCache.clone()).await);
			}
		}
		videos
	};

	// Navigate to requested page
	for walkingPage in 1..page {
		let walkingCacheKey = (q.clone(), walkingPage + 1);
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

	return Ok(videos);
}
