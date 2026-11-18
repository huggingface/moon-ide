// The file-kind icons of the file tree, for anywhere else a file is
// listed (pull request files, diff headers), and the colours a git
// status gives a file's name. Both follow the tree so a file reads
// the same wherever it shows up.

import { createFileTreeIconResolver, getBuiltInFileIconColor, getBuiltInSpriteSheet } from '@pierre/trees';
import type { GitFileStatus } from './protocol';

// The tree's own defaults (it is built without an `icons` option).
const ICON_SET = 'complete';
const resolver = createFileTreeIconResolver({ set: ICON_SET, colored: true });

export type FileIconRef = {
	/** Id of the icon's symbol in the sprite sheet. */
	symbol: string;
	/** CSS colour of the file kind; `null` for a kind with none. */
	color: string | null;
};

export function fileIconFor(path: string): FileIconRef {
	const icon = resolver.resolveIcon('file-tree-icon-file', path);
	return {
		symbol: icon.name,
		color: icon.token === undefined ? null : (getBuiltInFileIconColor(icon.token) ?? null),
	};
}

let spriteMounted = false;

/** Put the icons' sprite sheet in the document, once. The tree keeps
 *  its own copy inside its shadow root, out of reach from here. */
export function mountFileIconSprite(): void {
	if (spriteMounted || typeof document === 'undefined') {
		return;
	}
	spriteMounted = true;
	const holder = document.createElement('div');
	holder.setAttribute('aria-hidden', 'true');
	holder.style.cssText = 'position:absolute;width:0;height:0;overflow:hidden';
	holder.innerHTML = getBuiltInSpriteSheet(ICON_SET);
	document.body.append(holder);
}

const STATUS_COLORS: Record<GitFileStatus, string | undefined> = {
	added: 'var(--m-git-added)',
	untracked: 'var(--m-git-added)',
	modified: 'var(--m-git-modified)',
	deleted: 'var(--m-git-deleted)',
	conflicted: 'var(--m-warning)',
	ignored: undefined,
};

/** The colour a file's name takes for its git status, as the file
 *  tree colours it. `undefined` leaves the name as it is. */
export function statusColor(status: GitFileStatus): string | undefined {
	return STATUS_COLORS[status];
}

const STATUS_WORDS: Record<GitFileStatus, string | null> = {
	added: 'new',
	untracked: 'untracked',
	deleted: 'deleted',
	conflicted: 'conflict',
	modified: null,
	ignored: null,
};

/** A word for the statuses that aren't the ordinary "modified", to go
 *  beside a file's name. `null` when there is nothing to say. */
export function statusWord(status: GitFileStatus): string | null {
	return STATUS_WORDS[status];
}
