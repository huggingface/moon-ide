<script lang="ts">
	import { tick } from 'svelte';
	import { app, type AskUserQuestion } from './app.svelte';
	import Markdown from './Markdown.svelte';

	let draft = $state('');

	// User-row id whose action chips (edit & resend / replay) are
	// showing. Tap a user bubble to toggle; any action clears it.
	let actionsFor = $state<string | null>(null);

	function toggleActions(rowId: string): void {
		actionsFor = actionsFor === rowId ? null : rowId;
	}

	async function editAndResend(rowId: string): Promise<void> {
		actionsFor = null;
		const text = await app.revertToMessage(rowId);
		if (text !== null) {
			draft = text;
		}
	}

	async function replayFrom(rowId: string): Promise<void> {
		actionsFor = null;
		await app.replayFromMessage(rowId);
	}

	// Per-question answer state for the active ask_user prompt.
	// Map of questionId → { selected: Set<string>, freeText: string }
	let answers = $state<Record<string, { selected: Set<string>; freeText: string }>>({});

	async function send(): Promise<void> {
		const text = draft.trim();
		if (!text) {
			return;
		}
		draft = '';
		await app.sendPrompt(text);
	}

	function onKeydown(e: KeyboardEvent): void {
		// Enter sends; Shift+Enter newline (matches the desktop composer).
		if (e.key === 'Enter' && !e.shiftKey) {
			e.preventDefault();
			void send();
		}
	}

	function toggleOption(qId: string, optId: string, multi: boolean): void {
		if (!answers[qId]) {
			answers[qId] = { selected: new Set(), freeText: '' };
		}
		if (multi) {
			if (answers[qId].selected.has(optId)) {
				answers[qId].selected.delete(optId);
			} else {
				answers[qId].selected.add(optId);
			}
		} else {
			// Single-select: clear and set.
			answers[qId].selected.clear();
			answers[qId].selected.add(optId);
		}
		// Trigger reactivity.
		answers = { ...answers };
	}

	async function submitPrompt(): Promise<void> {
		if (!app.pendingPrompt) {
			return;
		}
		const response = app.pendingPrompt.questions.map((q) => ({
			question_id: q.id,
			selected: [...(answers[q.id]?.selected ?? [])],
			free_text: answers[q.id]?.freeText ?? '',
		}));
		await app.respondToPrompt(app.pendingPrompt.callId, response);
		answers = {};
	}

	// Single-select auto-submits when an option is clicked.
	function clickOption(q: AskUserQuestion, optId: string): void {
		toggleOption(q.id, optId, q.multi);
		if (!q.multi) {
			void submitPrompt();
		}
	}

	function truncate(s: string, max: number): string {
		return s.length > max ? s.slice(0, max) + '...' : s;
	}

	function short(s: string, n = 40): string {
		return s.length > n ? s.slice(0, n) + '…' : s;
	}

	/** Build a human-readable summary line for a tool call from its
	 * name + raw JSON args string. Falls back to the tool name alone
	 * when args are missing or unparseable. */
	function toolSummary(name: string, argsStr: string): string {
		if (!argsStr) {
			return name;
		}
		let args: Record<string, unknown>;
		try {
			args = JSON.parse(argsStr) as Record<string, unknown>;
		} catch {
			return name;
		}
		const p = (k: string): string => (typeof args[k] === 'string' ? (args[k] as string) : '');
		switch (name) {
			case 'read_file': {
				const path = p('path') || 'file';
				const range =
					p('start_line') || p('end_line') ? `:${p('start_line') || 1}${p('end_line') ? '-' + p('end_line') : ''}` : '';
				return `read ${path}${range}`;
			}
			case 'write_file':
				return `write ${p('path') || 'file'}`;
			case 'edit_file': {
				const find = short(p('find'), 30);
				return `edit ${p('path') || 'file'} — "${find}"`;
			}
			case 'list_dir':
				return `list ${p('path') || 'dir'}`;
			case 'grep':
				return `grep "${short(p('pattern'), 30)}"`;
			case 'bash': {
				const cmd = p('cmd');
				if (!cmd) {
					return 'bash';
				}
				const firstLine = cmd.split('\n')[0] ?? '';
				return `$ ${short(firstLine, 50)}`;
			}
			case 'web_search':
				return `search "${short(p('query'), 30)}"`;
			case 'web_fetch':
				return p('url') ? short(p('url'), 50) : 'web fetch';
			case 'todo_write':
				return 'update todo';
			case 'ask_user':
				return 'ask user';
			case 'read_process':
				return `poll ${p('id') || 'process'}`;
			case 'stop_process':
				return `stop ${p('id') || 'process'}`;
			default:
				return name;
		}
	}

	/** Structured tool body content for tools where a simple
	 * text preview isn't enough — `edit_file` shows a diff-like
	 * find→replace view, `write_file` shows a content preview.
	 * Returns `null` when the tool doesn't have structured body
	 * (caller falls through to `toolResultPreview`). */
	type ToolBody = { kind: 'diff'; find: string; replace: string } | { kind: 'content'; text: string };

	function toolBody(name: string, argsStr: string, _result: string): ToolBody | null {
		let args: Record<string, unknown>;
		try {
			args = JSON.parse(argsStr) as Record<string, unknown>;
		} catch {
			return null;
		}
		const p = (k: string): string => (typeof args[k] === 'string' ? (args[k] as string) : '');
		if (name === 'edit_file') {
			const find = p('find');
			const replace = p('replace');
			if (!find && !replace) {
				return null;
			}
			return { kind: 'diff', find, replace };
		}
		if (name === 'write_file') {
			const content = p('content');
			if (!content) {
				return null;
			}
			return { kind: 'content', text: content };
		}
		return null;
	}

	/** Extract the most useful preview from a tool result string.
	 * Tool-specific parsing where the JSON shape is known, falling
	 * back to a plain-text truncation. */
	function toolResultPreview(name: string, result: string): string {
		if (!result) {
			return '';
		}
		// bash returns {cmd, target, exit_code, stdout, stderr}.
		if (name === 'bash') {
			try {
				const parsed = JSON.parse(result) as Record<string, unknown>;
				const stdout = typeof parsed['stdout'] === 'string' ? (parsed['stdout'] as string) : '';
				const stderr = typeof parsed['stderr'] === 'string' ? (parsed['stderr'] as string) : '';
				const code = typeof parsed['exit_code'] === 'number' ? parsed['exit_code'] : null;
				const out = stdout || stderr || '';
				const prefix = code !== null && code !== 0 ? `[exit ${code}] ` : '';
				return truncate(prefix + out, 300);
			} catch {
				return truncate(result, 300);
			}
		}
		// read_file returns line-prefixed content; show a short
		// snippet without the line-number prefixes.
		if (name === 'read_file') {
			const lines = result.split('\n').slice(0, 5);
			const stripped = lines.map((l) => l.replace(/^\d+\|/, '')).join('\n');
			return truncate(stripped, 300);
		}
		// grep returns `path:line: match` lines; show the first few.
		if (name === 'grep') {
			const lines = result.split('\n').filter((l) => l.trim());
			const count = lines.length;
			if (count === 0) {
				return 'no matches';
			}
			return truncate(`${count} match${count === 1 ? '' : 'es'}\n${lines.slice(0, 3).join('\n')}`, 300);
		}
		// Generic fallback: try JSON, then plain text.
		try {
			const parsed = JSON.parse(result);
			if (typeof parsed === 'string') {
				return truncate(parsed, 200);
			}
			if (parsed && typeof parsed === 'object') {
				if (typeof parsed['error'] === 'string') {
					return truncate(parsed['error'], 200);
				}
				if (typeof parsed['content'] === 'string') {
					return truncate(parsed['content'], 200);
				}
				if (typeof parsed['output'] === 'string') {
					return truncate(parsed['output'], 200);
				}
			}
		} catch {
			// Not JSON — treat as plain text.
		}
		return truncate(result, 200);
	}

	const title = $derived(app.sessions.find((s) => s.id === app.activeSession)?.title ?? '');
	const isCoordinator = $derived(app.sessions.find((s) => s.id === app.activeSession)?.mode === 'coordinator');

	// Pin the transcript to the bottom while rows stream in, unless
	// the user scrolled away to read — same gesture as the desktop's
	// CoderThinking body (within-24px threshold absorbs subpixel
	// scroll positions on HiDPI).
	const PIN_THRESHOLD_PX = 24;
	let transcriptEl = $state<HTMLDivElement>();
	let pinned = $state(true);

	// Trailing render window over `app.rows` — the phone-sized
	// mirror of the desktop transcript windowing (test plan 0093).
	// Opening a long session mounts only the last `INITIAL_WINDOW`
	// rows; scrolling near the top (or the "Load older" pill) pulls
	// more in with a scroll anchor so nothing lurches. The mounted
	// count is capped at `WINDOW_MAX`: past it the window *slides*,
	// clipping rows off the (off-screen) bottom and detaching from
	// the live tail — "Load newer" / "Jump to latest" reel it back.
	const INITIAL_WINDOW = 50;
	const WINDOW_GROW_STEP = 50;
	const WINDOW_MAX = 300;
	const LOAD_MORE_THRESHOLD_PX = 600;

	let visibleCount = $state(INITIAL_WINDOW);
	let bottomClip = $state(0);

	const windowEnd = $derived(Math.max(0, app.rows.length - bottomClip));
	const windowStart = $derived(Math.max(0, windowEnd - visibleCount));
	const windowedRows = $derived(app.rows.slice(windowStart, windowEnd));
	const hiddenAbove = $derived(windowStart);
	const hiddenBelow = $derived(app.rows.length - windowEnd);

	// Scroll anchoring for a window change: pin a concrete edge row
	// element + its viewport-relative top before the slice changes,
	// then nudge `scrollTop` by how far it moved once the DOM
	// settles. Works for prepend, clip, and cap-slide alike.
	let pendingAnchorNode: HTMLElement | null = null;
	let pendingAnchorNodeTop = 0;
	// Set while applying the anchor's programmatic scroll so the
	// synthetic scroll event doesn't re-trigger a grow and cascade
	// the whole history in.
	let applyingAnchor = false;

	function isAnchorRow(child: Element): child is HTMLElement {
		return (
			child instanceof HTMLElement && !child.classList.contains('load-pill') && !child.classList.contains('jump-latest')
		);
	}

	function edgeRowEl(edge: 'first' | 'last'): HTMLElement | null {
		if (!transcriptEl) {
			return null;
		}
		const children = Array.from(transcriptEl.children).filter(isAnchorRow);
		if (children.length === 0) {
			return null;
		}
		return edge === 'first' ? children[0]! : children[children.length - 1]!;
	}

	function captureScrollAnchor(edge: 'first' | 'last' = 'first'): void {
		const el = edgeRowEl(edge);
		if (el && transcriptEl) {
			pendingAnchorNode = el;
			pendingAnchorNodeTop = el.getBoundingClientRect().top - transcriptEl.getBoundingClientRect().top;
		}
	}

	/** Pull older rows into the window; at the cap, slide instead
	 *  (drop the same count off the off-screen bottom). */
	function growWindowUp(): void {
		if (hiddenAbove <= 0) {
			return;
		}
		const step = Math.min(WINDOW_GROW_STEP, hiddenAbove);
		if (visibleCount + step <= WINDOW_MAX) {
			visibleCount += step;
			return;
		}
		visibleCount = WINDOW_MAX;
		bottomClip += step;
	}

	/** Mirror of `growWindowUp` for a detached bottom edge. */
	function growWindowDown(): void {
		if (hiddenBelow <= 0) {
			return;
		}
		const step = Math.min(WINDOW_GROW_STEP, hiddenBelow);
		bottomClip -= step;
		visibleCount = Math.min(visibleCount + step, WINDOW_MAX);
	}

	function loadOlderRows(): void {
		captureScrollAnchor('first');
		growWindowUp();
	}

	function loadNewerRows(): void {
		captureScrollAnchor('last');
		growWindowDown();
	}

	/** Snap the window back to the live tail and scroll to the
	 *  bottom — the escape hatch from a detached window. */
	function jumpToLatest(): void {
		bottomClip = 0;
		visibleCount = INITIAL_WINDOW;
		pendingAnchorNode = null;
		pinned = true;
		void tick().then(() => {
			if (transcriptEl) {
				transcriptEl.scrollTop = transcriptEl.scrollHeight;
			}
		});
	}

	function onTranscriptScroll(): void {
		const el = transcriptEl;
		if (!el) {
			return;
		}
		const distance = el.scrollHeight - el.scrollTop - el.clientHeight;
		// Only sticky when the window is anchored to the live tail;
		// a detached window's bottom edge is not the latest row.
		pinned = bottomClip === 0 && distance <= PIN_THRESHOLD_PX;
		if (applyingAnchor) {
			return;
		}
		if (el.scrollTop <= LOAD_MORE_THRESHOLD_PX && hiddenAbove > 0) {
			loadOlderRows();
		} else if (distance <= LOAD_MORE_THRESHOLD_PX && hiddenBelow > 0) {
			loadNewerRows();
		}
	}

	// Apply the captured scroll anchor after the windowed slice
	// re-renders.
	$effect(() => {
		void windowedRows;
		if (pendingAnchorNode === null) {
			return;
		}
		const node = pendingAnchorNode;
		const prevTop = pendingAnchorNodeTop;
		pendingAnchorNode = null;
		void tick().then(() => {
			if (!transcriptEl || !node.isConnected) {
				return;
			}
			const newTop = node.getBoundingClientRect().top - transcriptEl.getBoundingClientRect().top;
			const delta = newTop - prevTop;
			if (delta !== 0) {
				applyingAnchor = true;
				transcriptEl.scrollTop += delta;
				requestAnimationFrame(() => {
					applyingAnchor = false;
				});
			}
		});
	});

	let lastRowCount = 0;
	$effect(() => {
		const count = app.rows.length;
		if (count < lastRowCount) {
			// The transcript was reset (reconnect re-replays into an
			// emptied list): snap the window back to the tail and
			// re-arm sticky-bottom.
			pinned = true;
			visibleCount = INITIAL_WINDOW;
			bottomClip = 0;
			pendingAnchorNode = null;
		}
		const appended = count - lastRowCount;
		lastRowCount = count;
		const el = transcriptEl;
		if (!el) {
			return;
		}
		if (!pinned) {
			// Reading history: clip new arrivals off the bottom so
			// nothing on screen moves; "Jump to latest" appears.
			if (appended > 0) {
				bottomClip += appended;
			}
			return;
		}
		el.scrollTop = el.scrollHeight;
	});
