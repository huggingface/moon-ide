import type { Extension } from '@codemirror/state';
import { LRLanguage, LanguageSupport, StreamLanguage } from '@codemirror/language';
import { type Input, type Parser, type SyntaxNode, type SyntaxNodeRef, parseMixed } from '@lezer/common';

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

// `.github/workflows/*.yml` / `*.yaml` — GitHub Actions workflow files.
// The `run:` values in these hold shell, so we overlay the shell grammar
// onto them via `parseMixed` (see `githubActionsYaml`). Matched on the
// full workspace-relative path so a plain `deploy.yml` elsewhere isn't
// pulled into the Actions dialect. The moon-ide repo itself ships
// `.github/workflows/moon-base.yml` — bootstrap, not speculation (ADR 0005).
const WORKFLOW_PATH_RE = /^\.github\/workflows\/[^/]+\.(ya?ml)$/i;

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

// YAML key names whose scalar / block-scalar values we treat as shell
// when the file is a GitHub Actions workflow. `run` is the script step;
// `shell` carries the interpreter, which is shell-ish enough to read
// well with the shell grammar (bash is a strict superset of sh and the
// legacy mode handles both). Keys we don't list (`env`, `with`, …) keep
// their normal YAML highlighting — overlaying them would mostly repaint
// `KEY=VALUE` and `${{ }}` expressions as shell strings, which is a wash
// and loses the YAML-pair structure the user expects.
const RUN_SHELL_KEYS = new Set(['run', 'shell']);

// Overlay the legacy shell stream-parser onto the `run:` / `shell:`
// values of a GitHub Actions workflow YAML document.
//
// Why `parseMixed` + an overlay, not a bespoke tokenizer: the Lezer YAML
// grammar already parses the value's range — `BlockLiteralContent` for
// `|`/`>` block scalars, `Literal` for plain scalars (`run: echo x`).
// We mount the shell parser over that range and let CodeMirror resolve
// the inner tokens. Keys and every other YAML node keep their original
// grammar — `Key`, `Pair`, block-mapping indentation, folding all
// survive untouched.
//
// Key-walk: `BlockLiteralContent` / `Literal` sit under `BlockLiteral`
// → `Pair`. We climb to the enclosing `Pair`, grab its `Key` child, and
// read the key text from `input` (the `Input` arg `parseMixed` threads
// into the nest callback — the `SyntaxNode` itself has no `read`). A
// `Literal` that is itself a key (`Key > Literal`) is skipped: the climb
// stops at the enclosing `Key`, not `Pair`, so it keeps its YAML
// property-name styling.
function isRunShellValue(node: SyntaxNodeRef, input: Input): boolean {
	if (node.name !== 'BlockLiteralContent' && node.name !== 'Literal') {
		return false;
	}
	let pair: SyntaxNode | null = node.node;
	while (pair && pair.name !== 'Pair' && pair.name !== 'Key') {
		pair = pair.parent;
	}
	if (!pair || pair.name === 'Key') {
		return false;
	}
	const keyNode = pair.getChild('Key');
	if (!keyNode) {
		return false;
	}
	return RUN_SHELL_KEYS.has(input.read(keyNode.from, keyNode.to));
}

// The shell parser is built once and reused across every `run:` overlay
// in the document. `StreamLanguage.define(shell).parser` is the raw
// `Parser` the overlay machinery mounts; reusing it avoids building a
// fresh stream-language wrapper per value.
let shellParserCache: Parser | null = null;
async function ensureShellParser(): Promise<Parser> {
	if (shellParserCache) {
		return shellParserCache;
	}
	const { shell } = await import('@codemirror/legacy-modes/mode/shell');
	shellParserCache = StreamLanguage.define(shell).parser;
	return shellParserCache;
}

