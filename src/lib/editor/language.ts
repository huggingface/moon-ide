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

// `.env`, `.env.local`, `.env.production`, `.env.example`, etc. —
// dotenv-flavoured files. Bare `env` (no leading dot) is intentionally
// not matched; it's a Unix command, not a config convention. The
// per-line tokenizer below handles `KEY=VALUE`, `# comment`,
// `export `, and `${VAR}` interpolation inside double-quoted values.
const ENV_FILENAME_RE = /^\.env(?:\..+)?$/;

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

// Dotenv mode. Per-line, with a tiny bit of state to remember
// whether we've crossed the `=` on the current line — everything
// after the `=` is the value, everything before is the (optional)
// `export ` keyword and the key. State resets at start-of-line so
// continuations across lines aren't supported (dotenv conventions
// vary on this; almost nobody uses them, and supporting them
// would require deciding which dialect to follow).
//
// Token mapping mirrors the Lezer tag dictionary the editor theme
// is wired against, so the colours come out matching the rest of
// the IDE:
//   - `comment`        → `#` lines and trailing `# …` after a value
//   - `keyword`        → `export ` prefix
//   - `propertyName`   → the key on the left of `=`
//   - `operator`       → the `=` itself
//   - `string`         → unquoted, single-quoted, double-quoted value
//   - `variableName`   → `${VAR}` interpolation inside double-quoted
//                        (single-quoted values are taken literally,
//                        same as POSIX shell semantics)
const envLanguage = StreamLanguage.define<{ inValue: boolean }>({
	name: 'dotenv',
	startState() {
		return { inValue: false };
	},
	token(stream, state) {
		if (stream.sol()) {
			state.inValue = false;
			if (stream.eatSpace()) {
				return null;
			}
			if (stream.peek() === '#') {
				stream.skipToEnd();
				return 'comment';
			}
			if (stream.match(/^export\b/)) {
				return 'keyword';
			}
			if (stream.match(/^[A-Za-z_][A-Za-z0-9_]*/)) {
				return 'propertyName';
			}
		}
		if (!state.inValue) {
			if (stream.eatSpace()) {
				return null;
			}
			if (stream.eat('=')) {
				state.inValue = true;
				return 'operator';
			}
			// Stray content before `=` (e.g. `KEY .` typo). Walk on
			// rather than busy-loop the tokenizer.
			stream.next();
			return null;
		}
		// Past the `=` — value region until EOL.
		if (stream.peek() === '#') {
			stream.skipToEnd();
			return 'comment';
		}
		if (stream.eat('"')) {
			while (!stream.eol()) {
				const ch = stream.next();
				if (ch === '\\') {
					stream.next();
					continue;
				}
				if (ch === '$' && stream.peek() === '{') {
					// Back up so the `${…}` interpolation gets its
					// own token on the next dispatch.
					stream.backUp(1);
					return 'string';
				}
				if (ch === '"') {
					return 'string';
				}
			}
			return 'string';
		}
		if (stream.eat("'")) {
			while (!stream.eol()) {
				if (stream.next() === "'") {
					return 'string';
				}
			}
			return 'string';
		}
		if (stream.match(/^\$\{[^}]*\}/)) {
			return 'variableName';
		}
		if (stream.match(/^\$[A-Za-z_][A-Za-z0-9_]*/)) {
			return 'variableName';
		}
		if (stream.eatSpace()) {
			return null;
		}
		// Unquoted value run: read until whitespace, `#`, or `$`
		// (interpolation starts), then yield the chunk as a string.
		stream.eatWhile((c: string) => c !== ' ' && c !== '\t' && c !== '#' && c !== '$' && c !== '"' && c !== "'");
		return 'string';
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
	if (ENV_FILENAME_RE.test(baseName)) {
		return [envLanguage];
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
		case 'jsonl':
		case 'ndjson': {
			// Newline-delimited JSON. The `lang-json` Lezer grammar
			// is single-value at the top, so multiple objects across
			// lines parse as one big error and most highlighting
			// drops out. The legacy stream-mode tokenizer is
			// per-line, which is exactly what JSONL wants — each
			// line is independently tokenized as a JSON value, the
			// next line starts fresh, no cross-line state to break.
			// Coder session traces are the canonical use case.
			const { json } = await import('@codemirror/legacy-modes/mode/javascript');
			return [StreamLanguage.define(json)];
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
		case 'vue': {
			// `@codemirror/lang-vue` is the official upstream Vue SFC
			// grammar (Marijn's, same author as core CodeMirror). It
			// composes on top of `lang-html` for the template region
			// and `lang-javascript` for `<script>` blocks, which is
			// what you'd hand-roll anyway. Pulled in for the team's
			// Vue projects; no LSP wiring yet (Volar is the obvious
			// next step but isn't on the roadmap until there's a real
			// ask for it).
			const { vue } = await import('@codemirror/lang-vue');
			return [vue()];
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
		case 'go': {
			// `@codemirror/lang-go` is the official upstream — it
			// ships a Lezer grammar tracked against the latest Go
			// spec (generics, type params, range-over-func), so we
			// match the editor highlighter for `.go` to whatever
			// Go's own spec says is current. No extra arg needed:
			// the package's default export wires up indentation +
			// auto-close-brackets the same way the Rust / Python
			// extensions do.
			const { go } = await import('@codemirror/lang-go');
			return [go()];
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
