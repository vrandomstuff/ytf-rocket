export const API_BASE_URL = `${window.location.origin}/api`

interface Video {
    id: string,
    thumbnail: string,
    channel: string,
    channelAvatar: string,
    channelName: string,
    title: string
}

interface Channel {
    id: string,
    name: string,
    avatar: string | null,
    description: string | null,
    banner: string | null
}

export async function getChannelMetadata(channelId: string): Promise<Channel> {
	const response = await fetch(`${API_BASE_URL}/getChannelMetadata`)
	return {} as Channel
}
