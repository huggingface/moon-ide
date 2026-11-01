// The message background work sends its session when it finishes
// (ADR 0102): a user-role message the backend formats as
// `<background_result kind="…" id="…" status="…" [exit_code="…"]>\n…\n</background_result>`.
// Detected here so the transcript renders it as a result card
// instead of a "you" bubble holding raw tags.

export type BackgroundResultMessage = {
	kind: 'process' | 'subagent';
	id: string;
	status: 'done' | 'error';
	/** Processes only; `"unknown"` when the exit code wasn't captured. */
	exitCode: string | null;
	body: string;
};

const PATTERN =
	/^<background_result kind="(process|subagent)" id="([^"]*)" status="(done|error)"(?: exit_code="([^"]*)")?>\n([\s\S]*)\n<\/background_result>$/;

export function parseBackgroundResult(text: string): BackgroundResultMessage | null {
	const match = PATTERN.exec(text);
	if (match === null) {
		return null;
	}
	const [, kind, id = '', status, exitCode, body = ''] = match;
	return {
		kind: kind === 'process' ? 'process' : 'subagent',
		id,
		status: status === 'error' ? 'error' : 'done',
		exitCode: exitCode ?? null,
		body,
	};
}
