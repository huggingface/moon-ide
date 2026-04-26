import type { Extension } from '@codemirror/state';
import { StreamLanguage } from '@codemirror/language';

// Some files don't carry a useful extension and need to be matched by name.
// We add an entry whenever moon-ide's own source tree contains such a file
// (it counts as bootstrap, see ADR 0005), or when the team flags a real
// repo that needs it. Speculative additions stay out.
const FILENAME_LANGUAGES: Record<string, string> = {
	'Cargo.lock': 'toml',
	'bun.lock': 'json',
	'.editorconfig': 'properties',
	'.npmrc': 'properties',
};

// Language extensions are loaded lazily so we don't bundle every grammar
// up front. Returning [] means "no syntax extension yet, plain text is fine".
export async function languageFor(filename: string): Promise<Extension[]> {
	const baseName = filename.split('/').pop() ?? filename;
	const ext = FILENAME_LANGUAGES[baseName] ?? baseName.split('.').pop()?.toLowerCase() ?? '';

	switch (ext) {
		case 'ts':
		case 'tsx': {
			const { javascript } = await import('@codemirror/lang-javascript');
			return [javascript({ typescript: true, jsx: ext === 'tsx' })];
		}
		case 'js':
		case 'mjs':
		case 'cjs':
		case 'jsx': {
			const { javascript } = await import('@codemirror/lang-javascript');
			return [javascript({ jsx: ext === 'jsx' })];
		}
		case 'json':
		case 'jsonc': {
			const { json } = await import('@codemirror/lang-json');
			return [json()];
		}
		case 'css':
		case 'scss':
		case 'less': {
			const { css } = await import('@codemirror/lang-css');
			return [css()];
		}
		case 'html':
		case 'htm':
		case 'svelte': {
			// Svelte is rendered with the HTML grammar for now; a real Svelte
			// grammar lands when we wire svelte-language-server in Phase 4.
			const { html } = await import('@codemirror/lang-html');
			return [html()];
		}
		case 'md':
		case 'markdown': {
			const { markdown } = await import('@codemirror/lang-markdown');
			return [markdown()];
		}
		case 'rs': {
			const { rust } = await import('@codemirror/lang-rust');
			return [rust()];
		}
		case 'toml': {
			const { toml } = await import('@codemirror/legacy-modes/mode/toml');
			return [StreamLanguage.define(toml)];
		}
		case 'properties': {
			const { properties } = await import('@codemirror/legacy-modes/mode/properties');
			return [StreamLanguage.define(properties)];
		}
		default:
			return [];
	}
}
