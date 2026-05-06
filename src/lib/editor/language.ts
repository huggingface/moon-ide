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
	Dockerfile: 'dockerfile',
	Containerfile: 'dockerfile',
};

// `Dockerfile.dev`, `Dockerfile.prod`, `app.Dockerfile`, etc. all map
// to the same grammar. We treat them as Dockerfiles whenever
// `Dockerfile` appears as either the leading segment or the trailing
// segment of the dotted name.
const DOCKERFILE_VARIANT_RE = /(?:^Dockerfile\.|\.Dockerfile$)/;

// `.gitignore`, `.dockerignore`, `.prettierignore`, `.eslintignore`,
// `.npmignore`, etc. — anything matching `.<word>ignore` is treated as
// a gitignore-flavored file. They all share the "patterns + `#` line
// comments" syntax, so highlighting comments is the bare minimum we
// owe the user (the rest is plain text — pattern characters like
// `*` / `!` / `/` aren't meaningfully colorable without a proper
// grammar, and that's not worth shipping today).
const IGNORE_FILENAME_RE = /^\.[\w-]*ignore$/;

// Lazy ignore-file mode. CodeMirror's `StreamLanguage` is a thin
// per-line tokenizer; for ignore files we only need one rule: a `#`
// at start-of-line opens a comment that runs to end-of-line. `!` at
// start-of-line is *not* a comment (it's a gitignore negation marker)
// — we leave it plain.
const ignoreLanguage = StreamLanguage.define({
	name: 'gitignore',
	token(stream) {
		if (stream.sol() && stream.peek() === '#') {
			stream.skipToEnd();
			return 'comment';
		}
		stream.skipToEnd();
		return null;
	},
	languageData: {
		commentTokens: { line: '#' },
	},
});

// Match a `#!` interpreter line and capture the basename of the
// interpreter (the last path segment, ignoring `env <prog>` and any
// trailing arguments / flags). Used as a last-resort signal for files
// that lack a useful extension — `.husky/pre-commit` being the
// canonical example: it's shell content with no `.sh` to go on.
const SHEBANG_RE = /^#!\s*(?:\S+\/)?(?:env\s+)?([\w.-]+)/;
const SHEBANG_LANGUAGES: Record<string, string> = {
	sh: 'sh',
	bash: 'sh',
	zsh: 'sh',
	dash: 'sh',
	ash: 'sh',
};

// Language extensions are loaded lazily so we don't bundle every grammar
// up front. Returning [] means "no syntax extension yet, plain text is fine".
//
// `firstLine` is consulted only when filename + extension don't match
// anything; it lets us pick up shebang scripts that lack an extension.
export async function languageFor(filename: string, firstLine?: string): Promise<Extension[]> {
	const baseName = filename.split('/').pop() ?? filename;
	if (IGNORE_FILENAME_RE.test(baseName)) {
		return [ignoreLanguage];
	}
	let ext = FILENAME_LANGUAGES[baseName] ?? baseName.split('.').pop()?.toLowerCase() ?? '';
	if (DOCKERFILE_VARIANT_RE.test(baseName)) {
		ext = 'dockerfile';
	}
	// Fall back to shebang sniffing only when the basename has no `.`
	// in it — i.e. there was no extension to consult. A file called
	// `script.txt` that happens to start with `#!/bin/sh` is still a
	// `.txt` file by the user's intent.
	if (firstLine && !baseName.includes('.')) {
		const match = SHEBANG_RE.exec(firstLine);
		const interp = match?.[1]?.toLowerCase();
		if (interp && SHEBANG_LANGUAGES[interp]) {
			ext = SHEBANG_LANGUAGES[interp];
		}
	}

	switch (ext) {
		case 'ts':
		case 'mts':
		case 'cts':
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
		case 'py':
		case 'pyi': {
			const { python } = await import('@codemirror/lang-python');
			return [python()];
		}
		case 'toml': {
			const { toml } = await import('@codemirror/legacy-modes/mode/toml');
			return [StreamLanguage.define(toml)];
		}
		case 'properties': {
			const { properties } = await import('@codemirror/legacy-modes/mode/properties');
			return [StreamLanguage.define(properties)];
		}
		case 'sh':
		case 'bash':
		case 'zsh': {
			const { shell } = await import('@codemirror/legacy-modes/mode/shell');
			return [StreamLanguage.define(shell)];
		}
		case 'yaml':
		case 'yml': {
			const { yaml } = await import('@codemirror/legacy-modes/mode/yaml');
			return [StreamLanguage.define(yaml)];
		}
		case 'dockerfile': {
			const { dockerFile } = await import('@codemirror/legacy-modes/mode/dockerfile');
			return [StreamLanguage.define(dockerFile)];
		}
		default:
			return [];
	}
}
