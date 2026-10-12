import { describe, expect, it } from 'vitest';

import { hintFor, parseHttpError } from './coderError';

describe('parseHttpError', () => {
	it('parses an Anthropic error envelope', () => {
		const text =
			'HTTP 401 from https://api.anthropic.com/v1/messages: {"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"},"request_id":"req_011Cfx9bW6gjgsHw9LdEhKDD"}';
		const parsed = parseHttpError(text);
		expect(parsed).toMatchObject({
			status: 401,
			host: 'api.anthropic.com',
			title: 'Authentication failed',
			message: 'invalid x-api-key',
			errorType: 'authentication_error',
			requestId: 'req_011Cfx9bW6gjgsHw9LdEhKDD',
			prefix: null,
		});
		expect(parsed?.rawBody).toContain('\n');
		expect(parsed && hintFor(parsed)).toContain('model settings');
	});

	it('parses an OpenAI-style envelope with the request id suffix', () => {
		const text =
			'HTTP 429 from https://router.huggingface.co/v1/chat/completions: {"error":{"message":"Too many requests","type":"rate_limit_exceeded"}} [request id: abc-123]';
		expect(parseHttpError(text)).toMatchObject({
			status: 429,
			host: 'router.huggingface.co',
			title: 'Rate limited',
			message: 'Too many requests',
			requestId: 'abc-123',
		});
	});

	it('handles a plain string error field', () => {
		const parsed = parseHttpError('HTTP 503 from https://router.huggingface.co/x: {"error":"Model is loading"}');
		expect(parsed).toMatchObject({ title: 'Service unavailable', message: 'Model is loading', errorType: null });
	});

	it('keeps a non-JSON body as the message', () => {
		const parsed = parseHttpError('HTTP 500 from https://example.com/x: upstream exploded');
		expect(parsed).toMatchObject({
			title: 'Provider error',
			message: 'upstream exploded',
			rawBody: 'upstream exploded',
		});
	});

	it('drops HTML bodies from the message', () => {
		const parsed = parseHttpError('HTTP 502 from https://example.com/x: <html><body>Bad Gateway</body></html>');
		expect(parsed?.message).toBeNull();
	});

	it('captures a wrapping prefix', () => {
		const parsed = parseHttpError('compaction failed: HTTP 400 from https://example.com/x: nope');
		expect(parsed?.prefix).toBe('compaction failed');
	});

	it('returns null for non-HTTP errors', () => {
		expect(parseHttpError('network error: connection refused')).toBeNull();
	});

	it('suggests re-signing in for Hugging Face auth failures', () => {
		const parsed = parseHttpError('HTTP 401 from https://huggingface.co/api/whoami-v2: unauthorized');
		expect(parsed && hintFor(parsed)).toContain('Sign out');
	});
});
