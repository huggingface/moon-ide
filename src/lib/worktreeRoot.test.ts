import { describe, expect, it } from 'vitest';

import type { WorkspaceFolder } from './protocol';
import { worktreeRootPath } from './worktreeRoot';

function folder(path: string, parentPath?: string): WorkspaceFolder {
	return {
		path,
		name: path.split('/').pop() ?? path,
		host: 'local',
		origin: parentPath === undefined ? { kind: 'user_picked' } : { kind: 'worktree', parentPath, branch: 'b' },
	};
}

describe('worktreeRootPath', () => {
	const folders = [
		folder('/p'),
		folder('/p/.worktrees/a', '/p'),
		folder('/p/.worktrees/a/.worktrees/b', '/p/.worktrees/a'),
		folder('/orphan/.worktrees/c', '/orphan'),
	];

	it('returns a project folder as is', () => {
		expect(worktreeRootPath('/p', folders)).toBe('/p');
	});

	it('walks a nested worktree up to its project', () => {
		expect(worktreeRootPath('/p/.worktrees/a', folders)).toBe('/p');
		expect(worktreeRootPath('/p/.worktrees/a/.worktrees/b', folders)).toBe('/p');
	});

	it('stops at a worktree whose parent is not bound', () => {
		expect(worktreeRootPath('/orphan/.worktrees/c', folders)).toBe('/orphan/.worktrees/c');
	});
});
