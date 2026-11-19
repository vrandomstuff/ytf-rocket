export const API_BASE_URL = `${window.location.origin}/api/v2`;

export interface Video {
	id: string;
	thumbnail: string;
	channel: string;
	channelName: string;
	title: string;
}

export interface Channel {
	id: string;
	name: string;
	avatar: string | null;
	description: string | null;
	banner: string | null;
}

export async function getChannelMetadata(channelId: string): Promise<Channel> {
	const response = await fetch(
		`${API_BASE_URL}/getChannelMetadata?channelId=${encodeURIComponent(channelId)}`
	);

	if (!response.ok) {
		throw new Error(
			`getChannelMetadata response is not ok code: ${response.status} ${response.statusText}`
		);
	}

	return (await response.json()) as Channel;
}

export async function ytSearch(query: string, page: number): Promise<Video[]> {
	const response = await fetch(
		`${API_BASE_URL}/search?q=${encodeURIComponent(query)}&page=${page}`
	);

	if (!response.ok) {
		throw new Error(
			`search response is not ok code: ${response.status} ${response.statusText}`
		);
	}

	return (await response.json()) as Video[];
}
