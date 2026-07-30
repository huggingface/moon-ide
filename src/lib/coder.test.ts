import { describe, expect, it } from 'vitest';

import { CoderPanelState, renderPromptWithAttachments, type TerminalAttachment } from './coder.svelte';
import type { CoderSessionSummary } from './protocol';

const PARENT = '/repo';
const WORKTREE = '/repo/.worktrees/agent-abc';

function summary(id: string, worktreeRoot: string | null): CoderSessionSummary {
	return {
		id,
		title: id,
		created_at_ms: 0,
		updated_at_ms: 0,
		worktree_root: worktreeRoot,
	};
}

describe('folder-bar agent-status rollups (ADR 0028)', () => {
	it('keeps a worktree session off the parent row and on the worktree row', () => {
		const coder = new CoderPanelState();
		const bucket = coder.bucketFor(PARENT);
		bucket.sessions = [summary('wt-session', WORKTREE)];

		const session = coder.sessionStateFor(PARENT, 'wt-session');
		session.busy = true;
		session.awaitingInput = true;
		session.attentionPending = true;

		expect(coder.busyForFolder(PARENT)).toBe(false);
		expect(coder.awaitingInputForFolder(PARENT)).toBe(false);
		expect(coder.attentionPendingForFolder(PARENT)).toBe(false);

		expect(coder.busyForWorktree(PARENT, WORKTREE)).toBe(true);
		expect(coder.awaitingInputForWorktree(PARENT, WORKTREE)).toBe(true);
		expect(coder.attentionPendingForWorktree(PARENT, WORKTREE)).toBe(true);
	});

	it('shows parent-rooted sessions on the parent row only', () => {
		const coder = new CoderPanelState();
		const bucket = coder.bucketFor(PARENT);
		bucket.sessions = [summary('main-session', null)];

		const session = coder.sessionStateFor(PARENT, 'main-session');
		session.busy = true;
		session.awaitingInput = true;
		session.attentionPending = true;

		expect(coder.busyForFolder(PARENT)).toBe(true);
		expect(coder.awaitingInputForFolder(PARENT)).toBe(true);
		expect(coder.attentionPendingForFolder(PARENT)).toBe(true);

		expect(coder.busyForWorktree(PARENT, WORKTREE)).toBe(false);
		expect(coder.awaitingInputForWorktree(PARENT, WORKTREE)).toBe(false);
		expect(coder.attentionPendingForWorktree(PARENT, WORKTREE)).toBe(false);
	});

	it('shows both glyphs when both contexts have sessions in the same state', () => {
		const coder = new CoderPanelState();
		const bucket = coder.bucketFor(PARENT);
		bucket.sessions = [summary('main-session', null), summary('wt-session', WORKTREE)];

		coder.sessionStateFor(PARENT, 'main-session').busy = true;
		coder.sessionStateFor(PARENT, 'wt-session').busy = true;

		expect(coder.busyForFolder(PARENT)).toBe(true);
		expect(coder.busyForWorktree(PARENT, WORKTREE)).toBe(true);
	});

	it('falls back to activeSession when the sessions list has not loaded', () => {
		const coder = new CoderPanelState();
		const session = coder.sessionStateFor(PARENT, 'wt-session');
		session.activeSession = summary('wt-session', WORKTREE);
		session.busy = true;

		expect(coder.busyForFolder(PARENT)).toBe(false);
		expect(coder.busyForWorktree(PARENT, WORKTREE)).toBe(true);
	});
});

function terminalAttachment(overrides: Partial<TerminalAttachment> = {}): TerminalAttachment {
	return {
		kind: 'terminal',
		id: 'att-1',
		token: '@terminal:moon-ide',
		text: 'error: address already in use',
		label: 'moon-ide',
		lineCount: 1,
		terminalId: 'stream-abc',
		...overrides,
	};
}

describe('terminal attachments carry their source terminal (ADR 0048)', () => {
	it('names the terminal so the model can re-read it with read_terminal', () => {
		const out = renderPromptWithAttachments('why?', [], [terminalAttachment()]);
		expect(out).toContain('terminal_id="stream-abc"');
		expect(out).toContain('<terminal_output token="@terminal:moon-ide" label="moon-ide"');
	});

	it('omits the attribute rather than emitting an empty one when the terminal is unknown', () => {
		const out = renderPromptWithAttachments('why?', [], [terminalAttachment({ terminalId: null })]);
		expect(out).not.toContain('terminal_id');
		expect(out).toContain('<terminal_output token="@terminal:moon-ide" label="moon-ide">');
	});
});
