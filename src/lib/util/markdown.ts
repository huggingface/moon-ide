// Detect markdown buffers by extension. Lives in `util/` (not
// `editor/`) because the toggle UI and state both consult it; the
// extension list stays in lockstep with `editor/language.ts`'s
// `case 'md' | 'markdown'`.
const MARKDOWN_EXTS = new Set(['md', 'markdown', 'mdown']);

export function isMarkdownPath(path: string): boolean {
	const ext = path.split('.').pop()?.toLowerCase() ?? '';
	return MARKDOWN_EXTS.has(ext);
}
