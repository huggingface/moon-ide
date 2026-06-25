import { describe, expect, it } from 'vitest';

import type { Workspace, WorkspaceFolder } from './protocol';
import { parseFileLinks, resolveTerminalLink } from './terminalLinks';

function folder(path: string): WorkspaceFolder {
	const slash = path.lastIndexOf('/');
	const name = slash === -1 ? path : path.slice(slash + 1);
	return { path, name, host: 'local', origin: { kind: 'user_picked' } };
}

function ws(folders: WorkspaceFolder[]): Workspace {
	return { id: 'test', folders, active_folder: folders[0]?.path ?? null };
}

describe('parseFileLinks', () => {
	it('matches a file:// URI with line and column', () => {
		const text = 'Error in file:///workspace/repo/apps/api/server.ts:22:3 thrown';
		const matches = parseFileLinks(text);
		expect(matches).toHaveLength(1);
		expect(matches[0]).toMatchObject({
			rawPath: '/workspace/repo/apps/api/server.ts',
			line: 22,
			col: 3,
		});
		expect(text.slice(matches[0]!.start, matches[0]!.end)).toBe('file:///workspace/repo/apps/api/server.ts:22:3');
	});

	it('matches a bare absolute host path with line only', () => {
		const text = 'see /home/me/code/repo/x.ts:10 for details';
		const matches = parseFileLinks(text);
		expect(matches).toHaveLength(1);
		expect(matches[0]).toMatchObject({
			rawPath: '/home/me/code/repo/x.ts',
			line: 10,
			col: null,
		});
	});

	it('matches a path inside parens and stops at the closing paren', () => {
		const text = 'at Foo.bar (/home/me/code/repo/src/x.ts:22:3)';
		const matches = parseFileLinks(text);
		expect(matches).toHaveLength(1);
		expect(matches[0]).toMatchObject({
			rawPath: '/home/me/code/repo/src/x.ts',
			line: 22,
			col: 3,
		});
		expect(text.slice(matches[0]!.start, matches[0]!.end)).toBe('/home/me/code/repo/src/x.ts:22:3');
	});

	it('strips trailing sentence punctuation from extension-only matches', () => {
		const text = 'open /tmp/log.txt.';
		const matches = parseFileLinks(text);
		expect(matches).toHaveLength(1);
		expect(matches[0]!.rawPath).toBe('/tmp/log.txt');
	});

	it('skips bare directory mentions without a line tail or extension', () => {
		expect(parseFileLinks('cwd is /home/me/code')).toEqual([]);
		expect(parseFileLinks('/etc and /var/log are mentioned')).toEqual([]);
	});

	it('matches extensionless source-shaped names with a line tail', () => {
		const text = '/home/me/repo/Makefile:42 has the rule';
		const matches = parseFileLinks(text);
		expect(matches).toHaveLength(1);
		expect(matches[0]).toMatchObject({ rawPath: '/home/me/repo/Makefile', line: 42 });
	});

	it('matches extensionless source-shaped names without a line tail', () => {
		const text = 'edit /home/me/repo/Dockerfile next';
		const matches = parseFileLinks(text);
		expect(matches).toHaveLength(1);
		expect(matches[0]!.rawPath).toBe('/home/me/repo/Dockerfile');
	});

	it('does not pick up relative paths', () => {
		const text = 'src/lib/foo.ts:22:3 should be skipped';
		expect(parseFileLinks(text)).toEqual([]);
	});

	it('finds multiple matches in one row', () => {
		const text = 'a /a/b.ts:1 and b /c/d.rs:2:5 same line';
		const matches = parseFileLinks(text);
		expect(matches).toHaveLength(2);
		expect(matches[0]!.rawPath).toBe('/a/b.ts');
		expect(matches[1]!.rawPath).toBe('/c/d.rs');
	});

	it('rejects paths whose final segment looks like a long hash', () => {
		// Common in `git status` output: `/home/me/repo/.git/objects/ab/c0123abcdef…`
		// `c0123abcdef` looks extension-shaped but is too long to
		// be a real extension.
		const text = '/home/me/repo/.git/objects/ab/c0123abcdef0123abcdef';
		expect(parseFileLinks(text)).toEqual([]);
	});
});

describe('resolveTerminalLink', () => {
	const HOST_FOLDER = folder('/home/me/code/repo');
	const SECOND_FOLDER = folder('/home/me/code/other-repo');
	const NESTED_FOLDER = folder('/home/me/code/repo/sub');

	it('resolves a host absolute path against the longest-matching bound folder', () => {
		const parsed = parseFileLinks('/home/me/code/repo/sub/x.ts:5:1')[0]!;
		const r = resolveTerminalLink(parsed, ws([HOST_FOLDER, NESTED_FOLDER]));
		expect(r).toEqual({
			folder: NESTED_FOLDER.path,
			path: 'x.ts',
			line: 4,
			character: 0,
		});
	});

	it('resolves a container /workspace/<basename>/ path via basename match', () => {
		const parsed = parseFileLinks('file:///workspace/repo/apps/api/server.ts:22:3')[0]!;
		const r = resolveTerminalLink(parsed, ws([HOST_FOLDER, SECOND_FOLDER]));
		expect(r).toEqual({
			folder: HOST_FOLDER.path,
			path: 'apps/api/server.ts',
			line: 21,
			character: 2,
		});
	});

	it('returns null when nothing matches', () => {
		const parsed = parseFileLinks('/var/log/syslog:42')[0]!;
		const r = resolveTerminalLink(parsed, ws([HOST_FOLDER]));
		expect(r).toBeNull();
	});

	it('decrements 1-based line/col into 0-based for jumpTo', () => {
		const parsed = parseFileLinks('/home/me/code/repo/x.ts:1:1')[0]!;
		const r = resolveTerminalLink(parsed, ws([HOST_FOLDER]));
		expect(r).toEqual({ folder: HOST_FOLDER.path, path: 'x.ts', line: 0, character: 0 });
	});

	it('clamps negative-after-decrement line/col to 0', () => {
		// `:0` is rare but defensive — a 1-based emitter shouldn't
		// produce it, but we shouldn't trip over a misbehaving one.
		const parsed = parseFileLinks('/home/me/code/repo/x.ts:0')[0]!;
		const r = resolveTerminalLink(parsed, ws([HOST_FOLDER]));
		expect(r).toEqual({ folder: HOST_FOLDER.path, path: 'x.ts', line: 0, character: 0 });
	});
});
