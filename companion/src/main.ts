import { mount } from 'svelte';
import App from './App.svelte';
import './styles.css';

// Version check: open the console (or `remote debugging → console`
// on the phone) and compare against `git rev-parse --short HEAD` on
// the deploying machine. Answers "did my deploy actually land?"
// through the service-worker + browser cache layers.
// eslint-disable-next-line no-console
console.info(`[moon-companion] build ${__BUILD_INFO__}`);

const target = document.getElementById('app');
if (!target) {
	throw new Error('missing #app mount point');
}

mount(App, { target });

// Register the PWA service worker (home-screen installability +
// offline app shell). Dev server doesn't serve /sw.js from public
// with the right scope semantics we care about — registration is
// production-only and best-effort.
if (import.meta.env.PROD && 'serviceWorker' in navigator) {
	window.addEventListener('load', () => {
		void navigator.serviceWorker.register('/sw.js');
	});
}
