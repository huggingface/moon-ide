import { describe, expect, it } from 'vitest';

import { parseSubagentReport } from './subagentReport';

describe('parseSubagentReport', () => {
	it('parses the backend callback format', () => {
		const text =
			'<subagent_report subagent_id="sub-1" status="done">\nFound 3 callsites.\n\nDetails.\n</subagent_report>';
		expect(parseSubagentReport(text)).toEqual({
			subagentId: 'sub-1',
			status: 'done',
			body: 'Found 3 callsites.\n\nDetails.',
		});
	});

	it('reads the error status', () => {
		const text = '<subagent_report subagent_id="sub-2" status="error">\nboom\n</subagent_report>';
		expect(parseSubagentReport(text)?.status).toBe('error');
	});

	it('ignores ordinary messages', () => {
		expect(parseSubagentReport('please look at <subagent_report>')).toBeNull();
	});
});