</script>

<div class="session">
	<div class="row session-head">
		<button class="ghost back" onclick={() => app.closeSession()}>←</button>
		{#if isCoordinator}<span class="coord-badge" title="Coordinator — orchestrates worker agents">coord</span>{/if}
		<strong class="session-title">{title || 'Untitled session'}</strong>
		{#if app.busy}
			<span class="pip live" title="Running"></span>
		{:else if app.awaitingInput}
			<span class="pip" style="background: var(--accent)" title="Input needed"></span>
		{/if}
	</div>

	{#if app.tokenUsage}
		<div
			class="token-bar"
			title="{app.tokenUsage.total.toLocaleString()} / {app.tokenUsage.contextWindow.toLocaleString()} tokens"
		>
			<span class="token-pct">{app.tokenUsage.pct}%</span>
			<div class="token-meter">
				<div class="token-fill" style="width: {Math.min(100, app.tokenUsage.pct)}%"></div>
			</div>
			<span class="token-detail"
				>{app.tokenUsage.total.toLocaleString()} / {app.tokenUsage.contextWindow.toLocaleString()}</span
			>
		</div>
	{/if}

	<div class="transcript" bind:this={transcriptEl} onscroll={onTranscriptScroll}>
		{#if hiddenAbove > 0}
			<button type="button" class="load-pill" onclick={loadOlderRows}>
				Load {Math.min(WINDOW_GROW_STEP, hiddenAbove)} older ({hiddenAbove} hidden)
			</button>
		{/if}
		{#each windowedRows as row (row.kind + row.id)}
			{#if row.kind === 'user'}
				<div
					class="bubble user"
					class:queued={row.queued}
					class:actionable={!app.busy && !row.queued}
					role="button"
					tabindex="0"
					onclick={() => toggleActions(row.id)}
					onkeydown={(e) => {
						if (e.key === 'Enter' || e.key === ' ') {
							e.preventDefault();
							toggleActions(row.id);
						}
					}}
				>
					{row.text}
					{#if row.queued}<span class="queued-tag">queued</span>{/if}
				</div>
				{#if actionsFor === row.id && !app.busy && !row.queued}
					<div class="user-actions">
						<button
							class="ghost action-chip"
							onclick={() => editAndResend(row.id)}
							title="Drop this message and everything after it; put the text back in the composer"
							>✎ Edit & resend</button
						>
						<button
							class="ghost action-chip"
							onclick={() => replayFrom(row.id)}
							title="Drop this message and everything after it, then re-send the same prompt">↻ Replay</button
						>
					</div>
				{/if}
			{:else if row.kind === 'assistant'}
				{#if row.thinking}
					<details class="thinking">
						<summary>Thinking…</summary>
						<div class="thinking-body">{row.thinking}</div>
					</details>
				{/if}
				{#if row.text}
					<div class="bubble assistant"><Markdown text={row.text} /></div>
				{/if}
			{:else if row.kind === 'tool'}
				{@const body = toolBody(row.name, row.args, row.result)}
				<details class="tool" class:error={row.status === 'error'}>
					<summary>
						<span class="pip" class:live={row.status === 'running'}></span>
						<span class="tool-name">{toolSummary(row.name, row.args)}</span>
						{#if row.status === 'done'}<span class="tool-check">✓</span>{:else if row.status === 'error'}<span
								class="tool-check">✗</span
							>{/if}
					</summary>
					{#if body?.kind === 'diff'}
						<div class="tool-diff">
							{#if body.find}
								<pre class="diff-old">{truncate(body.find, 400)}</pre>
							{/if}
							{#if body.replace}
								<pre class="diff-new">{truncate(body.replace, 400)}</pre>
							{/if}
						</div>
					{:else if body?.kind === 'content'}
						<pre class="tool-content">{truncate(body.text, 400)}</pre>
					{:else if row.result}
						<div class="tool-result-preview">{toolResultPreview(row.name, row.result)}</div>
					{/if}
				</details>
			{:else if row.kind === 'ask_user'}
				<div class="ask-user" class:answered={row.answered}>
					{#if !row.answered && app.pendingPrompt?.callId === row.callId}
						{#each app.pendingPrompt.questions as q (q.id)}
							<div class="question">
								<p class="question-text">{q.question}</p>
								<div class="options">
									{#each q.options as opt (opt.id)}
										<button
											type="button"
											class="option"
											class:selected={answers[q.id]?.selected.has(opt.id) ?? false}
											onclick={() => clickOption(q, opt.id)}
										>
											{opt.label}
										</button>
									{/each}
								</div>
								<input
									type="text"
									class="free-text"
									placeholder="Other…"
									value={answers[q.id]?.freeText ?? ''}
									oninput={(e) => {
										const cur = answers[q.id] ?? { selected: new Set(), freeText: '' };
										cur.freeText = (e.target as HTMLInputElement).value;
										answers[q.id] = cur;
										answers = { ...answers };
									}}
								/>
							</div>
						{/each}
						{#if app.pendingPrompt.questions.some((q) => q.multi)}
							<button type="button" class="primary" onclick={submitPrompt}>Submit</button>
						{/if}
					{:else}
						<p class="muted">
							{#if row.answered}Answered{:else}Waiting for response…{/if}
						</p>
					{/if}
				</div>
			{:else if row.kind === 'diff'}
				<details class="diff">
					<summary>{row.files.length} file{row.files.length !== 1 ? 's' : ''} changed</summary>
					{#each row.files as f}
						<div class="diff-file">{f}</div>
					{/each}
					{#if row.diff}
						<pre class="diff-body">{truncate(row.diff, 1000)}</pre>
					{/if}
				</details>
			{:else if row.kind === 'compaction'}
				<div class="compaction">
					{#if row.done}
						<details>
							<summary>Context compacted</summary>
							<div class="muted">{row.summary}</div>
						</details>
					{:else}
						<span class="muted">Compacting context…</span>
					{/if}
				</div>
			{:else if row.kind === 'subagent'}
				<div class="subagent" class:finished={row.finished}>
					<span class="pip" class:live={!row.finished}></span>
					Sub-agent {#if row.folder}in {row.folder}{/if}
					{#if row.finished}✓{:else}running…{/if}
				</div>
			{/if}
		{/each}
		{#if hiddenBelow > 0}
			<button type="button" class="load-pill" onclick={loadNewerRows}>
				Load {Math.min(WINDOW_GROW_STEP, hiddenBelow)} newer ({hiddenBelow} below)
			</button>
			<button type="button" class="jump-latest" onclick={jumpToLatest}>Jump to latest ↓</button>
		{/if}
		{#if app.rows.length === 0}
			{#if isCoordinator}
				<div class="empty-hint">
					<p><strong>Coordinator session</strong></p>
					<p class="muted">
						An orchestrator that spawns and manages worker agents. It can't edit files itself — it delegates each task
						to a worker in its own git worktree.
					</p>
					<p class="muted">
						Describe a goal (e.g. <em>look at the open GitHub issues and open a PR for each</em>) and it will decompose
						it into worker tasks.
					</p>
				</div>
			{:else}
				<p class="muted">No messages yet. Send one below.</p>
			{/if}
		{/if}
	</div>

	<div class="composer">
		<textarea
			bind:value={draft}
			onkeydown={onKeydown}
			placeholder={isCoordinator
				? 'Describe a goal for the coordinator — Enter to send'
				: 'Message the coder — Enter to send'}
			rows="2"
		></textarea>
		{#if app.busy}
			<button class="ghost" onclick={() => app.abort()}>Stop</button>
		{:else}
			<button class="primary" onclick={send} disabled={!draft.trim()}>Send</button>
		{/if}
	</div>
</div>

<style>
	.session {
		display: flex;
		flex-direction: column;
		/* dvh tracks the mobile browser chrome collapsing/expanding;
		   vh alone leaves the composer hidden behind the toolbar. */
		height: 100dvh;
		padding: 0.75rem;
		gap: 0.5rem;
	}
	.session-head {
		gap: 0.5rem;
	}
	.back {
		flex: none;
		padding: 0.6rem 0.7rem;
	}
	.session-title {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-size: 0.95rem;
	}
	.transcript {
		flex: 1;
		overflow-y: auto;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding: 0.25rem;
	}
	.load-pill {
		flex: none;
		align-self: center;
		background: var(--bg-elev);
		border: 1px solid var(--border);
		border-radius: 999px;
		color: var(--fg-muted);
		font-size: 0.8rem;
		padding: 0.4rem 0.9rem;
	}
	.jump-latest {
		position: sticky;
		bottom: 0.25rem;
		flex: none;
		align-self: center;
		background: var(--bg-elev-2);
		border: 1px solid var(--accent);
		border-radius: 999px;
		color: var(--fg);
		font-size: 0.8rem;
		padding: 0.4rem 0.9rem;
		box-shadow: 0 2px 8px rgb(0 0 0 / 40%);
	}
	.bubble {
		padding: 0.5rem 0.7rem;
		border-radius: var(--radius);
		word-break: break-word;
	}
	.bubble.user {
		background: var(--bg-elev-2);
		align-self: flex-end;
		max-width: 85%;
		/* User text is plain (not markdown) — keep typed newlines. */
		white-space: pre-wrap;
	}
	.bubble.user.queued {
		opacity: 0.6;
	}
	.bubble.user.actionable {
		cursor: pointer;
	}
	.user-actions {
		display: flex;
		gap: 0.4rem;
		justify-content: flex-end;
		margin-top: -0.2rem;
	}
	.action-chip {
		font-size: 0.75rem;
		padding: 0.25rem 0.6rem;
		border: 1px solid var(--border);
		border-radius: 999px;
	}
	.queued-tag {
		font-size: 0.7rem;
		margin-left: 0.3rem;
		color: var(--fg-muted);
	}
	.bubble.assistant {
		background: var(--bg-elev);
		border: 1px solid var(--border);
	}
	.thinking {
		font-size: 0.8rem;
		color: var(--fg-muted);
	}
	.thinking summary {
		cursor: pointer;
		font-style: italic;
	}
	.thinking-body {
		white-space: pre-wrap;
		padding: 0.3rem 0;
	}
	.tool {
		font-size: 0.8rem;
		color: var(--fg-muted);
	}
	.tool summary {
		cursor: pointer;
		display: flex;
		align-items: center;
		gap: 0.4rem;
	}
	.tool.error {
		color: var(--danger);
	}
	.tool-name {
		font-family: var(--mono, monospace);
		font-size: 0.75rem;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.tool-check {
		flex: none;
		font-size: 0.7rem;
	}
	.tool-result-preview {
		font-size: 0.72rem;
		font-family: var(--mono, monospace);
		color: var(--fg);
		margin: 0.25rem 0 0.25rem 1rem;
		padding: 0.25rem 0.4rem;
		background: var(--bg-elev);
		border-radius: var(--radius);
		white-space: pre-wrap;
		word-break: break-word;
		max-height: 120px;
		overflow-y: auto;
	}
	.tool-diff {
		margin: 0.25rem 0 0.25rem 1rem;
		display: flex;
		flex-direction: column;
		gap: 1px;
	}
	.diff-old,
	.diff-new {
		font-size: 0.72rem;
		font-family: var(--mono, monospace);
		margin: 0;
		padding: 0.25rem 0.4rem;
		white-space: pre-wrap;
		word-break: break-word;
		max-height: 150px;
		overflow-y: auto;
		border-radius: var(--radius);
	}
	.diff-old {
		background: rgba(248, 81, 73, 0.12);
		border-left: 3px solid #f85149;
		color: #ffa19b;
	}
	.diff-new {
		background: rgba(63, 185, 80, 0.1);
		border-left: 3px solid #3fb950;
		color: #7ee78c;
	}
	.tool-content {
		font-size: 0.72rem;
		font-family: var(--mono, monospace);
		margin: 0.25rem 0 0.25rem 1rem;
		padding: 0.25rem 0.4rem;
		background: var(--bg-elev);
		border-radius: var(--radius);
		white-space: pre-wrap;
		word-break: break-word;
		max-height: 200px;
		overflow-y: auto;
	}
	.ask-user {
		background: var(--bg-elev);
		border: 1px solid var(--accent);
		border-radius: var(--radius);
		padding: 0.6rem;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
	}
	.ask-user.answered {
		border-color: var(--border);
		opacity: 0.7;
	}
	.question {
		display: flex;
		flex-direction: column;
		gap: 0.3rem;
	}
	.question-text {
		font-weight: 600;
		margin: 0;
	}
	.options {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
	}
	.option {
		background: none;
		border: 1px solid var(--border);
		border-radius: var(--radius);
		color: var(--fg);
		cursor: pointer;
		padding: 0.4rem 0.5rem;
		font-size: 0.85rem;
		text-align: left;
	}
	.option.selected {
		border-color: var(--accent);
		background: var(--bg-elev-2);
	}
	.free-text {
		background: var(--bg-elev-2);
		border: 1px solid var(--border);
		border-radius: var(--radius);
		color: var(--fg);
		padding: 0.3rem 0.5rem;
		font-size: 0.85rem;
		font: inherit;
	}
	.diff {
		font-size: 0.8rem;
	}
	.diff summary {
		cursor: pointer;
		color: var(--fg-muted);
	}
	.diff-file {
		font-family: var(--mono, monospace);
		font-size: 0.75rem;
		padding: 0.1rem 0;
	}
	.diff-body {
		font-size: 0.7rem;
		white-space: pre-wrap;
		word-break: break-all;
		margin-top: 0.3rem;
		max-height: 250px;
		overflow-y: auto;
	}
	.token-bar {
		flex: none;
		display: flex;
		align-items: center;
		gap: 0.4rem;
		padding: 0.25rem 0.6rem;
		background: var(--bg-elev);
		border-bottom: 1px solid var(--border);
		font-size: 0.65rem;
		color: var(--fg-muted);
	}
	.token-pct {
		flex: none;
		font-weight: 600;
		min-width: 2.5rem;
	}
	.token-meter {
		flex: 1;
		height: 4px;
		background: var(--border);
		border-radius: 2px;
		overflow: hidden;
	}
	.token-fill {
		height: 100%;
		background: var(--accent);
		border-radius: 2px;
		transition: width 0.3s ease;
	}
	.token-detail {
		flex: none;
		font-family: var(--mono, monospace);
		font-size: 0.6rem;
	}
	.compaction {
		font-size: 0.8rem;
	}
	.subagent {
		font-size: 0.8rem;
		color: var(--fg-muted);
		display: flex;
		align-items: center;
		gap: 0.4rem;
	}
	.composer {
		display: flex;
		gap: 0.5rem;
		align-items: flex-end;
	}
	.coord-badge {
		flex: none;
		font-size: 0.65rem;
		font-weight: 600;
		padding: 0.15em 0.4em;
		border-radius: 999px;
		background: var(--accent);
		color: var(--accent-fg, #fff);
		text-transform: uppercase;
		letter-spacing: 0.03em;
	}
	.empty-hint {
		padding: 1rem 0.5rem;
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
	}
	.empty-hint p {
		margin: 0;
		line-height: 1.4;
	}
	.empty-hint .muted {
		font-size: 0.85rem;
	}
	.composer textarea {
		flex: 1;
		resize: none;
		font: inherit;
		background: var(--bg-elev);
		color: var(--fg);
		border: 1px solid var(--border);
		border-radius: var(--radius);
		padding: 0.5rem;
	}
</style>
