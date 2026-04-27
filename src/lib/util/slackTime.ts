// Tiny formatter for Slack message timestamps.
//
// Slack `ts` strings look like `"1700000001.000100"` — seconds since
// epoch with a microsecond suffix. The suffix is a per-channel sequence
// number, not real microseconds, but parsing the float gets us close
// enough for "2 min ago" rendering.
//
// Kept here (not in `markdown.ts`) because it's Slack-specific and the
// chat panel is the only caller; if a second caller ever shows up,
// promote this to a generic `relativeTime.ts` then.

const MINUTE = 60;
const HOUR = MINUTE * 60;
const DAY = HOUR * 24;
const WEEK = DAY * 7;

/**
 * Convert a Slack `ts` to a Date. Returns `null` for malformed input
 * — every caller in the panel renders "—" rather than crashing.
 */
export function parseSlackTs(ts: string): Date | null {
	const seconds = Number.parseFloat(ts);
	if (!Number.isFinite(seconds)) {
		return null;
	}
	return new Date(seconds * 1000);
}

/**
 * Short relative time like "2 min" or "yesterday" suitable for a
 * compact chat row. Switches to an absolute date once we're past a
 * week — relative gets imprecise after that and the panel is too
 * narrow for "3 weeks 2 days".
 *
 * `now` is injectable for tests.
 */
export function formatSlackRelative(ts: string, now: Date = new Date()): string {
	const date = parseSlackTs(ts);
	if (date === null) {
		return '—';
	}
	const diffSec = Math.max(0, Math.floor((now.getTime() - date.getTime()) / 1000));
	if (diffSec < 5) {
		return 'just now';
	}
	if (diffSec < MINUTE) {
		return `${diffSec}s`;
	}
	if (diffSec < HOUR) {
		return `${Math.floor(diffSec / MINUTE)} min`;
	}
	if (diffSec < DAY) {
		return `${Math.floor(diffSec / HOUR)} h`;
	}
	if (diffSec < DAY * 2) {
		return 'yesterday';
	}
	if (diffSec < WEEK) {
		return `${Math.floor(diffSec / DAY)} d`;
	}
	// > 1 week: drop the year only when it matches the current one,
	// to keep older threads unambiguous without screaming "2026" at
	// the user every time.
	const sameYear = date.getFullYear() === now.getFullYear();
	return date.toLocaleDateString(undefined, {
		month: 'short',
		day: 'numeric',
		...(sameYear ? {} : { year: 'numeric' }),
	});
}

/**
 * Hour-and-minute string for the in-thread bubble timestamp. Always
 * 24-hour for consistency across team locales.
 */
export function formatSlackTime(ts: string): string {
	const date = parseSlackTs(ts);
	if (date === null) {
		return '—';
	}
	return date.toLocaleTimeString(undefined, {
		hour: '2-digit',
		minute: '2-digit',
		hour12: false,
	});
}
