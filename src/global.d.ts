/// <reference types="svelte" />
/// <reference types="vite/client" />

declare module '*.css' {
	const content: string;
	export default content;
}

declare module '*.svg' {
	const src: string;
	export default src;
}
