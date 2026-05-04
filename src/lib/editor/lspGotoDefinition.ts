// Ctrl/Cmd-click "go to definition" for CodeMirror.
//
// Two user-facing affordances:
//
// 1. **Link preview**: holding the platform "jump" modifier (Ctrl on
//    Linux/Windows, Cmd on macOS) and hovering over an identifier
//    underlines the span iff the LSP has a definition target for it.
//    No decoration = no jump available; users learn fast.
// 2. **Jump on click**: a click with the modifier held calls
//    `textDocument/definition`, then routes through
//    `workspace.jumpTo(path, position)` so the tab opens, the caret
//    lands on the target, and the move is recorded in nav history.
//
// External targets (node_modules, Rust toolchain sources) surface a
// toast rather than silently opening nothing — we don't have a
// read-only external-file viewer yet.
//
// This module is pure-ish: it takes `jumpTo` and `flash` callbacks at
// construction time so it never imports `state.svelte.ts`. The
// Editor component wires the real workspace methods through.

import type { Extension } from '@codemirror/state';
import { StateEffect, StateField } from '@codemirror/state';
import { Decoration, type DecorationSet, EditorView, ViewPlugin } from '@codemirror/view';
import { ipc } from '../ipc';
import type { LspLocation, LspPosition } from '../protocol';
import { filePathFacet } from './lsp';
import { lspLanguageFor } from './lspLanguage';

/// Platform's "open the definition" modifier. `Meta` is Cmd on macOS
/// (and the Windows key elsewhere, but on Linux/Windows nobody
/// reaches for it with a mouse); `Control` is Ctrl. We lock to one
/// per OS to match the convention a user already uses in every other
/// editor — mixing them would mean a Ctrl+Click that does different
/// things on different machines.
const IS_MAC = typeof navigator !== 'undefined' && /Mac|iPod|iPhone|iPad/.test(navigator.platform);

/** True when `event` carries the platform's jump modifier. */
export function hasGotoModifier(event: MouseEvent | KeyboardEvent): boolean {
	return IS_MAC ? event.metaKey : event.ctrlKey;
}

/** KeyboardEvent.key values that toggle the modifier flag on/off. */
const MOD_KEY_NAMES = IS_MAC ? new Set(['Meta']) : new Set(['Control']);

// One-shot effect that replaces the link decoration (or clears it
// when passed `null`). Held in a small StateField below.
const setLinkEffect = StateEffect.define<{ from: number; to: number } | null>();

const linkMark = Decoration.mark({ class: 'cm-lsp-link' });

const linkField = StateField.define<DecorationSet>({
	create: () => Decoration.none,
	update: (decos, tr) => {
		// Remap through document changes so a decoration stays pinned
		// to its identifier if the user types elsewhere; a held
		// modifier shouldn't flicker on every keystroke.
		let next = decos.map(tr.changes);
		for (const eff of tr.effects) {
			if (eff.is(setLinkEffect)) {
				next = eff.value === null ? Decoration.none : Decoration.set([linkMark.range(eff.value.from, eff.value.to)]);
			}
		}
		return next;
	},
	provide: (f) => EditorView.decorations.from(f),
});

export type GotoDefinitionDeps = {
	jumpTo: (path: string, position: LspPosition) => void | Promise<void>;
	flash: (message: string) => void;
};

/**
 * Build the goto-definition extension. The returned bundle is:
 *
 * 1. A state field that stores the currently-decorated span (if any).
 * 2. A ViewPlugin that watches mouse + modifier state, probes LSP on
 *    hover, and jumps on modified click.
 */
