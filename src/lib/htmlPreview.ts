// The frames an agent's `html preview` fences are drawn in (ADR 0127).
//
// The frame runs no script at all (`sandbox` without `allow-scripts`),
// which is what makes `allow-same-origin` safe: nothing inside can use
// that origin, while this side can read the frame's document to size
// it to its content and to catch its link clicks. A policy in the
// document itself blocks every fetch, so a remote image or font can't
// report that the transcript was opened; only inline styles and
// `data:` images draw. No forms, no popups, no navigating the IDE.

const POLICY = "default-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:";

/** The IDE's variables a preview may use, so a mockup looks native. */
const THEME_VARS = [
	'--m-bg',
	'--m-bg-1',
	'--m-bg-2',
	'--m-bg-3',
	'--m-fg',
	'--m-fg-muted',
	'--m-fg-subtle',
	'--m-border',
	'--m-border-strong',
	'--m-accent',
	'--m-accent-strong',
	'--m-accent-tint',
	'--m-danger',
	'--m-success',
	'--m-warning',
	'--m-font-ui',
	'--m-font-mono',
	'--m-radius-sm',
	'--m-radius-md',
	'--m-radius-lg',
	'--m-space-1',
	'--m-space-2',
	'--m-space-3',
	'--m-space-4',
	'--m-text-xs',
	'--m-text-sm',
	'--m-text-md',
	'--m-text-lg',
];

/** The frame's document: the policy, the theme, and the agent's HTML
 *  as the body. */
export function previewDocument(html: string, theme: Record<string, string>, scheme = 'normal'): string {
	const vars = Object.entries(theme)
		.filter(([, value]) => value !== '')
		.map(([name, value]) => `${name}: ${value};`)
		.join(' ');
	return (
		`<!doctype html><html><head><meta charset="utf-8">` +
		`<meta http-equiv="Content-Security-Policy" content="${POLICY}">` +
		// The page's own colour scheme: a frame whose scheme differs
		// from its embedder's gets an opaque canvas of its own.
		`<style>:root { ${vars} color-scheme: ${scheme}; } ` +
		`html, body { margin: 0; background: var(--m-bg); color: var(--m-fg); ` +
		`font-family: var(--m-font-ui, sans-serif); font-size: var(--m-text-md, 13px); line-height: 1.45; } ` +
		`body { padding: 8px; overflow: hidden; } a { color: var(--m-accent); } ` +
		`*, *::before, *::after { box-sizing: border-box; }</style>` +
		`</head><body>${html}</body></html>`
	);
}

/** The slot's HTML, which `markdown.ts` stores base64. */
export function decodePreview(encoded: string): string {
	try {
		return new TextDecoder().decode(Uint8Array.from(atob(encoded), (char) => char.charCodeAt(0)));
	} catch {
		return '';
	}
}

function currentTheme(): Record<string, string> {
	const style = getComputedStyle(document.documentElement);
	return Object.fromEntries(THEME_VARS.map((name) => [name, style.getPropertyValue(name).trim()]));
}

/** Fill every preview slot under `root` not filled yet. `onLink` gets
 *  the `href` of a link clicked in a frame, which never follows it. */
export function mountHtmlPreviews(root: HTMLElement, onLink: (href: string) => void): void {
	const slots = root.querySelectorAll<HTMLElement>('.md-html-preview:not([data-mounted])');
	if (slots.length === 0) {
		return;
	}
	const theme = currentTheme();
	const scheme = getComputedStyle(document.documentElement).colorScheme || 'normal';
	for (const slot of slots) {
		slot.dataset.mounted = 'true';
		const frame = document.createElement('iframe');
		frame.setAttribute('sandbox', 'allow-same-origin');
		frame.setAttribute('referrerpolicy', 'no-referrer');
		frame.title = 'Preview drawn by the agent';
		frame.srcdoc = previewDocument(decodePreview(slot.dataset.html ?? ''), theme, scheme);
		frame.addEventListener('load', () => wireFrame(frame, onLink));
		slot.replaceChildren(frame);
	}
}

/** The link a click in the frame landed in, if any. The frame's
 *  nodes belong to its own realm, so `instanceof Element` is false for
 *  them here: they are read through their own methods. */
function linkAt(target: EventTarget | null): { href: string | null } | null {
	if (target === null) {
		return null;
	}
	const closest: unknown = Reflect.get(target, 'closest');
	if (typeof closest !== 'function') {
		return null;
	}
	const anchor: unknown = Reflect.apply(closest, target, ['a']);
	if (anchor === null || typeof anchor !== 'object') {
		return null;
	}
	const href: unknown = Reflect.apply(Reflect.get(anchor, 'getAttribute'), anchor, ['href']);
	return { href: typeof href === 'string' ? href : null };
}

function wireFrame(frame: HTMLIFrameElement, onLink: (href: string) => void): void {
	const doc = frame.contentDocument;
	if (doc === null) {
		return;
	}
	const fit = () => {
		frame.style.height = `${Math.ceil(doc.documentElement.scrollHeight)}px`;
	};
	fit();
	// A link opens a new window, which the sandbox refuses (no
	// popups): it can't load a page into the frame either, whether or
	// not the click below reaches us.
	for (const anchor of doc.querySelectorAll('a[href]')) {
		anchor.setAttribute('target', '_blank');
	}
	// Late layout (an embedded image decoding) and a narrower panel
	// both change the height.
	new ResizeObserver(fit).observe(doc.body);
	doc.addEventListener('click', (event) => {
		const anchor = linkAt(event.target);
		if (anchor === null) {
			return;
		}
		event.preventDefault();
		const href = anchor.href;
		if (href !== null && href !== '' && !href.startsWith('#')) {
			onLink(href);
		}
	});
}
