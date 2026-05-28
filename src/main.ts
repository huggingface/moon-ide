import { mount } from 'svelte';
import App from './App.svelte';
import './styles.css';
import { frontendLog } from './lib/logs.svelte';

// Global capture for unhandled promise rejections / uncaught errors.
// Without this they only surface as `Unhandled Promise Rejection:
// [object Object]` in the devtools console with no stack and no
// payload visible — fine for spotting that *something* is broken,
// useless for diagnosing what. Routing them through `frontendLog`
// gives us the message + reason in the diag-logs panel (source
// `runtime`) and keeps the console log too so existing muscle
// memory still works.
function describeReason(reason: unknown): string {
	if (reason instanceof Error) {
		return `${reason.name}: ${reason.message}\n${reason.stack ?? '(no stack)'}`;
	}
	if (typeof reason === 'object' && reason !== null) {
		try {
			return JSON.stringify(reason);
		} catch {
			return Object.prototype.toString.call(reason);
		}
	}
	return String(reason);
}
window.addEventListener('unhandledrejection', (event) => {
	frontendLog('runtime', 'error', `unhandledrejection: ${describeReason(event.reason)}`);
});
window.addEventListener('error', (event) => {
	const detail = event.error ? describeReason(event.error) : event.message;
	frontendLog('runtime', 'error', `window.error: ${detail}`);
});

const target = document.getElementById('app');
if (!target) {
	throw new Error('mount target #app not found');
}

// Wipe the inline boot splash (`index.html`) before Svelte mounts so
// we don't end up with both the static markup and the component
// splash layered in the same container. Svelte 5's `mount()`
// appends; it doesn't replace.
target.replaceChildren();

mount(App, { target });
