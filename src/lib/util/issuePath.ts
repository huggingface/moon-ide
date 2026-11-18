// Path of an issue's pseudo-tab (ADR 0117): `issue://<number>`, an
// issue of the repository behind the folder whose tab strip holds it.
// Same shape as `prPath.ts`.

import { prTabName } from './prPath';

const PREFIX = 'issue://';

export function issuePath(number: number): string {
	return `${PREFIX}${number}`;
}

export function isIssuePath(path: string): boolean {
	return path.startsWith(PREFIX);
}

export function numberFromIssuePath(path: string): number | null {
	if (!path.startsWith(PREFIX)) {
		return null;
	}
	const digits = path.slice(PREFIX.length);
	return /^[1-9][0-9]*$/.test(digits) ? Number(digits) : null;
}

/** `#<number> <title>`, bounded like a pull request's tab name. */
export const issueTabName = prTabName;
