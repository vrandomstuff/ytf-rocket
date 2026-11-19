type HlsType = typeof import("hls.js").default;

const Hls = window.Hls as HlsType;

import { API_BASE_URL } from "./api.js";

const params = new URLSearchParams(window.location.search);

const vid = params.get("v") ?? "";

// Shown in place of the title when the playlist cannot be fetched. The usual
// cause is a video the VISIONOS player response refuses to serve (made for kids,
// private, age-restricted), which /m3u8 answers with a 502/503 rather than a
// transport error.
const playlistError =
	"The video could not be loaded. This can happen for videos marked made for kids, private or age-restricted videos, or when the server is unreachable.";

async function playHls(url: string, videoPlayer: HTMLVideoElement) {
	if (!(videoPlayer instanceof HTMLVideoElement)) {
		throw new Error("videoPlayer given to playHls is not a videoPlayer");
	}

	const titleElement = document.getElementById("videoTitle");

	if (!(titleElement instanceof HTMLHeadingElement)) {
		throw new Error("unable to get titleElement");
	}

	// The playlist is read here anyway to pull out `#VIDEO_TITLE`, so it is
	// fetched once and then handed to hls.js as a blob instead of a second
	// request for the same text.
	let playlist: string;

	try {
		const response = await fetch(url);

		if (!response.ok) {
			throw new Error(`Failed to fetch playlist: ${response.status}`);
		}

		playlist = await response.text();
	} catch (e) {
		console.error(e);
		titleElement.textContent = playlistError;
		return;
	}

	titleElement.textContent =
		playlist.match(/^#VIDEO_TITLE:(.*)$/m)?.[1] ?? "Unknown";

	const blob = new Blob([playlist], {
		type: "application/vnd.apple.mpegurl"
	});
	const blobUrl = URL.createObjectURL(blob);

	if (Hls.isSupported()) {
		const hls = new Hls({});
		hls.loadSource(blobUrl);
		hls.attachMedia(videoPlayer);

		hls.on(Hls.Events.MEDIA_ATTACHED, function () {
			videoPlayer.play();
		});

		// A fatal error is only logged: neither the instance nor the blob URL is
		// torn down, so whatever was buffered stays replayable and a reload of the
		// page is not the only way back. The blob is freed when the document goes
		// away.
		hls.on(Hls.Events.ERROR, function (_event, data) {
			if (data.fatal) {
				console.error("Fatal hls.js error", data);
			}
		});
	} else if (videoPlayer.canPlayType("application/vnd.apple.mpegurl")) {
		videoPlayer.src = blobUrl;

		videoPlayer.addEventListener(
			"canplay",
			function () {
				videoPlayer.play();
			},
			{ once: true }
		);
	}
}

if (vid) {
	const player = document.getElementById("videoPlayer");

	if (!(player instanceof HTMLVideoElement)) {
		throw new Error("videoPlayer is not a video player");
	}

	// `playHls` shows playlist failures in the title, but the programmer-error
	// throws above would otherwise end up as unhandled rejections.
	playHls(`${API_BASE_URL}/m3u8/${vid}`, player).catch(function (e) {
		console.error(e);
	});
}