export function lspGotoDefinitionExtension(deps: GotoDefinitionDeps): Extension {
	const plugin = ViewPlugin.fromClass(
		class {
			view: EditorView;
			modHeld = false;
			// Bumped on every mousemove probe. The async LSP callback
			// checks the value it captured against `probeEpoch` on
			// return; if it moved on, we drop the stale response.
			probeEpoch = 0;
			lastPos: number | null = null;
			// Last word we actually probed; lets us skip duplicate
			// IPC calls while the mouse lingers inside the same span.
			lastWord: { from: number; to: number } | null = null;

			constructor(view: EditorView) {
				this.view = view;
				// Key state lives at the window level — the user can
				// press Ctrl/Cmd with focus outside the editor
				// (e.g. toolbar hover) and we still want the next
				// mouse move to pick it up.
				window.addEventListener('keydown', this.onWindowKey);
				window.addEventListener('keyup', this.onWindowKey);
				window.addEventListener('blur', this.onBlur);
			}

			destroy() {
				window.removeEventListener('keydown', this.onWindowKey);
				window.removeEventListener('keyup', this.onWindowKey);
				window.removeEventListener('blur', this.onBlur);
				this.clearLink();
			}

			onWindowKey = (event: KeyboardEvent) => {
				if (!MOD_KEY_NAMES.has(event.key)) {
					return;
				}
				const held = hasGotoModifier(event);
				if (held === this.modHeld) {
					return;
				}
				this.modHeld = held;
				if (!held) {
					this.clearLink();
				}
			};

			// Focus loss out of the window = no way to tell when the
			// user releases the modifier (no keyup will fire). Drop
			// the link state to avoid a stale underline that stays
			// armed after the user has already moved on.
			onBlur = () => {
				this.modHeld = false;
				this.clearLink();
			};

			onMouseMove(event: MouseEvent) {
				const held = hasGotoModifier(event);
				this.modHeld = held;
				if (!held) {
					this.clearLink();
					return;
				}
				const pos = this.view.posAtCoords({ x: event.clientX, y: event.clientY });
				if (pos === null) {
					this.clearLink();
					return;
				}
				const word = this.view.state.wordAt(pos);
				if (!word) {
					this.clearLink();
					return;
				}
				// Inside the same word we already probed — nothing
				// new to ask the server for.
				if (this.lastWord && this.lastWord.from === word.from && this.lastWord.to === word.to) {
					return;
				}
				this.lastWord = { from: word.from, to: word.to };
				this.lastPos = pos;
				const path = this.view.state.facet(filePathFacet);
				if (path === null) {
					this.clearLink();
					return;
				}
				const languageId = lspLanguageFor(path);
				if (languageId === null) {
					this.clearLink();
					return;
				}
				const myEpoch = ++this.probeEpoch;
				const position = positionFor(this.view, pos);
				void this.probe(myEpoch, path, languageId, position, word);
			}

			async probe(
				epoch: number,
				path: string,
				languageId: string,
				position: LspPosition,
				word: { from: number; to: number },
			) {
				let location: LspLocation | null = null;
				try {
					location = await ipc.lsp.definition(path, languageId, position);
				} catch {
					// Swallow — the underline is a quiet affordance, not
					// a notification surface. A persistent outage shows
					// up via the status-bar pill instead.
					return;
				}
				// Raced: the pointer moved on while we were waiting,
				// so the answer no longer pertains to the current hover.
				if (epoch !== this.probeEpoch) {
					return;
				}
				if (!location) {
					this.clearLink();
					return;
				}
				// Self-jump: the server says the definition is the
				// identifier itself. No underline — there's nowhere
				// to go.
				if (location.path === path && rangesOverlap(location.range, position)) {
					this.clearLink();
					return;
				}
				this.view.dispatch({
					effects: setLinkEffect.of({ from: word.from, to: word.to }),
				});
			}

			onMouseUp(event: MouseEvent) {
				if (!hasGotoModifier(event)) {
					return;
				}
				// Primary button only. CM's default middle-click and
				// context-menu behaviours stay untouched.
				if (event.button !== 0) {
					return;
				}
				const pos = this.view.posAtCoords({ x: event.clientX, y: event.clientY });
				if (pos === null) {
					return;
				}
				const path = this.view.state.facet(filePathFacet);
				if (path === null) {
					return;
				}
				const languageId = lspLanguageFor(path);
				if (languageId === null) {
					return;
				}
				const position = positionFor(this.view, pos);
				event.preventDefault();
				event.stopPropagation();
				void this.jump(path, languageId, position);
			}

			async jump(path: string, languageId: string, position: LspPosition) {
				let location: LspLocation | null = null;
				try {
					location = await ipc.lsp.definition(path, languageId, position);
				} catch {
					deps.flash('Goto definition failed');
					return;
				}
				if (!location) {
					return;
				}
				if (location.path === '') {
					// External target — node_modules, toolchain source, or
					// a synthetic `ts://` URI for built-in types. No
					// read-only viewer yet, so surface the URI and move on.
					deps.flash(`Definition outside workspace: ${location.externalUri}`);
					return;
				}
				this.clearLink();
				await deps.jumpTo(location.path, location.range.start);
			}

			private clearLink() {
				this.lastWord = null;
				if (this.view.state.field(linkField, false)?.size === 0) {
					return;
				}
				this.view.dispatch({ effects: setLinkEffect.of(null) });
			}
		},
		{
			eventHandlers: {
				mousemove(event) {
					this.onMouseMove(event);
				},
				mouseleave() {
					this.modHeld = false;
					this.lastWord = null;
					this.view.dispatch({ effects: setLinkEffect.of(null) });
				},
				mouseup(event) {
					this.onMouseUp(event);
				},
			},
		},
	);
	return [linkField, plugin];
}

/// Convert a CM offset to an LSP position (line / UTF-16 character).
/// Duplicates `lsp.ts`'s private helper — shared helper is not yet
/// warranted for two call sites.
function positionFor(view: EditorView, offset: number): LspPosition {
	const line = view.state.doc.lineAt(offset);
	return {
		line: line.number - 1,
		character: offset - line.from,
	};
}

/// True when `range` contains the single position `pos` — used to
/// suppress the underline on the identifier's own declaration (the
/// LSP returns the declaration span as the definition of a
/// declaration, which would otherwise flag every `function foo` as
/// "jump-able" for its own name).
function rangesOverlap(range: { start: LspPosition; end: LspPosition }, pos: LspPosition): boolean {
	const afterStart =
		pos.line > range.start.line || (pos.line === range.start.line && pos.character >= range.start.character);
	const beforeEnd = pos.line < range.end.line || (pos.line === range.end.line && pos.character <= range.end.character);
	return afterStart && beforeEnd;
}
