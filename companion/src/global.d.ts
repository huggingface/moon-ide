/// <reference types="svelte" />
/// <reference types="vite/client" />

/** Build stamp (`<git-sha>[-dirty] <build-iso-date>`) injected by
 * `companion/vite.config.ts` via `define`. `'dev'` under the dev
 * server would be nicer, but a constant keeps the wiring trivial —
 * dev builds just show the current sha + serve time. */
declare const __BUILD_INFO__: string;

declare module '*.css' {
	const content: string;
	export default content;
}
