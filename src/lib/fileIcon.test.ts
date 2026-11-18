import { describe, expect, it } from 'vitest';
import { fileIconFor, statusColor, statusWord } from './fileIcon';

describe('fileIconFor', () => {
	it('picks the icon of the file kind, with its colour', () => {
		expect(fileIconFor('crates/moon-core/src/host.rs').symbol).toBe('file-tree-builtin-rust');
		expect(fileIconFor('src/lib/prFiles.test.ts').symbol).toBe('file-tree-builtin-typescript');
		expect(fileIconFor('src/lib/components/PrView.svelte').symbol).toBe('file-tree-builtin-svelte');
		expect(fileIconFor('specs/frontend.md').color).toContain('markdown');
	});

	it('falls back to the plain file icon', () => {
		expect(fileIconFor('Makefile').symbol).toBe('file-tree-builtin-default');
	});
});

describe('git status', () => {
	it('colours every status but ignored', () => {
		expect(statusColor('added')).toBe('var(--m-git-added)');
		expect(statusColor('untracked')).toBe(statusColor('added'));
		expect(statusColor('modified')).toBe('var(--m-git-modified)');
		expect(statusColor('deleted')).toBe('var(--m-git-deleted)');
		expect(statusColor('ignored')).toBeUndefined();
	});

	it('names only what is out of the ordinary', () => {
		expect(statusWord('modified')).toBeNull();
		expect(statusWord('added')).toBe('new');
		expect(statusWord('deleted')).toBe('deleted');
		expect(statusWord('conflicted')).toBe('conflict');
	});
});
