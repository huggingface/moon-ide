// Synthetic path scheme for a pull request opened without a checkout:
// `pr://<folder>#<number>`, where `<folder>` is the bound project
// folder whose repository the pull request belongs to. The folder is
// in the path because the tab is shown in every folder's strip
// (ADR 0121), so "the active folder" says nothing about it. Same
// can't-collide-with-a-real-path trick as `commit://`.

const PREFIX = 'pr://';

/** Tab titles are `#<number> <title>`; longer titles are cut here. */
const MAX_TITLE = 48;

/** A pull request or issue tab's subject: the project folder to read
 *  it through, and its number there. */
export type GithubTabRef = { folder: string; number: number };

/** `<prefix><folder>#<number>`. */
export function githubTabPath(prefix: string, folder: string, number: number): string {
	return `${prefix}${folder}#${number}`;
}

/** Inverse of `githubTabPath`; `null` for anything else. The number
 *  follows the last `#`, so a folder with one in its name still
 *  parses. */
export function parseGithubTabPath(prefix: string, path: string): GithubTabRef | null {
	if (!path.startsWith(prefix)) {
		return null;
	}
	const cut = path.lastIndexOf('#');
	if (cut <= prefix.length) {
		return null;
	}
	const digits = path.slice(cut + 1);
	if (!/^[1-9][0-9]*$/.test(digits)) {
		return null;
	}
	return { folder: path.slice(prefix.length, cut), number: Number(digits) };
}

export function prPath(folder: string, number: number): string {
	return githubTabPath(PREFIX, folder, number);
}

export function isPrPath(path: string): boolean {
	return path.startsWith(PREFIX);
}

export function parsePrPath(path: string): GithubTabRef | null {
	return parseGithubTabPath(PREFIX, path);
}

/** `#<number> <title>`, or just the number when the title isn't known
 *  (a tab reopened from navigation history). */
export function prTabName(number: number, title: string): string {
	const trimmed = title.trim();
	if (trimmed === '') {
		return `#${number}`;
	}
	const shown = trimmed.length > MAX_TITLE ? `${trimmed.slice(0, MAX_TITLE - 1).trimEnd()}…` : trimmed;
	return `#${number} ${shown}`;
}

/** A `#<number> <title>` tab name in its two parts, for showing the
 *  number apart; `null` for a name of another shape. */
export function splitTabName(name: string): { ref: string; title: string } | null {
	const match = /^(#\d+)(?: ([\s\S]*))?$/.exec(name);
	if (match === null) {
		return null;
	}
	return { ref: match[1] ?? '', title: match[2] ?? '' };
}