// A `LanguageSupport` for GitHub Actions workflow YAML: the upstream
// `yamlLanguage` with a `parseMixed` wrapper that overlays the shell
// grammar onto `run:` / `shell:` values. Built lazily — only workflow
// files pay for it. The `yamlLanguage` already carries the fold /
// indent / comment props, so `configure({ wrap })` preserves them.
let githubActionsYamlCache: LanguageSupport | null = null;
async function githubActionsYaml(): Promise<LanguageSupport> {
	if (githubActionsYamlCache) {
		return githubActionsYamlCache;
	}
	const { yamlLanguage } = await import('@codemirror/lang-yaml');
	await ensureShellParser();
	const parser = yamlLanguage.parser.configure({
		wrap: parseMixed((node, input) =>
			isRunShellValue(node, input) ? { parser: shellParserCache!, overlay: [{ from: node.from, to: node.to }] } : null,
		),
	});
	githubActionsYamlCache = new LanguageSupport(
		LRLanguage.define({
			name: 'github-actions',
			parser,
			languageData: {
				commentTokens: { line: '#' },
				indentOnInput: /^\s*[\]}]$/,
			},
		}),
	);
	return githubActionsYamlCache;
}

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
	// GitHub Actions workflow files get a shell-overlay YAML grammar
	// (see `githubActionsYaml`). Checked on the full path so a plain
	// `deploy.yml` elsewhere keeps the standard YAML grammar.
	if (WORKFLOW_PATH_RE.test(filename)) {
		return [await githubActionsYaml()];
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
			const { jsdocExtension } = await import('./jsdoc');
			return [javascript({ typescript: true, jsx: ext === 'tsx' }), jsdocExtension()];
		}
		case 'js':
		case 'mjs':
		case 'cjs':
		case 'jsx': {
			const { javascript } = await import('@codemirror/lang-javascript');
			const { jsdocExtension } = await import('./jsdoc');
			return [javascript({ jsx: ext === 'jsx' }), jsdocExtension()];
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
		case 'htm': {
			const { html } = await import('@codemirror/lang-html');
			return [html()];
		}
		case 'svelte': {
			// Replit's Svelte SFC grammar — composes `lang-html` for
			// the template with `lang-javascript` / `lang-css` for
			// `<script>` / `<style>` blocks, mirroring what
			// `@codemirror/lang-vue` does for Vue. Landed together
			// with the svelte-language-server LSP wiring.
			const { svelte } = await import('@replit/codemirror-lang-svelte');
			return [svelte()];
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
			// `@codemirror/lang-yaml` is the official Lezer grammar.
			// We use it over the legacy `StreamLanguage` mode for the
			// folding it brings: block mappings and sequences carry
			// `foldNodeProp`, so the `foldGutter` gets fold markers
			// for free (the stream mode had no fold info). Highlighting
			// is also more accurate — anchors, tags, flow collections.
			const { yaml } = await import('@codemirror/lang-yaml');
			return [yaml()];
		}
		case 'dockerfile': {
			const { dockerFile } = await import('@codemirror/legacy-modes/mode/dockerfile');
			return [StreamLanguage.define(dockerFile)];
		}
		case 'tf':
		case 'tfvars':
		case 'hcl': {
			// HashiCorp Configuration Language — covers Terraform
			// (`.tf`, `.tfvars`) and standalone HCL configs
			// (`.hcl`, e.g. Packer, Consul, Nomad). The package
			// ships a Lezer grammar ported from `tree-sitter-hcl`,
			// which is the canonical HCL2 grammar HashiCorp's own
			// tooling references — so heredoc / template
			// interpolation / object expressions all parse
			// correctly. Same shape as the other `lang-*`
			// extensions: indentation, brace matching, and the
			// `languageData.foldNodeProp` that hooks our
			// `foldGutter` lands automatically.
			//
			// `.tfstate` is **not** wired up: it's machine-written
			// JSON and the `json` arm above already covers it
			// (the `.json` extension takes precedence anyway, so
			// the question is only what to do for files renamed
			// `*.tfstate`). Terraform never edits state files by
			// hand and the grammar would be wrong.
			const { hcl } = await import('codemirror-lang-hcl');
			return [hcl()];
		}
		default:
			return [];
	}
}

// Exposed for unit tests so they can exercise the workflow detection and
// the shell-overlay tree walk without spinning up a full CodeMirror view.
export const __test = {
	WORKFLOW_PATH_RE,
	RUN_SHELL_KEYS,
	isRunShellValue,
	githubActionsYaml,
};
