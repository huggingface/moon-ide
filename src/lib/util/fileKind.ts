// Maps a path or filename to the renderer that should display it.
// Today we branch text / image / pdf / video; extend as the IDE grows
// the ability to display new file types — when there's a real need.

export type FileKind = 'text' | 'image' | 'pdf' | 'video';

/** The binary kinds that open as read-only preview buffers. */
export type PreviewKind = Exclude<FileKind, 'text'>;

const IMAGE_EXTS = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'ico', 'avif']);
// Only containers the webview can actually decode (WKWebView / WebKitGTK).
// Keep in step with `preview_mime` in `src-tauri/src/commands/fs.rs`.
const VIDEO_EXTS = new Set(['mp4', 'm4v', 'webm', 'mov']);

export function fileKindFor(path: string): FileKind {
	const ext = path.split('.').pop()?.toLowerCase() ?? '';
	if (IMAGE_EXTS.has(ext)) {
		return 'image';
	}
	if (VIDEO_EXTS.has(ext)) {
		return 'video';
	}
	if (ext === 'pdf') {
		return 'pdf';
	}
	return 'text';
}

export function isPreviewKind(kind: FileKind): kind is PreviewKind {
	return kind !== 'text';
}
