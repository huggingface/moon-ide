<script lang="ts">
	// Tool body for the `ask_user` tool. Unlike every other tool
	// body, this one is interactive: while the call is in flight
	// (`!hasResult`) it renders the agent's questions with clickable
	// options + a custom-answer textarea per question, and submits
	// the user's choices via `coder_respond_to_prompt`. Once the
	// tool settles (`hasResult`) it falls back to a read-only
	// summary of what was answered (or that the user skipped).
	//
	// The user can always skip the whole prompt by ignoring this
	// card and just sending a normal message in the composer — that
	// resolves the parked prompt with a `skipped` result on the
	// backend, so this card flips to its "skipped" summary on the
	// next `tool_result`.
	import type { AskUserQuestion, QuestionAnswer } from '../protocol';
	import { ipc } from '../ipc';
	import { fmtJson } from './toolBodyHelpers';

	interface Props {
		args: unknown;
		result: unknown;
		hasResult: boolean;
		/** Tool-call id — the key the backend parked the prompt
		 *  under. Passed through to `coder_respond_to_prompt`. */
		callId: string;
	}

	let { args, result, hasResult, callId }: Props = $props();

	/** Parse the `questions[]` array out of the tool args. Returns
	 *  `null` on any shape mismatch so the caller punts to the JSON
	 *  fallback rather than rendering a half-broken form. */
	function parseQuestions(value: unknown): AskUserQuestion[] | null {
		if (typeof value !== 'object' || value === null) {
			return null;
		}
		const raw = (value as { questions?: unknown }).questions;
		if (!Array.isArray(raw)) {
			return null;
		}
		const items: unknown[] = raw;
		const out: AskUserQuestion[] = [];
		for (const item of items) {
			if (typeof item !== 'object' || item === null) {
				return null;
			}
			const q = item as { id?: unknown; question?: unknown; options?: unknown; allow_multiple?: unknown };
			if (typeof q.id !== 'string' || typeof q.question !== 'string' || !Array.isArray(q.options)) {
				return null;
			}
			const opts: { id: string; label: string }[] = [];
			for (const o of q.options as unknown[]) {
				if (typeof o !== 'object' || o === null) {
					return null;
				}
				const opt = o as { id?: unknown; label?: unknown };
				if (typeof opt.id !== 'string' || typeof opt.label !== 'string') {
					return null;
				}
				opts.push({ id: opt.id, label: opt.label });
			}
			out.push({
				id: q.id,
				question: q.question,
				options: opts,
				allow_multiple: q.allow_multiple === true,
			});
		}
		return out;
	}

	const questions = $derived(parseQuestions(args));

	// Per-question working state, keyed by question id. `selected`
	// is the set of option ids the user has toggled; `text` is their
	// custom answer. Kept in plain `$state` maps so a re-render
	// (e.g. a sibling row updating) doesn't reset the user's
	// in-progress picks.
	let selectedByQuestion = $state<Record<string, string[]>>({});
	let textByQuestion = $state<Record<string, string>>({});
	let submitting = $state(false);
	// `true` once we've fired `respondToPrompt` from this card.
	// Guards against a double-submit while the `tool_result` round-
	// trip is still landing.
	let submitted = $state(false);

	function isSelected(qid: string, oid: string): boolean {
		return (selectedByQuestion[qid] ?? []).includes(oid);
	}

	function toggleOption(q: AskUserQuestion, oid: string): void {
		const current = selectedByQuestion[q.id] ?? [];
		if (q.allow_multiple) {
			selectedByQuestion[q.id] = current.includes(oid) ? current.filter((id) => id !== oid) : [...current, oid];
		} else {
			// Single-select: clicking the already-selected option
			// clears it (lets the user fall back to a custom answer).
			selectedByQuestion[q.id] = current.includes(oid) ? [] : [oid];
		}
	}

	/** A question counts as answered when the user picked at least
	 *  one option or typed a non-empty custom answer. */
	function isAnswered(q: AskUserQuestion): boolean {
		const sel = selectedByQuestion[q.id] ?? [];
		return sel.length > 0 || (textByQuestion[q.id] ?? '').trim().length > 0;
	}

	const anyAnswered = $derived((questions ?? []).some(isAnswered));

	async function submit(): Promise<void> {
		if (questions === null || submitting || submitted) {
			return;
		}
		const answers: QuestionAnswer[] = [];
		for (const q of questions) {
			const selected = selectedByQuestion[q.id] ?? [];
			const freeText = (textByQuestion[q.id] ?? '').trim();
			if (selected.length === 0 && freeText.length === 0) {
				// Unanswered question: omit it. The tool result
				// spells out which questions came back so the model
				// can decide whether the gap matters.
				continue;
			}
			answers.push({ question_id: q.id, selected, free_text: freeText });
		}
		submitting = true;
		try {
			await ipc.coder.respondToPrompt(callId, { answers });
			submitted = true;
		} finally {
			submitting = false;
		}
	}

	/** Submit a single-select question immediately on click (no
	 *  confirm button). Multi-select / custom-answer questions wait
	 *  for the shared confirm button. We only auto-submit when this
	 *  is the *only* question and it's single-select — multi-
	 *  question prompts always use the confirm button so the user
	 *  can answer them all before sending. */
	function onOptionClick(q: AskUserQuestion, oid: string): void {
		toggleOption(q, oid);
		if (questions !== null && questions.length === 1 && !q.allow_multiple && isSelected(q.id, oid)) {
			void submit();
		}
	}

	// --- Answered / skipped summary parsing (hasResult path) ---

	type ResultSummary = { status: 'skipped' } | { status: 'answered'; answers: QuestionAnswer[] } | { status: 'other' };

	function parseResult(value: unknown): ResultSummary {
		if (typeof value !== 'object' || value === null) {
			return { status: 'other' };
		}
		const o = value as { status?: unknown; answers?: unknown };
		if (o.status === 'skipped') {
			return { status: 'skipped' };
		}
		if (o.status === 'answered' && Array.isArray(o.answers)) {
			const answers: QuestionAnswer[] = [];
			for (const a of o.answers as unknown[]) {
				if (typeof a !== 'object' || a === null) {
					continue;
				}
				const ans = a as { question_id?: unknown; selected?: unknown; free_text?: unknown };
				answers.push({
					question_id: typeof ans.question_id === 'string' ? ans.question_id : '',
					selected: Array.isArray(ans.selected)
						? (ans.selected as unknown[]).filter((s): s is string => typeof s === 'string')
						: [],
					free_text: typeof ans.free_text === 'string' ? ans.free_text : '',
				});
			}
			return { status: 'answered', answers };
		}
		return { status: 'other' };
	}

	const summary = $derived(hasResult ? parseResult(result) : null);

	/** Map a question id + option id back to its human label for the
	 *  answered summary. Falls back to the raw id when the args
	 *  didn't parse (legacy traces). */
	function optionLabel(qid: string, oid: string): string {
		const q = (questions ?? []).find((x) => x.id === qid);
		const opt = q?.options.find((o) => o.id === oid);
		return opt?.label ?? oid;
	}

	function questionLabel(qid: string): string {
		return (questions ?? []).find((x) => x.id === qid)?.question ?? qid;
	}
