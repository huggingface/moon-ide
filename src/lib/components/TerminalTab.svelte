<script lang="ts">
	import { onDestroy, onMount, untrack } from 'svelte';
	import { Terminal, type IDisposable, type ILink, type ILinkProvider, type ITheme } from '@xterm/xterm';
	import { FitAddon } from '@xterm/addon-fit';
	import { WebLinksAddon } from '@xterm/addon-web-links';
	import { openUrl } from '@tauri-apps/plugin-opener';
	import '@xterm/xterm/css/xterm.css';

	import type { TerminalTab } from '../bottomPanel.svelte';
	import { terminal as terminalStore } from '../terminal.svelte';
	import { workspace } from '../state.svelte';
	import { parseFileLinks, resolveTerminalLink } from '../terminalLinks';

	// Body component for a `kind: 'terminal'` bottom-panel tab.
	// Mounts an xterm.js Terminal wired to the store's IO bus:
	// keystrokes go out via `ipc.terminal.write`, output bytes
	// from the supervisor are pushed in via the store's writer
	// registry. Keeping the Terminal alive across tab switches
	// is the bottom panel's responsibility — `BottomPanel.svelte`
	// renders every tab body and hides inactive ones with
	// `display: none`, so `xterm`'s scrollback survives the
	// switch.
	//
	// Where, what, and exit status all live in the tab title
	// (icon for host vs container, basename for cwd, suffix
	// for exit code) — see `BottomPanel.svelte`. The body
	// itself is just xterm, with one error fallback when the
	// supervisor refuses to spawn.

	type Props = { tab: TerminalTab };
	let { tab }: Props = $props();

	const session = $derived(terminalStore.sessionFor(tab.id));
	const openError = $derived(session?.openError ?? null);

	let hostEl: HTMLDivElement | null = $state(null);
	let term: Terminal | null = null;
	let fitAddon: FitAddon | null = null;
	let resizeObserver: ResizeObserver | null = null;
	let fileLinkProvider: IDisposable | null = null;

	onMount(() => {
		if (!hostEl) {
			return;
		}

		// `convertEol: false` — we never inject text ourselves;
		// the host shell takes care of CR/LF semantics. Theme
		// values are read from the same CSS tokens the editor
		// uses so terminal and editor coexist visually; a
		// separate `$effect` below re-applies them when the
		// active theme flips. Once Phase 8 ships Pierre's
		// themes, this gets sourced from one place.
		term = new Terminal({
			fontFamily: 'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
			fontSize: 12,
			cursorBlink: true,
			scrollback: 5000,
			convertEol: false,
			theme: terminalThemeFromCss(),
		});
		fitAddon = new FitAddon();
		term.loadAddon(fitAddon);
		// xterm's default click handler is `window.open(uri,
		// '_blank')`, which is a no-op inside Tauri's webview —
		// the link gets detected and hover-underlined, but the
		// click does nothing. Route through the `opener` plugin
		// (same path the rest of the IDE uses for external URLs)
		// so clicking `http://localhost:4508` in a container
		// terminal actually launches the host's default browser.
		term.loadAddon(
			new WebLinksAddon((event, uri) => {
				event.preventDefault();
				void openUrl(uri);
			}),
		);

		term.open(hostEl);
		// File-link provider — recognise `file:///abs/path:line:col`
		// URIs and bare absolute paths in the row text and
		// underline them on hover. Activation is gated on
		// Ctrl/Cmd-click (matching the editor's goto-definition
		// gesture) so plain clicks and drag-selection across a
		// stack-trace path keep working. Container `/workspace/...`
		// paths are reverse-mapped to the bound folder via
		// basename match — same convention `containerCwdFor`
		// uses when opening a container terminal. Resolution
		// fans out: a host shell that prints container paths or
		// vice versa still gets links.
		fileLinkProvider = term.registerLinkProvider(buildFileLinkProvider());
		// Windows-Terminal-style copy/paste mapping. xterm.js
		// ships neither by default, so we intercept the keydown
		// before it reaches the terminal's input pipeline.
		// `attachCustomKeyEventHandler` returning `false` swallows
		// the event entirely (no PTY write, no scroll, no bell).
		// `event.code` is layout-independent — important on a
		// French keyboard where `event.key` for the C key shifts
		// to a different glyph.
		//
		// `Ctrl+C` is overloaded: a non-empty selection copies
		// (and keeps the selection visible so the user can
		// re-verify what landed in the clipboard, drag to extend
		// it, or fire another copy); an empty selection falls
		// through to xterm's default and sends SIGINT. `Ctrl+V`
		// always pastes. `Ctrl+L` is swallowed here when text is
		// selected so the window-level handler in App.svelte
		// (which forwards the selection to the coder composer)
		// is the only thing that runs — without this, the shell
		// would still see `^L` and clear its screen, dropping
		// the selected scrollback on the floor.
		// We deliberately don't reserve the `Ctrl+Shift+*`
		// variants — the user already has unambiguous primaries.
		term.attachCustomKeyEventHandler((event) => {
			if (event.type !== 'keydown' || !event.ctrlKey || event.shiftKey || event.altKey || event.metaKey) {
				return true;
			}
			if (event.code === 'KeyC') {
				const text = term?.getSelection() ?? '';
				if (text.length === 0) {
					return true;
				}
				void navigator.clipboard
					.writeText(text)
					.then(() => {
						workspace.flash('Copied');
					})
					.catch(() => {
						// Swallow — failing silently is better than a
						// modal; the user can retry, or fall back to
						// the menu copy via right-click selection.
					});
				return false;
			}
			if (event.code === 'KeyV') {
				void navigator.clipboard
					.readText()
					.then((text) => {
						if (text.length === 0) {
							return;
						}
						void terminalStore.writeInput(tab.id, encoder.encode(text));
					})
					.catch(() => {
						// As above — silent failure beats a modal.
					});
				return false;
			}
			if (event.code === 'KeyL' && (term?.getSelection() ?? '').length > 0) {
				return false;
			}
			return true;
		});
		// Defer the initial fit: in some startup paths the panel
		// is still settling layout when `onMount` fires, so a
		// direct `fit()` here can compute a 1-row grid and stick
		// xterm with it. `refit` skips zero-size containers, and
		// the ResizeObserver below picks up the first real size.
		refit();

		// Hook the store's IO bus. The writer pushes raw bytes
		// from the supervisor straight into xterm — its ANSI
		// parser handles colour, cursor, alt-screen, etc.
		untrack(() => {
			terminalStore.setWriter(tab.id, (bytes) => {
				if (term) {
					term.write(bytes);
				}
			});
		});

		// Forward keystrokes (and pasted text) to the supervisor.
		// `onData` already decodes xterm's input modes correctly
		// (e.g. arrow keys → CSI sequences); we just transport.
		term.onData((data) => {
			void terminalStore.writeInput(tab.id, encoder.encode(data));
		});

		// Mirror xterm's selection state into the terminal store
		// so App.svelte's Ctrl+L handler can attach the highlighted
		// scrollback to the coder composer when the editor has
		// nothing selected. xterm doesn't pass the text on the
		// event, so we read it via `getSelection()` each fire —
		// the operation is O(rows) on the live selection range,
		// fine even for kilobyte drag-selects.
		term.onSelectionChange(() => {
			const text = term?.getSelection() ?? '';
			terminalStore.setSelection(tab.id, text, tab.title);
		});

		// Resize: refit on container resize. The fit addon reads
		// the host element's bounding box, so this fires whenever
		// the panel height changes or the user toggles between
		// tabs (ResizeObserver picks up display:none flipping
		// back to flex).
		resizeObserver = new ResizeObserver(() => {
			refit();
		});
		resizeObserver.observe(hostEl);
	});

	// Re-theme on every dark/light flip. Xterm.js has no CSS-
	// variable pathway for its colours; the palette is copied into
	// the Terminal's option bag at construction and stays stale
	// until we overwrite it. Reading `workspace.effectiveTheme`
	// (not `workspace.theme`) so "System" also repaints when the
	// OS preference changes mid-session.
	$effect(() => {
		workspace.effectiveTheme;
		if (!term) {
			return;
		}
		term.options.theme = terminalThemeFromCss();
	});

	onDestroy(() => {
		resizeObserver?.disconnect();
		resizeObserver = null;
		terminalStore.clearWriter(tab.id);
		fileLinkProvider?.dispose();
		fileLinkProvider = null;
		term?.dispose();
		term = null;
		fitAddon = null;
	});

	/**
	 * One xterm `ILinkProvider` per Terminal instance. xterm
	 * calls `provideLinks(y, cb)` each time the cursor enters
	 * a new row; we read the row's text, scan it for path
	 * matches, and hand back ranges with hover/leave (so the
	 * underline shows up) plus an `activate` callback gated on
	 * Ctrl/Cmd-click.
	 *
	 * Wrapped lines are ignored — most stack-trace paths fit
	 * in one row, and stitching wrap continuations is
	 * surprisingly involved (xterm exposes `isWrapped` per
	 * line but we'd have to walk forward/backward, re-derive
	 * column ranges across the join, and avoid double-counting
	 * matches that overlap the wrap point). If a path ever
	 * actually wraps in real usage we revisit then.
	 */
	function buildFileLinkProvider(): ILinkProvider {
		return {
			provideLinks(y: number, callback: (links: ILink[] | undefined) => void): void {
				const t = term;
				if (t === null) {
					callback(undefined);
					return;
				}
				const buffer = t.buffer.active;
				const line = buffer.getLine(y - 1);
				if (line === undefined) {
					callback(undefined);
					return;
				}
				// `trimRight: true` strips trailing whitespace cells
				// so the regex doesn't have to deal with them.
				const text = line.translateToString(true);
				const matches = parseFileLinks(text);
				if (matches.length === 0) {
					callback(undefined);
					return;
				}
				const links: ILink[] = matches.map((m) => ({
					// xterm's `IBufferRange` is 1-based and
					// inclusive on both ends; the JS string
					// indices are 0-based half-open, so `+1` on
					// start and use `end` directly for the
					// inclusive end column.
					range: {
						start: { x: m.start + 1, y },
						end: { x: m.end, y },
					},
					text: text.slice(m.start, m.end),
					activate: (event) => {
						// Gate on Ctrl (Linux/Win) or Cmd (mac).
						// Bare clicks fall through silently so the
						// user can drag-select across a path
						// without launching a navigation.
						if (!event.ctrlKey && !event.metaKey) {
							return;
						}
						event.preventDefault();
						const resolved = resolveTerminalLink(m, workspace.workspace);
						if (resolved === null) {
							return;
						}
						void workspace.jumpTo(
							resolved.path,
							{ line: resolved.line, character: resolved.character },
							workspace.focusedSide,
							resolved.folder,
						);
					},
				}));
				callback(links);
			},
		};
	}

	function refit() {
		if (!fitAddon || !term) {
			return;
		}
		// Skip the fit when the host element is collapsed
		// (display:none or zero size); the addon would compute
		// 0×0 cols/rows and brick the PTY's view of the screen.
		if (!hostEl || hostEl.clientWidth === 0 || hostEl.clientHeight === 0) {
			return;
		}
		fitAddon.fit();
		void terminalStore.resize(tab.id, term.cols, term.rows);
	}

	function focusTerminal() {
		term?.focus();
	}

	const encoder = new TextEncoder();

	function terminalThemeFromCss(): ITheme {
		// Read from the same CSS tokens the editor uses so
		// dark/light theme switching keeps the terminal in
		// step. Fallbacks are the dark palette literals so the
		// terminal never paints on a transparent background if
		// a token is missing.
		const css = getComputedStyle(document.documentElement);
		const v = (name: string, fallback: string) => css.getPropertyValue(name).trim() || fallback;
		return {
			background: v('--m-bg', '#0e0f12'),
			foreground: v('--m-fg', '#e1e3e8'),
			cursor: v('--m-fg', '#e1e3e8'),
			cursorAccent: v('--m-bg', '#0e0f12'),
			selectionBackground: v('--m-selection', '#264f78'),
		};
	}
</script>

<div class="term-wrap" onclick={focusTerminal} role="presentation">
	{#if openError}
		<div class="error" role="alert">
			Failed to open terminal: {openError}
		</div>
	{:else}
		<div class="term-host" bind:this={hostEl}></div>
	{/if}
</div>

<style>
	.term-wrap {
		flex: 1;
		display: flex;
		flex-direction: column;
		min-height: 0;
		min-width: 0;
		background: var(--m-bg);
	}
	.error {
		padding: 8px 12px;
		color: var(--m-danger);
	}
	.term-host {
		flex: 1;
		min-height: 0;
		min-width: 0;
		overflow: hidden;
		padding: 4px 6px 0 6px;
	}
	.term-host :global(.xterm),
	.term-host :global(.xterm-viewport) {
		background-color: var(--m-bg) !important;
	}
</style>
