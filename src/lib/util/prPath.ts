// Synthetic path scheme for a pull request opened without a checkout.
// One tab per pull request, keyed on `pr://<number>`; the number is
// all `PrView` needs, since it reads the pull request from the active
// folder's repository. Same can't-collide-with-a-real-path trick as
// `commit://`.

const PREFIX = 'pr://';

/** Tab titles are `#<number> <title>`; longer titles are cut here. */
const MAX_TITLE = 48;

export function prPath(number: number): string {
	return `${PREFIX}${number}`;
}

export function isPrPath(path: string): boolean {
	return path.startsWith(PREFIX);
}

/** The pull request number of a `pr://<number>` path; `null` for
 * anything else, which callers treat as "not a pull request tab". */
export function numberFromPrPath(path: string): number | null {
	if (!path.startsWith(PREFIX)) {
		return null;
	}
	const digits = path.slice(PREFIX.length);
	return /^[1-9][0-9]*$/.test(digits) ? Number(digits) : null;
}

export function prTabName(number: number, title: string): string {
	const trimmed = title.trim();
	if (trimmed === '') {
		return `#${number}`;
	}
	const shown = trimmed.length > MAX_TITLE ? `${trimmed.slice(0, MAX_TITLE - 1).trimEnd()}…` : trimmed;
	return `#${number} ${shown}`;
}
