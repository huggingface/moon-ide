// Maps a path or filename to the renderer that should display it.
// Today we only branch text vs image; extend as the IDE grows the ability
// to display new file types (PDF, video, etc. — when there's a real need).

export type FileKind = 'text' | 'image';

const IMAGE_EXTS = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'ico', 'avif']);

export function fileKindFor(path: string): FileKind {
	const ext = path.split('.').pop()?.toLowerCase() ?? '';
	if (IMAGE_EXTS.has(ext)) {
		return 'image';
	}
	return 'text';
}
