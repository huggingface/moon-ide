// Path of an issue's pseudo-tab (ADR 0117): `issue://<folder>#<number>`,
// an issue of the repository behind the bound project folder
// `<folder>`. Same shape as `prPath.ts`.

import { githubTabPath, parseGithubTabPath, prTabName, type GithubTabRef } from './prPath';

const PREFIX = 'issue://';

export function issuePath(folder: string, number: number): string {
	return githubTabPath(PREFIX, folder, number);
}

export function isIssuePath(path: string): boolean {
	return path.startsWith(PREFIX);
}

export function parseIssuePath(path: string): GithubTabRef | null {
	return parseGithubTabPath(PREFIX, path);
}

/** `#<number> <title>`, bounded like a pull request's tab name. */
export const issueTabName = prTabName;
