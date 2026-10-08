// The completion callback a background sub-agent sends its parent
// (ADR 0091): a user-role message the backend formats as
// `<subagent_report subagent_id="…" status="…">\n…\n</subagent_report>`.
// Detected here so the transcript renders it as a report card
// instead of a "you" bubble holding raw tags.

export type SubagentReportMessage = {
	subagentId: string;
	status: 'done' | 'error';
	body: string;
};

const PATTERN = /^<subagent_report subagent_id="([^"]*)" status="(done|error)">\n([\s\S]*)\n<\/subagent_report>$/;

export function parseSubagentReport(text: string): SubagentReportMessage | null {
	const match = PATTERN.exec(text);
	if (match === null) {
		return null;
	}
	const [, subagentId = '', status, body = ''] = match;
	return { subagentId, status: status === 'error' ? 'error' : 'done', body };
}
