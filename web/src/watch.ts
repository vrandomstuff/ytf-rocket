type AlpineType = typeof import("alpinejs");

const AlpineInstance = window.Alpine as AlpineType;

type HlsType = typeof import("hls.js").default;

const Hls = window.Hls as HlsType;

import { API_BASE_URL } from "./api.js";
import type { Video } from "./api.js";

const params = new URLSearchParams(window.location.search);

const vid = params.get("v") ?? "";

function playHls(url: string, videoPlayer: HTMLVideoElement) {
	if (!(videoPlayer instanceof HTMLVideoElement)) {
		throw new Error("videoPlayer player given to playHls is not a videoPlayer player");
	}
	if (Hls.isSupported()) {
		var hls = new Hls({});
		hls.loadSource(url);
		hls.attachMedia(videoPlayer);
		hls.on(Hls.Events.MEDIA_ATTACHED, function () {
			videoPlayer.play();
		});
	} else if (videoPlayer.canPlayType("application/vnd.apple.mpegurl")) {
		videoPlayer.src = url;
		videoPlayer.addEventListener("canplay", function () {
			videoPlayer.play();
		});
	}
}
if (vid) {
	const player = document.getElementById("videoPlayer");
	if (!(player instanceof HTMLVideoElement)) {
		throw new Error("videoPlayer is not a video player");
	}
	playHls(`${API_BASE_URL}/m3u8/${vid}`, player);
}
