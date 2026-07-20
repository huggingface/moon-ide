// Companion PWA service worker. Deliberately tiny and deploy-safe:
//
// - Navigations (the app shell) are network-first with a cache
//   fallback, so a new deploy shows up on the next load and the app
//   still opens from the home screen when the relay is unreachable
//   (it then renders its own "reconnecting" state).
// - Hashed build assets (/assets/*) are cache-first: their names
//   change on every build, so a cached copy can never go stale.
// - Everything else same-origin (manifest, icons) is network-first.
//
// The WebSocket to the bridge is untouched — service workers don't
// intercept WS upgrades.

const CACHE = 'companion-v1';

self.addEventListener('install', (event) => {
	event.waitUntil(
		caches
			.open(CACHE)
			.then((cache) => cache.add('/'))
			.then(() => self.skipWaiting()),
	);
});

self.addEventListener('activate', (event) => {
	event.waitUntil(
		caches
			.keys()
			.then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
			.then(() => self.clients.claim()),
	);
});

self.addEventListener('fetch', (event) => {
	const req = event.request;
	if (req.method !== 'GET') {
		return;
	}
	const url = new URL(req.url);
	if (url.origin !== self.location.origin) {
		return;
	}

	// Hashed build assets: immutable, cache-first.
	if (url.pathname.startsWith('/assets/')) {
		event.respondWith(
			caches.match(req).then(
				(hit) =>
					hit ??
					fetch(req).then((resp) => {
						if (resp.ok) {
							const copy = resp.clone();
							void caches.open(CACHE).then((cache) => cache.put(req, copy));
						}
						return resp;
					}),
			),
		);
		return;
	}

	// Navigations + everything else: network-first, cache fallback.
	const cacheKey = req.mode === 'navigate' ? '/' : req;
	event.respondWith(
		fetch(req)
			.then((resp) => {
				if (resp.ok) {
					const copy = resp.clone();
					void caches.open(CACHE).then((cache) => cache.put(cacheKey, copy));
				}
				return resp;
			})
			.catch(() => caches.match(cacheKey).then((hit) => hit ?? Response.error())),
	);
});
