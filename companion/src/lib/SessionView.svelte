<script lang="ts">
	import { app, type AskUserQuestion } from './app.svelte';

	let draft = $state('');

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
</script>

<div class="session">
	<div class="row session-head">
		<button class="ghost" onclick={() => app.closeSession()}>← Sessions</button>
		{#if app.busy}
			<span class="muted">running…</span>
		{:else if app.awaitingInput}
			<span class="muted" style="color: var(--accent)">input needed</span>
		{/if}
	</div>

	<div class="transcript">
		{#each app.rows as row (row.kind + row.id)}
			{#if row.kind === 'user'}
				<div class="bubble user" class:queued={row.queued}>
					{row.text}
					{#if row.queued}<span class="queued-tag">queued</span>{/if}
				</div>
			{:else if row.kind === 'assistant'}
				{#if row.thinking}
					<details class="thinking">
						<summary>Thinking…</summary>
						<div class="thinking-body">{row.thinking}</div>
					</details>
				{/if}
				{#if row.text}
					<div class="bubble assistant">{row.text}</div>
				{/if}
			{:else if row.kind === 'tool'}
				<details class="tool" class:error={row.status === 'error'}>
					<summary>
						<span class="pip" class:live={row.status === 'running'}></span>
						{row.name}
						{#if row.status === 'done'}✓{:else if row.status === 'error'}✗{/if}
					</summary>
					{#if row.args}
						<pre class="tool-args">{truncate(row.args, 500)}</pre>
					{/if}
					{#if row.result}
						<pre class="tool-result">{truncate(row.result, 500)}</pre>
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
			{:else if row.kind === 'tokens'}
				<div class="tokens">
					{#if row.contextWindow > 0}
						{Math.round((row.total / row.contextWindow) * 100)}% context ({row.total.toLocaleString()} / {row.contextWindow.toLocaleString()})
					{:else}
						{row.total.toLocaleString()} tokens
					{/if}
				</div>
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
		{#if app.rows.length === 0}
			<p class="muted">No messages yet. Send one below.</p>
		{/if}
	</div>

	{#if app.error}
		<p class="error">{app.error}</p>
	{/if}

	<div class="composer">
		<textarea bind:value={draft} onkeydown={onKeydown} placeholder="Message the coder — Enter to send" rows="2"
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
		height: 100vh;
		padding: 0.75rem;
		gap: 0.5rem;
	}
	.session-head {
		justify-content: space-between;
	}
	.transcript {
		flex: 1;
		overflow-y: auto;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		padding: 0.25rem;
	}
	.bubble {
		padding: 0.5rem 0.7rem;
		border-radius: var(--radius);
		white-space: pre-wrap;
		word-break: break-word;
	}
	.bubble.user {
		background: var(--bg-elev-2);
		align-self: flex-end;
		max-width: 85%;
	}
	.bubble.user.queued {
		opacity: 0.6;
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
		font-size: 0.85rem;
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
	.tool-args,
	.tool-result {
		font-size: 0.75rem;
		white-space: pre-wrap;
		word-break: break-all;
		margin: 0.3rem 0;
		padding: 0.3rem;
		background: var(--bg-elev);
		border-radius: var(--radius);
		max-height: 200px;
		overflow-y: auto;
	}
	.tool-result {
		color: var(--fg);
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
	.tokens {
		font-size: 0.7rem;
		color: var(--fg-muted);
		text-align: right;
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
