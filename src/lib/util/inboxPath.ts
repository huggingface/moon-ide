// The GitHub inbox pseudo-tab (ADR 0112). One fixed path: the inbox is
// the same list whichever folder's tab strip it sits in.

export const INBOX_PATH = 'inbox://github';

export function isInboxPath(path: string): boolean {
	return path === INBOX_PATH;
}
