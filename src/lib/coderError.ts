// Turn errors reach the transcript as the backend's flattened
// `CoderError` display string (it's also what the session JSONL
// persists, so replayed sessions carry the same text). The common
// failure is a provider HTTP error whose body is a JSON envelope —
// unreadable as one line. This recovers the structure for the card.

export type ParsedHttpError = {
	status: number;
	endpoint: string;
	host: string;
	/** Short human title derived from the status / provider error type. */
	title: string;
	/** The provider's own message, when the body carried one. */
	message: string | null;
	/** Provider error type / code (`authentication_error`, `rate_limit_exceeded`, …). */
	errorType: string | null;
	requestId: string | null;
	/** Body pretty-printed when it was JSON, verbatim otherwise. */
	rawBody: string;
	/** Text before `HTTP …` when the error was wrapped by another layer. */
	prefix: string | null;
};

const HTTP_PATTERN = /^([\s\S]*?)HTTP (\d{3}) from (\S+): ([\s\S]*)$/;
const REQUEST_ID_SUFFIX = /\s*\[request id: ([^\]]+)\]\s*$/;

export function parseHttpError(text: string): ParsedHttpError | null {
	const match = HTTP_PATTERN.exec(text);
	if (match === null) {
		return null;
	}
	const [, rawPrefix = '', statusText = '', endpoint = '', rest = ''] = match;
	const status = Number(statusText);

	let body = rest;
	let requestId: string | null = null;
	const idMatch = REQUEST_ID_SUFFIX.exec(body);
	if (idMatch !== null) {
		requestId = idMatch[1]?.trim() || null;
		body = body.slice(0, idMatch.index);
	}
	body = body.trim();

	let message: string | null = null;
	let errorType: string | null = null;
	let rawBody = body;
	const json = tryParseJson(body);
	if (json !== undefined) {
		rawBody = JSON.stringify(json, null, '\t');
		const extracted = extractProviderError(json);
		message = extracted.message;
		errorType = extracted.errorType;
		requestId ??= extracted.requestId;
	} else if (body !== '' && !looksLikeHtml(body)) {
		message = body;
	}

	const prefix = rawPrefix.trim().replace(/:$/, '').trim();
	return {
		status,
		endpoint,
		host: hostOf(endpoint),
		title: titleFor(status, errorType),
		message,
		errorType,
		requestId,
		rawBody,
		prefix: prefix === '' ? null : prefix,
	};
}

/** Actionable next step for the statuses where there is one. */
export function hintFor(err: ParsedHttpError): string | null {
	if (err.status === 401 || err.status === 403) {
		return isHuggingFaceHost(err.host)
			? 'Your Hugging Face session may have expired. Sign out and sign in again.'
			: 'Check the API key configured for this provider in model settings.';
	}
	if (err.status === 429) {
		return 'The provider is rate-limiting requests. Wait a moment and retry.';
	}
	if (err.status === 529 || err.status >= 500) {
		return 'The provider had a server-side problem. Retrying usually works.';
	}
	if (err.status === 413) {
		return 'The request was too large. Compact the session or start a new one.';
	}
	return null;
}

export function isHuggingFaceHost(host: string): boolean {
	return host === 'huggingface.co' || host.endsWith('.huggingface.co') || host === 'hf.co';
}

function titleFor(status: number, errorType: string | null): string {
	switch (errorType) {
		case 'authentication_error':
		case 'invalid_api_key':
			return 'Authentication failed';
		case 'permission_error':
			return 'Permission denied';
		case 'rate_limit_error':
		case 'rate_limit_exceeded':
			return 'Rate limited';
		case 'overloaded_error':
			return 'Provider overloaded';
		case 'insufficient_quota':
			return 'Quota exceeded';
		case 'not_found_error':
		case 'model_not_found':
			return 'Not found';
		case 'request_too_large':
		case 'context_length_exceeded':
			return 'Request too large';
		case 'invalid_request_error':
			return 'Invalid request';
		default:
			break;
	}
	switch (status) {
		case 400:
			return 'Bad request';
		case 401:
			return 'Authentication failed';
		case 402:
			return 'Payment required';
		case 403:
			return 'Permission denied';
		case 404:
			return 'Not found';
		case 408:
			return 'Request timed out';
		case 413:
			return 'Request too large';
		case 422:
			return 'Invalid request';
		case 429:
			return 'Rate limited';
		case 502:
			return 'Bad gateway';
		case 503:
			return 'Service unavailable';
		case 504:
			return 'Gateway timed out';
		case 529:
			return 'Provider overloaded';
		default:
			return status >= 500 ? 'Provider error' : 'Request failed';
	}
}

type Extracted = { message: string | null; errorType: string | null; requestId: string | null };

// Covers the envelopes we actually see: Anthropic
// (`{type:"error", error:{type, message}, request_id}`), OpenAI-style
// (`{error:{message, type, code}}`), and HF / TGI (`{error:"…"}`,
// `{message:"…"}`, `{detail:"…"}`).
function extractProviderError(json: unknown): Extracted {
	const out: Extracted = { message: null, errorType: null, requestId: null };
	if (!isRecord(json)) {
		return out;
	}
	out.requestId = stringField(json, 'request_id') ?? stringField(json, 'requestId');
	const inner = json.error;
	if (isRecord(inner)) {
		out.message = stringField(inner, 'message');
		out.errorType = stringField(inner, 'type') ?? stringField(inner, 'code');
		if (out.errorType === null && typeof inner.code === 'number') {
			out.errorType = String(inner.code);
		}
		return out;
	}
	if (typeof inner === 'string' && inner !== '') {
		out.message = inner;
		out.errorType = stringField(json, 'error_type');
		return out;
	}
	out.message = stringField(json, 'message') ?? stringField(json, 'detail');
	const type = stringField(json, 'type');
	out.errorType = type === 'error' ? null : type;
	return out;
}

function tryParseJson(text: string): unknown {
	if (!text.startsWith('{') && !text.startsWith('[')) {
		return undefined;
	}
	try {
		return JSON.parse(text) as unknown;
	} catch {
		return undefined;
	}
}

function looksLikeHtml(text: string): boolean {
	return /^<(!doctype|html)/i.test(text);
}

function hostOf(endpoint: string): string {
	try {
		return new URL(endpoint).host;
	} catch {
		return endpoint;
	}
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function stringField(obj: Record<string, unknown>, key: string): string | null {
	const value = obj[key];
	return typeof value === 'string' && value !== '' ? value : null;
}
