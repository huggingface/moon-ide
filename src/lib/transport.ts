// The one seam between the UI and the workspace process (ADR 0095).
// Today the workspace process is always in-process (Tauri); a remote
// workspace swaps the implementation behind `call` / `subscribe` /
// `fileUrl` without touching any caller.

import { convertFileSrc, invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

export type Unsubscribe = () => void;

/** Call a command on the shared workspace-process surface (`moon-app`). */
export function call<T>(method: string, params?: Record<string, unknown>): Promise<T> {
	return invoke<T>('rpc', { method, params: params ?? {} });
}

/**
 * Call a Tauri command on this desktop process: desktop-shell actions
 * (windows, dialogs, opener), plus commands not yet moved onto the
 * shared surface (Phase 16.2).
 */
export function local<T>(command: string, args?: Record<string, unknown>): Promise<T> {
	return invoke<T>(command, args);
}

/**
 * Subscribe to a push event from the workspace process. The handler
 * annotates its own payload type; `never` lets any typed handler in,
 * since the wire can't check the shape either way.
 */
export function subscribe(event: string, handler: (payload: never) => void): Promise<Unsubscribe> {
	return listen<never>(event, ({ payload }) => handler(payload));
}

/** URL the webview can load a workspace file's bytes from (image / PDF previews). */
export function fileUrl(absolutePath: string): string {
	return convertFileSrc(absolutePath);
}
