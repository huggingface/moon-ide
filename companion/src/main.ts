import { mount } from 'svelte';
import App from './App.svelte';
import './styles.css';

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
