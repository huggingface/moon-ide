import { EditorState } from '@codemirror/state';
import { syntaxTree } from '@codemirror/language';
import { describe, expect, it } from 'vitest';

import { __test, languageFor } from './language';

const { WORKFLOW_PATH_RE, githubActionsYaml } = __test;

// Parse the shared Zig sample and return its syntax tree plus the
// sample text, so tests can resolve nodes at given offsets.
async function parseZig() {
	const { zigLanguage } = await import('codemirror-lang-zig');
	const state = EditorState.create({ doc: ZIG_SAMPLE, extensions: [zigLanguage] });
	return { tree: syntaxTree(state), source: ZIG_SAMPLE };
}

// Resolve the `classHighlighter` classes for the innermost node at
// `needle`'s start — i.e. what the editor actually paints. Falls
// back to the node name when the node carries no style tags, so
// assertions read uniformly.
async function zigHighlightAt(needle: string): Promise<string> {
	const { classHighlighter, getStyleTags } = await import('@lezer/highlight');
	const { tree, source } = await parseZig();
	const pos = source.indexOf(needle);
	if (pos < 0) {
		throw new Error(`needle "${needle}" not found in source`);
	}
	const node = tree.resolveInner(pos, 1);
	const styled = getStyleTags(node);
	if (styled === null) {
		return node.name;
	}
	return classHighlighter.style(styled.tags) ?? node.name;
}

const ZIG_SAMPLE = [
	'const std = @import("std");',
	'',
	'/// Doc comment for the entry point.',
	'pub fn main() !void {',
	'    const count: u32 = 42;',
	'    std.debug.print("Hello, {s}!\\n", .{"world"});',
	'}',
].join('\n');

// Build a fresh editor state with the GitHub Actions YAML language
// extension loaded, then parse a sample. Returns the syntax tree so a
// test can resolve nodes at given offsets and inspect the grammar that
// owns them.
async function parseWorkflow(source: string) {
	const support = await githubActionsYaml();
	const state = EditorState.create({ doc: source, extensions: [support] });
	// `syntaxTree` is the (possibly partial) tree; for a small document
	// it's complete synchronously after `create` dispatches.
	return { tree: syntaxTree(state), source };
}

// Resolve the innermost node at `pos` *including* overlay-mounted
// subtrees (`resolveInner` enters them; plain `resolve` stops at the
// host node). Returns the node name so a test can assert, e.g., that a
// `$VAR` inside a `run:` block lands in the shell grammar as
// `variableName.definition` rather than the YAML `Literal`.
function nodeAt(tree: ReturnType<typeof syntaxTree>, source: string, needle: string): string {
	const pos = source.indexOf(needle);
	if (pos < 0) {
		throw new Error(`needle "${needle}" not found in source`);
	}
	const resolved = tree.resolveInner(pos, 1);
	return resolved.name;
}

describe('WORKFLOW_PATH_RE', () => {
	it('matches .github/workflows/*.yml', () => {
		expect(WORKFLOW_PATH_RE.test('.github/workflows/moon-base.yml')).toBe(true);
		expect(WORKFLOW_PATH_RE.test('.github/workflows/deploy.yaml')).toBe(true);
	});

	it('is case-insensitive on the extension', () => {
		expect(WORKFLOW_PATH_RE.test('.github/workflows/ci.YML')).toBe(true);
	});

	it('rejects a plain yaml file outside .github/workflows', () => {
		expect(WORKFLOW_PATH_RE.test('compose.yaml')).toBe(false);
		expect(WORKFLOW_PATH_RE.test('src/deploy.yml')).toBe(false);
	});

	it('rejects files in .github but not under workflows/', () => {
		expect(WORKFLOW_PATH_RE.test('.github/dependabot.yml')).toBe(false);
	});
});

describe('languageFor — github actions workflow', () => {
	it('returns the shell-overlay extension for .github/workflows/*.yml', async () => {
		const exts = await languageFor('.github/workflows/ci.yml');
		// `LanguageSupport` is itself an extension value; we just assert
		// it's present (a non-empty array) rather than peeking at the
		// internal class — the tree-walk test below covers the behaviour.
		expect(exts.length).toBeGreaterThan(0);
	});

	it('returns plain yaml for a non-workflow .yml', async () => {
		const exts = await languageFor('compose.yaml');
		expect(exts.length).toBeGreaterThan(0);
	});
});

