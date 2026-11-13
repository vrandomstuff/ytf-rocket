type AlpineType = typeof import("alpinejs");

const AlpineInstance = window.Alpine as AlpineType;

import { ytSearch } from "./api.js";
import type { Video } from "./api.js";

export async function populateSearchResults(query: string, page: number) {
    const videos = await ytSearch(query, page);

    const container = document.getElementById("searchResults");
    const template = document.getElementById("VideoElement");

    if (!(container instanceof HTMLDivElement)) {
        throw new Error("Search results container not found");
    }

    if (!(template instanceof HTMLTemplateElement)) {
        throw new Error("Video template not found");
    }

    const fragment = document.createDocumentFragment();

	for (const video of videos) {
		const clone = template.content.cloneNode(true) as DocumentFragment;
		const element = clone.firstElementChild as HTMLElement;
		
		AlpineInstance.initTree(element);
		interface SearchData {
			videoInfo?: Video;
		}

		(AlpineInstance.$data(element) as SearchData).videoInfo = video;
		
		fragment.appendChild(element);
	}
	
	container.appendChild(fragment);
}

const params = new URLSearchParams(window.location.search);

const query = params.get("q") ?? "";
const pageParam = Number(params.get("page"));
const page = Number.isInteger(pageParam) && pageParam > 0 ? pageParam : 1;

if (query) {
    populateSearchResults(query, page);
}