</script>

{#if questions === null}
	<div class="block-label">args</div>
	<pre class="block">{fmtJson(args)}</pre>
	{#if hasResult}
		<div class="block-label">result</div>
		<pre class="block">{fmtJson(result)}</pre>
	{/if}
{:else if summary !== null}
	<!-- Settled: read-only summary of the outcome. -->
	{#if summary.status === 'skipped'}
		<div class="ask-settled skipped">Skipped — the user continued with their own message.</div>
	{:else if summary.status === 'answered'}
		<ul class="ask-summary">
			{#each summary.answers as ans (ans.question_id)}
				<li>
					<span class="ask-q">{questionLabel(ans.question_id)}</span>
					<span class="ask-a">
						{#if ans.selected.length > 0}{ans.selected.map((s) => optionLabel(ans.question_id, s)).join(', ')}{/if}
						{#if ans.free_text.length > 0}<span class="ask-free"
								>{ans.selected.length > 0 ? ' · ' : ''}{ans.free_text}</span
							>{/if}
					</span>
				</li>
			{/each}
		</ul>
	{:else}
		<div class="block-label">result</div>
		<pre class="block">{fmtJson(result)}</pre>
	{/if}
{:else}
	<!-- In flight: the interactive prompt. -->
	<div class="ask-form" class:submitting={submitting || submitted}>
		{#each questions as q (q.id)}
			<div class="ask-question">
				<div class="ask-question-text">{q.question}</div>
				<div class="ask-options">
					{#each q.options as opt (opt.id)}
						<button
							type="button"
							class="ask-option"
							class:selected={isSelected(q.id, opt.id)}
							disabled={submitting || submitted}
							onclick={() => onOptionClick(q, opt.id)}
						>
							{#if q.allow_multiple}<span class="ask-check" aria-hidden="true"
									>{isSelected(q.id, opt.id) ? '☑' : '☐'}</span
								>{/if}
							{opt.label}
						</button>
					{/each}
				</div>
				<textarea
					class="ask-custom"
					rows="1"
					placeholder="Or type a custom answer…"
					bind:value={textByQuestion[q.id]}
					disabled={submitting || submitted}
				></textarea>
			</div>
		{/each}
		<!-- Confirm button shown for multi-question prompts, multi-
		     select questions, or whenever a custom answer is in
		     play. A single single-select question auto-submits on
		     click, so the button is just a fallback there. -->
		<div class="ask-actions">
			<button
				type="button"
				class="ask-submit"
				disabled={!anyAnswered || submitting || submitted}
				onclick={() => void submit()}
			>
				{submitted ? 'Sent' : submitting ? 'Sending…' : 'Send answer'}
			</button>
			<span class="ask-skip-hint">or just type in the composer to skip</span>
		</div>
	</div>
{/if}

<style>
	.ask-form {
		display: flex;
		flex-direction: column;
		gap: 12px;
		margin-top: 4px;
		padding: 8px;
		background: var(--m-bg);
		border-radius: 4px;
	}
	.ask-form.submitting {
		opacity: 0.7;
	}
	.ask-question {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.ask-question-text {
		font-size: 12px;
		font-weight: 500;
		color: var(--m-fg);
	}
	.ask-options {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.ask-option {
		display: inline-flex;
		align-items: center;
		gap: 5px;
		padding: 4px 10px;
		font-size: 12px;
		color: var(--m-fg);
		background: var(--m-bg-raised, var(--m-bg));
		border: 1px solid var(--m-border);
		border-radius: 4px;
		cursor: pointer;
	}
	.ask-option:hover:not(:disabled) {
		border-color: var(--m-accent);
	}
	.ask-option.selected {
		border-color: var(--m-accent);
		background: color-mix(in srgb, var(--m-accent) 18%, var(--m-bg));
		color: var(--m-accent);
		font-weight: 500;
	}
	.ask-option:disabled {
		cursor: default;
	}
	.ask-check {
		font-family: var(--m-font-mono, ui-monospace, monospace);
	}
	.ask-custom {
		width: 100%;
		box-sizing: border-box;
		resize: vertical;
		min-height: 1.8em;
		padding: 4px 6px;
		font-size: 12px;
		font-family: inherit;
		color: var(--m-fg);
		background: var(--m-bg-raised, var(--m-bg));
		border: 1px solid var(--m-border);
		border-radius: 4px;
	}
	.ask-custom:focus {
		outline: none;
		border-color: var(--m-accent);
	}
	.ask-actions {
		display: flex;
		align-items: center;
		gap: 10px;
	}
	.ask-submit {
		padding: 5px 14px;
		font-size: 12px;
		font-weight: 500;
		color: var(--m-accent-fg, #fff);
		background: var(--m-accent);
		border: none;
		border-radius: 4px;
		cursor: pointer;
	}
	.ask-submit:disabled {
		opacity: 0.5;
		cursor: default;
	}
	.ask-skip-hint {
		font-size: 11px;
		color: var(--m-fg-subtle);
		font-style: italic;
	}
	.ask-settled {
		font-size: 12px;
		color: var(--m-fg-subtle);
		padding: 6px 8px;
		background: var(--m-bg);
		border-radius: 4px;
	}
	.ask-summary {
		list-style: none;
		margin: 4px 0 0;
		padding: 6px 8px;
		background: var(--m-bg);
		border-radius: 4px;
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.ask-summary li {
		display: flex;
		flex-direction: column;
		gap: 2px;
		font-size: 12px;
	}
	.ask-q {
		color: var(--m-fg-subtle);
	}
	.ask-a {
		color: var(--m-fg);
		font-weight: 500;
	}
	.ask-free {
		font-weight: 400;
		font-style: italic;
	}
</style>