describe('githubActionsYaml — shell overlay', () => {
	it('highlights a block-scalar run: value as shell', async () => {
		const source = [
			'jobs:',
			'  build:',
			'    runs-on: ubuntu-latest',
			'    steps:',
			'      - name: Run',
			'        run: |',
			'          set -euo pipefail',
			'          echo "hello"',
			'',
		].join('\n');
		const { tree, source: doc } = await parseWorkflow(source);
		// `set` is a shell keyword; inside the overlay it resolves to
		// `keyword`, not the YAML `Literal` it would be without overlay.
		expect(nodeAt(tree, doc, 'set ')).toBe('keyword');
		// The double-quoted string inside the run block is a shell string.
		expect(nodeAt(tree, doc, '"hello"')).toBe('string');
	});

	it('highlights a plain run: value as shell', async () => {
		const source = ['jobs:', '  build:', '    steps:', '      - name: One-liner', '        run: echo hi', ''].join(
			'\n',
		);
		const { tree, source: doc } = await parseWorkflow(source);
		// `echo` is a shell builtin in the legacy mode's command list.
		// Plain-scalar `run:` values are `Literal` nodes that we overlay
		// just like block scalars.
		expect(nodeAt(tree, doc, 'echo ')).toBe('variableName.standard');
	});

	it('highlights shell: interpreter values as shell', async () => {
		const source = [
			'jobs:',
			'  build:',
			'    steps:',
			'      - name: Step',
			'        shell: bash',
			'        run: echo hi',
			'',
		].join('\n');
		const { tree, source: doc } = await parseWorkflow(source);
		// `bash` is in the shell mode's command list.
		expect(nodeAt(tree, doc, 'bash')).toBe('variableName.standard');
	});

	it('keeps the run key itself as YAML', async () => {
		const source = ['jobs:', '  build:', '    steps:', '      - name: Run', '        run: echo hi', ''].join('\n');
		const { tree, source: doc } = await parseWorkflow(source);
		// The `run` key (a `Key > Literal`) must keep its YAML
		// property-name styling — we never overlay keys.
		expect(nodeAt(tree, doc, 'run:')).toBe('Literal');
	});

	it('does not overlay non-run keys', async () => {
		const source = [
			'jobs:',
			'  build:',
			'    runs-on: ubuntu-latest',
			'    steps:',
			'      - name: Run',
			'        run: echo hi',
			'',
		].join('\n');
		const { tree, source: doc } = await parseWorkflow(source);
		// `runs-on`'s value `ubuntu-latest` stays a YAML `Literal`,
		// not a shell token — only `run` / `shell` get the overlay.
		expect(nodeAt(tree, doc, 'ubuntu-latest')).toBe('Literal');
	});

	it('keeps $VAR interpolation inside run as a shell variable', async () => {
		const source = [
			'jobs:',
			'  build:',
			'    steps:',
			'      - name: Deploy',
			'        env:',
			'          KEY: value',
			'        run: |',
			'          echo "$KEY"',
			'',
		].join('\n');
		const { tree, source: doc } = await parseWorkflow(source);
		// `$KEY` inside the run block resolves via the shell grammar as
		// a `variableName.definition` token, not a YAML `Literal`.
		expect(nodeAt(tree, doc, '$KEY')).toBe('variableName.definition');
	});
});

describe('languageFor — zig', () => {
	it('returns a non-empty extension set for .zig files', async () => {
		const exts = await languageFor('src/main.zig');
		expect(exts.length).toBeGreaterThan(0);
	});

	it('does not claim zig-adjacent extensions', async () => {
		// `.zig`'s neighbour extensions stay unmatched — only the
		// exact extension maps.
		const exts = await languageFor('build.zig.zon');
		// `build.zig.zon` ends in `.zon` (Zig's package-lock
		// format), which has no grammar — plain text is fine.
		expect(exts).toEqual([]);
	});
});

describe('zig grammar highlighting', () => {
	it('paints keywords, strings, and comments through the standard tag set', async () => {
		// `pub` / `fn` map to `keyword`, string literals to
		// `string`, `///` doc comments to `comment` — the exact
		// classes our `--m-syntax-*` CSS targets.
		expect(await zigHighlightAt('pub fn')).toBe('tok-keyword');
		expect(await zigHighlightAt('"Hello')).toContain('tok-string');
		expect(await zigHighlightAt('/// Doc')).toContain('tok-comment');
	});

	it('paints builtin identifiers and number literals distinctly', async () => {
		// `@import` is a builtin (variableName per the grammar's
		// styleTags); `42` a number — `tags.integer` subclasses
		// `tags.number` and `classHighlighter` only names the
		// base, hence `tok-number`.
		expect(await zigHighlightAt('@import(')).toContain('tok-variableName');
		expect(await zigHighlightAt('42')).toContain('tok-number');
	});
});
