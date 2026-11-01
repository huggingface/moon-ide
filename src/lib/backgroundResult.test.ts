import { describe, expect, it } from 'vitest';

import { parseBackgroundResult } from './backgroundResult';

describe('parseBackgroundResult', () => {
	it('parses a sub-agent report', () => {
		const text =
			'<background_result kind="subagent" id="sub-1" status="done">\nFound 3 callsites.\n\nDetails.\n</background_result>';
		expect(parseBackgroundResult(text)).toEqual({
			kind: 'subagent',
			id: 'sub-1',
			status: 'done',
			exitCode: null,
			body: 'Found 3 callsites.\n\nDetails.',
		});
	});

	it('parses a process exit with its exit code', () => {
		const text =
			'<background_result kind="process" id="bg_4" status="error" exit_code="1">\n$ pnpm test\n2 failed\n</background_result>';
		expect(parseBackgroundResult(text)).toEqual({
			kind: 'process',
			id: 'bg_4',
			status: 'error',
			exitCode: '1',
			body: '$ pnpm test\n2 failed',
		});
	});

	it('ignores ordinary messages', () => {
		expect(parseBackgroundResult('please look at <background_result>')).toBeNull();
	});
});
