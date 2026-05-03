// Shared types for `ContextMenu.svelte`. Lives outside the
// component file so non-Svelte consumers (state helpers, unit
// tests, call sites that import the type but never instantiate the
// component) can reach it without going through a `.svelte`
// import — and so we don't rely on `<script lang="ts">` exporting
// a type alias, which Svelte's compiler allows but tooling handles
// inconsistently.

export type ContextMenuItem = {
	id: string;
	label: string;
	onSelect: () => void;
	disabled?: boolean;
	/** Visual grouping — items sharing a kind render adjacent, kinds
	 * are separated by a thin divider. Defaults to `'default'` so
	 * callers that don't care just get a flat list. */
	kind?: 'default' | 'danger';
	title?: string;
};
