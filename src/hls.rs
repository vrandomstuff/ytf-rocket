use crate::innertube;
use innertube_rs::models::video::GetVideoInfoOptions;
use rocket::http::Status;

#[derive(Debug)]
struct AudioRendition {
	group_id: String,
	url: String,
	is_original: bool
}

#[derive(Debug)]
struct Variant {
	bandwidth: u64,
	resolution: String,
	codecs: Option<String>,
	// `AUDIO="<group>"` names the audio rendition group this variant should be
	// paired with; the master groups audio per codec/language, so this is the only
	// reliable way to pick the track YouTube intends for a given video rendition.
	audio_group: Option<String>,
	url: String
}

// `Clone` lets `apiGetM3u8` hand the parsed renditions to the cache while still
// using them to build the playlist for the current request.
#[derive(Debug, Clone)]
pub struct M3u8 {
	pub title: String,
	pub audio: String,
	pub video: String,
	pub videoRes: String,
	pub videoBandwidth: String
}

/// Read a `KEY=value` or `KEY="value"` attribute from an HLS tag payload.
fn attribute<'a>(line: &'a str, key: &str) -> Option<&'a str> {
	let prefix = format!("{key}=");
	let mut offset = 0;

	while let Some(found) = line[offset..].find(&prefix) {
		let start = offset + found;
		// Skip matches that are part of a longer attribute name.
		if start == 0 || line.as_bytes()[start - 1] == b',' {
			let value = &line[start + prefix.len()..];
			return if let Some(quoted) = value.strip_prefix('"') {
				quoted.find('"').map(|end| &quoted[..end])
			} else {
				let end = value.find(',').unwrap_or(value.len());
				Some(value[..end].trim())
			};
		}
		offset = start + prefix.len();
	}

	None
}

fn parse_master_playlist(text: &str) -> (Vec<AudioRendition>, Vec<Variant>) {
	let mut audios = Vec::new();
	let mut variants = Vec::new();
	let mut pending_variant: Option<(u64, String, Option<String>, Option<String>)> = None;

	for raw_line in text.lines() {
		let line = raw_line.trim();

		if let Some(tags) = line.strip_prefix("#EXT-X-MEDIA:") {
			if attribute(tags, "TYPE") != Some("AUDIO") {
				continue;
			}
			let Some(url) = attribute(tags, "URI") else {
				continue;
			};
			let group_id = attribute(tags, "GROUP-ID").unwrap_or_default().to_string();
			let is_original =
				url.contains("acont%3Doriginal") || url.contains("acont=original") || attribute(tags, "NAME").is_some_and(|name| name.ends_with("- original"));

			audios.push(AudioRendition {
				group_id,
				url: url.to_string(),
				is_original
			});
		} else if let Some(tags) = line.strip_prefix("#EXT-X-STREAM-INF:") {
			let bandwidth = attribute(tags, "BANDWIDTH").and_then(|value| value.parse().ok()).unwrap_or(0);
			let resolution = attribute(tags, "RESOLUTION").unwrap_or_default().to_string();
			let codecs = attribute(tags, "CODECS").map(str::to_string);
			let audio_group = attribute(tags, "AUDIO").map(str::to_string);
			pending_variant = Some((bandwidth, resolution, codecs, audio_group));
		} else if !line.is_empty() && !line.starts_with('#') {
			if let Some((bandwidth, resolution, codecs, audio_group)) = pending_variant.take() {
				variants.push(Variant {
					bandwidth,
					resolution,
					codecs,
					audio_group,
					url: line.to_string()
				});
			}
		}
	}

	(audios, variants)
}

/// Read the audio itag out of a rendition URL (`.../itag/234/...`). Higher values
/// are the better encoding of the same track (233 is HE-AAC, 234 is AAC-LC), and
/// it is 0 when the URL carries no itag so the rendition still sorts.
fn url_itag(url: &str) -> u32 {
	let mut segments = url.split('/');

	while let Some(segment) = segments.next() {
		if segment == "itag" {
			return segments.next().and_then(|itag| itag.parse().ok()).unwrap_or(0);
		}
	}

	return 0;
}

pub async fn getM3u8(videoId: String) -> Result<M3u8, (Status, String)> {
	let yt = innertube().await.map_err(|e| (Status::ServiceUnavailable, e))?;
	let options = GetVideoInfoOptions {
		client: Some("VISIONOS".into()),
		..Default::default()
	};
	let info = yt
		.get_basic_info(&videoId, Some(&options))
		.await
		.map_err(|e| (Status::ServiceUnavailable, e.to_string()))?;
	// `videoDetails` is optional in innertube-rs and only supplies the cosmetic
	// title for the web player, so fall back to a placeholder instead of failing
	// the playlist for a response that still has usable `streamingData`.
	let title = info
		.player_response
		.video_details
		.map(|videoDetails| videoDetails.title)
		.unwrap_or_else(|| "Unknown".to_string());
	let hls_url = info
		.player_response
		.streaming_data
		.as_ref()
		.and_then(|sd| sd.hls_manifest_url.as_deref())
		.ok_or((Status::BadGateway, "No HLS manifest URL in player response".to_string()))?;

	// The variant master groups audio into many language renditions that some
	// players (e.g. VLC, most likely hls.js) handle poorly. Fetch it and build a simple two-track
	// master instead: the original audio plus the 720p AVC video variant.
	let manifest = yt
		.session
		.http_client
		.get(hls_url)
		.send()
		.await
		.map_err(|e| (Status::BadGateway, e.to_string()))?
		.error_for_status()
		.map_err(|e| (Status::BadGateway, e.to_string()))?
		.text()
		.await
		.map_err(|e| (Status::BadGateway, e.to_string()))?;

	let (audios, variants) = parse_master_playlist(&manifest);

	// Consume the vectors so the chosen renditions can be moved into the result.
	let video = variants
		.into_iter()
		.find(|variant| {
			variant.resolution.ends_with("x720") // TODO: make it support videos without 720p (example being me at the zoo)
				&& variant
					.codecs
					.as_deref()
					.is_some_and(|c| c.starts_with("avc1"))
		})
		.ok_or((Status::BadGateway, "No 720p AVC video variant in HLS master playlist".to_string()))?;

	// Pick the audio the chosen variant references via `AUDIO="<group>"`; YouTube's
	// manifest carries no `acont=original` marker any more, so filtering on that
	// alone found nothing. Order of preference: the original-language rendition,
	// then the group this variant belongs to, then the best audio itag.
	let audio = audios
		.into_iter()
		.max_by_key(|rendition| {
			let group_match = video.audio_group.as_deref().is_some_and(|group| group == rendition.group_id);

			(rendition.is_original, group_match, url_itag(&rendition.url))
		})
		.ok_or((Status::BadGateway, "No audio rendition in HLS master playlist".to_string()))?;

	return Ok(M3u8 {
		title,
		audio: audio.url,
		video: video.url,
		videoRes: video.resolution,
		videoBandwidth: video.bandwidth.to_string()
	});
}
