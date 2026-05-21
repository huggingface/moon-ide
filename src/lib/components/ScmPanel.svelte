<script lang="ts">
	// Source Control panel under the folder bars. Composes a
	// commit gesture plus the standard pull/push/sync affordances.
	//
	// Layout (top to bottom):
	//
	//   1. Header: branch / detached-HEAD short SHA, revert-all,
	//      open-PR, the change-count pill that doubles as the
	//      "filter to changes only" toggle.
	//   2. Composer: textarea for the message, with an inset
	//      sparkle button in the top-right that drives
	//      `coder_suggest_commit_message` when the user is signed
	//      in.
	//   3. Commit row: a single split-button "[Commit ...] [⎇] [✎]"
	//      where the main label flips between **Commit / Amend /
	//      Commit to new branch** depending on which toggle is on.
	//      Branch and amend are mutually exclusive — flipping one
	//      on flips the other off. When branch-mode is active a
	//      branch-name input slides in above the button.
	//   4. Sync button (or Publish-branch when no upstream is
	//      configured), with the refresh icon rotating + label
	//      flipping to "Syncing…" while busy.
	//
	// Amend prefill: toggling amend on with an empty message
	// fetches `git log -1 --pretty=%B` and seeds the textarea so
	// the user can edit-from-existing rather than re-type. The
	// prefill is tracked in `amendPrefill`; toggling amend off
	// only clears the textarea if the user never touched the
	// prefilled bytes — any edit they made survives the toggle.
	//
	// "Stage everything then commit" remains the gesture (Phase 5
	// per-file staging is later); errors from git (no identity,
	// nothing to commit, push/pull failures) surface as flash
	// toasts and focus returns to the composer for retry.

	import { tick } from 'svelte';
	import { openUrl } from '@tauri-apps/plugin-opener';
	import { workspace } from '../state.svelte';
	import { coder } from '../coder.svelte';
	import { ipc } from '../ipc';
	import { formatError } from '../protocol';
	import { textInputUndo } from '../actions/textInputUndo';
	import { isReviewPath } from '../util/reviewPath';
	import BranchIcon from './icons/BranchIcon.svelte';
	import MergeIcon from './icons/MergeIcon.svelte';
	import PullRequestIcon from './icons/PullRequestIcon.svelte';
	import RefreshIcon from './icons/RefreshIcon.svelte';
	import RevertIcon from './icons/RevertIcon.svelte';
	import ReviewIcon from './icons/ReviewIcon.svelte';
	import SparklesIcon from './icons/SparklesIcon.svelte';

	const branch = $derived(workspace.gitBranch);

	// Short label for the SCM compare-baseline pill — e.g.
	// `'main'` / `'master'`. Resolved from
	// `FolderState.defaultBranchName` when the `'default'`
	// baseline is active and applicable, else falls back to
	// `branch.defaultBranchRemoteRef` so the pill can suggest
	// "vs main" before the user ever flips the toggle (lets the
	// affordance signal what it'd compare against). `null` means
	// "no default branch resolvable" — the pill suppresses
	// itself entirely in that case.
	const compareLabel = $derived.by(() => {
		const ref = workspace.defaultBranchName ?? branch.defaultBranchRemoteRef;
		if (ref === null) {
			return null;
		}
		const slash = ref.indexOf('/');
		return slash === -1 ? ref : ref.slice(slash + 1);
	});

	// On the default branch itself the toggle is meaningless
	// (merge-base = HEAD, diff is empty). Suppress the pill
	// rather than rendering an inert "vs main" you can't click
	// usefully.
	const compareToggleApplicable = $derived(
		compareLabel !== null && branch.name !== null && branch.name !== compareLabel,
	);
	const branchLabel = $derived.by(() => {
		if (branch.name !== null) {
			return branch.name;
		}
		if (branch.headShortSha !== null) {
			// Detached HEAD: surface the SHA so the user knows
			// where they are. Parens distinguish "this is a hash,
			// not a branch name".
			return `(${branch.headShortSha})`;
		}
		return null;
	});

	let amend = $state(false);
	// Split into two flags so the commit-row spinner only fires
	// during commit / commit-on-new-branch, not during sync /
	// publish (which used to flash a spinner on the disabled
	// commit button — implying "your commit is in progress" when
	// it's actually a pull/push). The broader `busy` derived flag
	// keeps gating "disable everything else while one git op
	// runs" with no per-call-site changes. `revertAll` doesn't
	// need its own flag — it's a one-shot, doesn't fan out into
	// spinners.
	let committing = $state(false);
	let syncing = $state(false);
	// `git merge origin/main` ("Update from main") gets its own
	// busy flag so its button can spin independently while not
	// firing the commit-row spinner. Folded into the broader
	// `busy` gate so commit / sync stay disabled while a merge
	// is in flight (and vice versa) — running two write-side
	// git ops in parallel is a recipe for "what just happened
	// to my working tree".
	let merging = $state(false);
	// `git merge --abort` gets its own flag for the same reason
	// commit / sync / merge do — the button needs a spinner that
	// doesn't piggy-back on a sibling op's busy state. Aborting
	// is short (one git invocation, no hooks) but the user is in
	// a "did this work?" frame of mind during a conflict, so the
	// affordance has to feel definite.
	let aborting = $state(false);
	let busy = $derived(committing || syncing || merging || aborting);

	// Master switch for the merge-in-progress UI shape. Reading
	// off `workspace.gitMergeState.inProgress` keeps every
	// reshape (header pill, hidden sync buttons, commit-row
	// label swap) on a single source of truth that the fs-watcher
	// drives. The `Merging <ref>` pill prefers `mergingRef`;
	// `mergingRef` itself falls back to a short SHA inside the
	// backend, so a missing ref never blanks the pill.
	const mergeMode = $derived(workspace.gitMergeState.inProgress);
	const unmergedCount = $derived(workspace.gitMergeState.unmergedPaths.length);
	const mergeRefLabel = $derived(workspace.gitMergeState.mergingRef ?? 'merge');
	let textarea: HTMLTextAreaElement | undefined = $state();

	// Tracks the bytes we wrote into `workspace.commitDraft` from
	// `git_head_commit_message` when the amend toggle flipped on
	// from an empty composer. If the user toggles amend off
	// *without* editing the prefill, we clear the textarea so
	// they're back at a fresh-commit composer; if they edited it,
	// we leave their bytes alone. Set to `''` whenever the user
	// types or the AI suggestion lands — those are user-driven
	// values, not the prefill.
	let amendPrefill = $state('');

	// Tracks the bytes we wrote into `workspace.commitDraft`
	// from `.git/MERGE_MSG` when the panel entered merge-in-
	// progress mode with an empty composer. Same shape as
	// `amendPrefill`: the abort handler only clears the
	// composer when the bytes still match the prefill, so a
	// user-typed message during resolution survives the abort.
	let mergePrefill = $state('');

	// "Commit to new branch" inline form. `newBranchOpen` toggles
	// the branch-name input row visible above the commit button;
	// `newBranchName` mirrors its input. Closing the form clears
	// the name so a re-open doesn't carry stale text.
	// `suggestingBranch` gates the branch-name sparkle while the
	// fast-model call is in flight; `suggestingMessage` does the
	// same for the composer-inset commit-message sparkle.
	let newBranchOpen = $state(false);
	let newBranchName = $state('');
	let suggestingBranch = $state(false);
	let suggestingMessage = $state(false);
	let newBranchInput: HTMLInputElement | undefined = $state();

	const changeCount = $derived(workspace.scmChangeCount);

	// Amend-with-empty-message is valid (preserve previous
	// subject); fresh commits still need a message. Push and pull
	// buttons just need "not currently busy". In merge-mode the
	// gate adds "no unresolved conflicts remain" — git itself
	// would refuse the commit otherwise, but disabling the button
	// is the better surface.
	const canCommit = $derived.by(() => {
		if (busy) {
			return false;
		}
		if (mergeMode) {
			return unmergedCount === 0 && workspace.commitDraft.trim().length > 0;
		}
		return amend || workspace.commitDraft.trim().length > 0;
	});

	// "Commit to new branch" requires both a non-empty message and
	// a non-empty branch name; amend doesn't apply (you can't
	// amend HEAD into a new branch with the same gesture). The
	// "branch toggle" pill is also disabled while busy.
	const canCommitNewBranch = $derived(
		!busy && workspace.commitDraft.trim().length > 0 && newBranchName.trim().length > 0,
	);

	// Single gate for the unified commit button. In branch mode it
	// requires the branch-name field too; otherwise it's the
	// regular commit-or-amend gate.
	const canSubmit = $derived(newBranchOpen ? canCommitNewBranch : canCommit);

	// Main label for the unified commit button. The button's
	// onclick branches on `newBranchOpen` to call the right
	// backend; the label is the user-facing signal that "this
	// gesture will do that thing".
	const commitButtonLabel = $derived.by(() => {
		if (mergeMode) {
			return 'Commit merge';
		}
		if (newBranchOpen) {
			return 'Commit to new branch';
		}
		if (amend) {
			return 'Amend';
		}
		return 'Commit';
	});

	// "Publish branch" lives in the same slot as the sync button
	// and takes priority: when a real branch is checked out but
	// has no upstream configured (freshly-created locally, never
	// pushed), `git push` would fail with "no upstream branch" —
	// the user actually wants `git push -u origin <branch>` here.
	// Detached HEAD (`branch.name === null`) is excluded; there's
	// nothing to publish.
	const needsPublish = $derived(branch.name !== null && !branch.hasUpstream);

	// "Open PR" button visibility. We surface the affordance only
	// when:
	//   - the backend produced a PR URL (recognised remote host
	//     and a non-detached branch, so `prUrl !== null`),
	//   - the branch has been published (no point asking GitHub to
	//     PR an upstream that doesn't exist),
	//   - the branch isn't main / master (PR'ing the default branch
	//     into itself is never the intent — and the extra click on
	//     the branch you most often have checked out would be a
	//     papercut).
	// Hardcoding the default-branch names is the "hardcode first,
	// configure later" rule from AGENTS.md; if a team uses a
	// different default we'll add a config knob then.
	//
	// When `workspace.gitExistingPrUrl` is set (a `gh pr list
	// --head <branch>` call resolved a matching open PR), prefer
	// that over `branch.prUrl` so the button takes the user to the
	// existing PR rather than the create-PR form. The gating above
	// still applies to the visibility decision; the URL swap is
	// orthogonal.
	const prButtonVisible = $derived.by(() => {
		if (branch.prUrl === null || !branch.hasUpstream) {
			return false;
		}
		if (branch.name === 'main' || branch.name === 'master') {
			return false;
		}
		return true;
	});

	const prUrl = $derived.by(() => {
		if (!prButtonVisible) {
			return null;
		}
		return workspace.gitExistingPrUrl ?? branch.prUrl;
	});

	const prIsExisting = $derived(prButtonVisible && workspace.gitExistingPrUrl !== null);

	function openPr() {
		if (prUrl === null) {
			return;
		}
		void openUrl(prUrl);
	}

	// `true` iff the branch has commits to pull or push — the sync
	// button is hidden otherwise. Counts render as separate spans
	// inside the button so each direction can carry its own glyph
	// (`N↓` for behind, `M↑` for ahead). Diverged branches show
	// both, matching Cursor's / VSCode's "Sync Changes" pattern.
	const canSync = $derived(!needsPublish && (branch.ahead > 0 || branch.behind > 0));

	// Local short name of the repo's default branch, derived from
	// the remote-tracking ref (`origin/main` → `main`). Used in
	// the "Update from <name>" button label and confirm-flash so
	// the user sees the local-style name they recognise rather
	// than the disambiguated remote-tracking string.
	const defaultBranchShortName = $derived.by(() => {
		const ref = branch.defaultBranchRemoteRef;
		if (ref === null) {
			return null;
		}
		const slash = ref.indexOf('/');
		if (slash < 0) {
			return ref;
		}
		return ref.slice(slash + 1);
	});

	// "Update from main" affordance. Only shown when the remote
	// default branch has commits we don't, AND we're not on it
	// ourselves (the regular `Sync Changes` button covers the
	// "I'm on main and origin/main moved" case via `behind`).
	// Auto-fetch keeps `defaultBranchBehind` current; we don't
	// fetch here, the merge runs against whatever the local
	// remote-tracking ref points at right now.
	const canMergeDefault = $derived(!busy && branch.defaultBranchRemoteRef !== null && branch.defaultBranchBehind > 0);

	// Tooltip detail for the sync button. Plain-text fallback for
	// the (Push / Pull / Sync) labels that we used to bake into
	// the button text.
	const syncTitle = $derived.by(() => {
		const a = branch.ahead;
		const b = branch.behind;
		if (a > 0 && b > 0) {
			// Diverged: first click rebases local commits on top
			// of upstream, second click pushes. The tooltip
			// mirrors what the click will actually do *right
			// now* so it doesn't oversell the operation.
			return `Rebase ${a} commit${a === 1 ? '' : 's'} onto ${b} upstream commit${b === 1 ? '' : 's'} (push on next click)`;
		}
		if (a > 0) {
			return `Push ${a} commit${a === 1 ? '' : 's'} to upstream`;
		}
		if (b > 0) {
			return `Pull ${b} commit${b === 1 ? '' : 's'} from upstream`;
		}
		return '';
	});

	async function commit() {
		if (!canCommit) {
			return;
		}
		committing = true;
		try {
			const ok = await workspace.commitChanges(workspace.commitDraft, amend);
			if (ok) {
				workspace.commitDraft = '';
				amend = false;
				amendPrefill = '';
				await tick();
				autoSize();
			}
		} finally {
			committing = false;
			textarea?.focus();
		}
	}

	async function commitOnNewBranch() {
		if (!canCommitNewBranch) {
			return;
		}
		committing = true;
		try {
			const ok = await workspace.commitChangesOnNewBranch(newBranchName, workspace.commitDraft);
			if (ok) {
				workspace.commitDraft = '';
				newBranchName = '';
				newBranchOpen = false;
				amend = false;
				amendPrefill = '';
				await tick();
				autoSize();
			}
		} finally {
			committing = false;
			textarea?.focus();
		}
	}

	/**
	 * Flip the amend toggle. Mutually exclusive with branch mode —
	 * amending into a *new* branch isn't a coherent gesture, so
	 * branch mode goes off when amend turns on (and vice versa via
	 * `setNewBranch`).
	 *
	 * On amend → on with an empty composer, fetch the HEAD commit
	 * subject + body and prefill the textarea so the user can
	 * edit-from-existing rather than re-type. Bytes get tracked in
	 * `amendPrefill`; toggling amend off only clears the textarea
	 * if those bytes are still untouched (any user edit survives).
	 */
	async function setAmend(next: boolean) {
		if (next === amend) {
			return;
		}
		if (next) {
			if (newBranchOpen) {
				newBranchOpen = false;
				newBranchName = '';
			}
			amend = true;
			if (workspace.commitDraft.trim().length > 0) {
				return;
			}
			try {
				const head = await ipc.fs.gitHeadCommitMessage();
				if (head.length === 0) {
					return;
				}
				workspace.commitDraft = head;
				amendPrefill = head;
				await tick();
				autoSize();
			} catch {
				// Best-effort prefill — failure is silent. Amend
				// itself still works, the user just types a fresh
				// message (or leaves it blank for `--no-edit`).
			}
			return;
		}
		amend = false;
		if (amendPrefill.length > 0 && workspace.commitDraft === amendPrefill) {
			workspace.commitDraft = '';
			amendPrefill = '';
			await tick();
			autoSize();
		}
	}

	/**
	 * Flip the branch-mode toggle. Mutually exclusive with amend
	 * (see `setAmend`). On open, focuses the freshly-revealed
	 * branch-name input so the user can type immediately, and —
	 * when the coder is signed in — fans out two AI suggestions
	 * in parallel: one to fill the empty commit message and one
	 * to fill the empty branch name. Both are silent on failure
	 * and won't clobber bytes the user typed during the
	 * roundtrip. They run independently — the branch-name
	 * suggester pulls its own diff summary on the backend, so it
	 * doesn't need to wait on the commit-message round-trip to
	 * produce something useful.
	 *
	 * On close, clears the in-progress branch name so a re-open
	 * doesn't carry stale text. The auto-suggested commit
	 * message stays — the user moved away from "commit on a new
	 * branch" but probably still wants to commit.
	 */
	async function setNewBranch(next: boolean) {
		if (next === newBranchOpen) {
			return;
		}
		if (next) {
			if (amend) {
				await setAmend(false);
			}
			newBranchOpen = true;
			await tick();
			newBranchInput?.focus();
			if (coder.signedIn) {
				if (workspace.commitDraft.trim().length === 0 && !suggestingMessage) {
					void autoSuggestCommitMessage();
				}
				if (newBranchName.length === 0 && !suggestingBranch) {
					void autoSuggestBranchName();
				}
			}
			return;
		}
		newBranchOpen = false;
		newBranchName = '';
	}

	/**
	 * Auto-fill the branch-name input when branch-mode opens.
	 * Distinct from the explicit sparkle-click
	 * (`suggestBranchName`): silent on failure (no flash toast —
	 * the user didn't ask), and re-checks `newBranchOpen` /
	 * `newBranchName` after the await so we don't clobber bytes
	 * the user typed during the roundtrip or fill a form they
	 * already closed. Focus stays wherever the user put it; the
	 * explicit sparkle is the gesture that grabs focus.
	 */
	async function autoSuggestBranchName() {
		suggestingBranch = true;
		try {
			const name = await ipc.coder.suggestBranchName(workspace.commitDraft);
			if (!newBranchOpen || newBranchName.length > 0) {
				return;
			}
			if (name.trim().length === 0) {
				return;
			}
			newBranchName = name;
		} catch {
			// Silent: the user didn't ask for this suggestion, so
			// surfacing a toast would be noise. They can always
			// click the sparkle for an explicit retry.
		} finally {
			suggestingBranch = false;
		}
	}

	/**
	 * Auto-fill the commit-message textarea when branch-mode
	 * opens. Same silent-on-failure / no-clobber treatment as
	 * `autoSuggestBranchName`: the user clicked branch toggle,
	 * not the commit-message sparkle, so failure goes to debug
	 * rather than a toast, and any bytes the user typed during
	 * the roundtrip win.
	 */
	async function autoSuggestCommitMessage() {
		suggestingMessage = true;
		try {
			const next = await ipc.coder.suggestCommitMessage(workspace.commitDraft);
			if (workspace.commitDraft.trim().length > 0) {
				return;
			}
			if (next.trim().length === 0) {
				return;
			}
			workspace.commitDraft = next;
			// User-driven content for amend-toggle-off purposes —
			// clear the prefill marker so toggling amend off
			// later doesn't try to wipe the suggestion thinking
			// it was an amend prefill.
			amendPrefill = '';
			await tick();
			autoSize();
		} catch {
			// Silent for the same reason as the branch-name
			// auto-suggester.
		} finally {
			suggestingMessage = false;
		}
	}

	async function suggestCommitMessage() {
		if (suggestingMessage || busy) {
			return;
		}
		suggestingMessage = true;
		try {
			const next = await ipc.coder.suggestCommitMessage(workspace.commitDraft);
			if (next.trim().length === 0) {
				return;
			}
			workspace.commitDraft = next;
			// User-driven content; clear the prefill marker so a
			// later amend-toggle-off doesn't try to wipe the AI
			// suggestion thinking it was the prefill we wrote.
			amendPrefill = '';
			await tick();
			autoSize();
			textarea?.focus();
		} catch (err) {
			workspace.flash(`Could not suggest a commit message: ${formatError(err)}`);
		} finally {
			suggestingMessage = false;
		}
	}

	async function onCommitClick() {
		if (mergeMode) {
			await commitMerge();
			return;
		}
		if (newBranchOpen) {
			await commitOnNewBranch();
			return;
		}
		await commit();
	}

	/**
	 * Finish the in-flight merge by committing the resolved tree.
	 * The composer's bytes ride the regular `git_commit` path
	 * server-side; with `.git/MERGE_HEAD` present, git produces a
	 * two-parent merge commit. `workspace.commitMerge` handles
	 * the post-success refresh + scm-filter reset.
	 */
	async function commitMerge() {
		if (!canCommit) {
			return;
		}
		committing = true;
		try {
			const ok = await workspace.commitMerge(workspace.commitDraft);
			if (ok) {
				workspace.commitDraft = '';
				mergePrefill = '';
				amend = false;
				amendPrefill = '';
				await tick();
				autoSize();
			}
		} finally {
			committing = false;
			textarea?.focus();
		}
	}

	/**
	 * Abort the in-flight merge. Destructive (wipes whatever
	 * conflict-resolution edits the user staged so far), but
	 * cheaper to invoke than to undo by hand — the user can
	 * always retry the merge. No confirm modal: the button label
	 * is unambiguous, and stashing on cancel would require its
	 * own UX. If a confirmation step turns out to be missed in
	 * practice we'll add it then.
	 */
	async function abortMerge() {
		if (busy) {
			return;
		}
		aborting = true;
		try {
			const ok = await workspace.abortMerge();
			if (ok) {
				// Composer carried `MERGE_MSG`'s prefill or
				// whatever the user typed during resolution.
				// Either way it's no longer relevant once
				// the merge is gone.
				if (workspace.commitDraft === mergePrefill || mergePrefill.length === 0) {
					workspace.commitDraft = '';
				}
				mergePrefill = '';
				await tick();
				autoSize();
			}
		} finally {
			aborting = false;
			textarea?.focus();
		}
	}

	async function suggestBranchName() {
		if (suggestingBranch) {
			return;
		}
		suggestingBranch = true;
		try {
			const name = await ipc.coder.suggestBranchName(workspace.commitDraft);
			newBranchName = name;
			void tick().then(() => newBranchInput?.focus());
		} catch (err) {
			workspace.flash(`Could not suggest a branch name: ${formatError(err)}`);
		} finally {
			suggestingBranch = false;
		}
	}

	function onNewBranchKey(event: KeyboardEvent) {
		if (event.key === 'Enter' && !event.isComposing) {
			event.preventDefault();
			void commitOnNewBranch();
			return;
		}
		if (event.key === 'Escape') {
			event.preventDefault();
			newBranchOpen = false;
			newBranchName = '';
			textarea?.focus();
		}
	}

	/**
	 * Single-button sync: pulls first if behind, then pushes if
	 * the local branch was (or still is, after the pull) ahead.
	 * If pull fails we bail early so the user doesn't end up with
	 * a non-fast-forward push attempt on top of an unresolved
	 * merge / conflict / dirty-tree situation.
	 */
	async function publish() {
		if (busy) {
			return;
		}
		syncing = true;
		try {
			await workspace.publishBranch();
		} finally {
			syncing = false;
		}
	}

	async function mergeDefault() {
		if (busy) {
			return;
		}
		const ref = branch.defaultBranchRemoteRef;
		if (ref === null) {
			return;
		}
		merging = true;
		try {
			await workspace.mergeDefaultBranch(ref);
			// Same await-the-refresh-before-clearing-busy pattern
			// `sync()` uses, for the same reason: stop the button
			// from briefly un-disabling between "merge returned"
			// and "behind hit zero" before it unmounts.
			await workspace.refreshGitBranch();
		} finally {
			merging = false;
		}
	}

	async function sync() {
		if (busy) {
			return;
		}
		const initialAhead = branch.ahead;
		const initialBehind = branch.behind;
		if (initialAhead === 0 && initialBehind === 0) {
			return;
		}
		syncing = true;
		try {
			if (initialBehind > 0) {
				// `pullChanges` runs `git pull --rebase`; on
				// conflict the backend aborts the rebase so the
				// working tree is restored. When the branch is
				// diverged (both ahead and behind) we *only*
				// pull on this click — pushing immediately after
				// a rebase would surprise the user who hasn't
				// had a chance to see the rebased history. The
				// next click sees ahead-only and pushes.
				await workspace.pullChanges();
				return;
			}
			if (initialAhead > 0) {
				await workspace.pushChanges();
			}
			// Wait for the branch counters to refresh before
			// flipping `syncing` off, so the button doesn't
			// briefly un-disable between "pull/push returned"
			// and "ahead/behind hit zero" — without this the
			// button visibly flickers from disabled-spinning
			// → enabled → unmounted as the post-sync git
			// state catches up. `refreshGitBranch` is a
			// single `git symbolic-ref` + `git rev-list
			// --count`, so the extra await is cheap.
			await workspace.refreshGitBranch();
		} finally {
			syncing = false;
		}
	}

	/**
	 * "Revert all changes" — discards every non-ignored entry in
	 * `gitStatusEntries`. Tracked-changed paths route through
	 * `git restore --source=HEAD --staged --worktree`; untracked
	 * paths route to the OS trash. `discardPaths` already wraps
	 * the work in a confirm dialog, so this is just "build the
	 * list and hand it over".
	 */
	async function revertAll() {
		if (busy) {
			return;
		}
		const paths = workspace.gitStatusEntries.filter((e) => e.status !== 'ignored').map((e) => e.path);
		if (paths.length === 0) {
			return;
		}
		// Reuse `committing` here — revert is a working-tree
		// mutation in the same family as commit, and we want the
		// same "everything is disabled while I rewrite the tree"
		// gate. Borrowing the flag avoids a third per-row state.
		committing = true;
		try {
			await workspace.discardPaths(paths);
		} finally {
			committing = false;
		}
	}

	// Ctrl/Cmd+Enter commits; plain Enter falls through to the
	// browser's default and inserts a newline (so a multi-line
	// commit body composes naturally). Esc clears the draft so a
	// quick "ah, never mind" doesn't leave the buffer dirty across
	// panel re-renders.
	function onKeydown(event: KeyboardEvent) {
		if (event.key === 'Enter' && (event.ctrlKey || event.metaKey) && !event.isComposing) {
			event.preventDefault();
			void onCommitClick();
			return;
		}
		if (event.key === 'Escape' && workspace.commitDraft.length > 0) {
			event.preventDefault();
			workspace.commitDraft = '';
			void tick().then(autoSize);
		}
	}

	// Manual autoresize: collapse with `height: auto`, then set
	// height to the post-collapse `scrollHeight`. The CSS
	// `max-height` caps multi-line bodies so they don't push the
	// file tree off-screen. We toggle `overflow-y` based on
	// whether we hit the cap because a permanent
	// `overflow-y: auto` shows a scrollbar in the rare 1-2px
	// rounding gap between `scrollHeight` and the rendered box —
	// content fits, scrollbar appears anyway, looks broken. Hidden
	// by default → switch to `auto` only when the content can't
	// actually fit.
	function autoSize() {
		const el = textarea;
		if (!el) {
			return;
		}
		el.style.height = 'auto';
		const cap = parseFloat(getComputedStyle(el).maxHeight);
		const next = el.scrollHeight;
		if (Number.isFinite(cap) && next > cap) {
			el.style.height = `${cap}px`;
			el.style.overflowY = 'auto';
			return;
		}
		el.style.height = `${next}px`;
		el.style.overflowY = 'hidden';
	}

	// Re-run autosize whenever the message string changes (clearing
	// the draft after a commit needs the textarea to shrink back,
	// and a folder switch may swap in a longer/shorter draft for
	// the new folder).
	$effect(() => {
		void workspace.commitDraft;
		autoSize();
	});

	// Merge-in-progress prefill. When the panel notices a fresh
	// merge (`mergeMode` flipped on, no draft yet), seed the
	// composer with `.git/MERGE_MSG` — git's own default merge
	// commit message — so the user only has to edit if they
	// disagree. Same shape as the amend prefill: stamp the bytes
	// into both the draft and `mergePrefill` so the abort
	// handler can tell whether the user touched it.
	$effect(() => {
		const state = workspace.gitMergeState;
		if (!state.inProgress) {
			// Coming out of merge mode: drop the prefill marker
			// so a later, unrelated draft change doesn't
			// accidentally match a stale value.
			mergePrefill = '';
			return;
		}
		if (workspace.commitDraft.length > 0) {
			return;
		}
		const msg = state.defaultMessage;
		if (msg === null || msg.trim().length === 0) {
			return;
		}
		workspace.commitDraft = msg;
		mergePrefill = msg;
		void tick().then(autoSize);
	});

	// Post-flush marker for folder-swap profiling: fires after the
	// SCM panel has reconciled. See the matching mark in CoderPanel
	// for the wider strategy.
	$effect(() => {
		void workspace.activeFolderPath;
		void workspace.gitStatusEntries;
		void workspace.scmFilterOn;
		performance.mark('moon:scmPanel.update');
	});
</script>

<section class="scm" aria-label="Source control">
	{#if branchLabel !== null}
		<div class="header">
			<button
				type="button"
				class="branch"
				title={branch.name === null
					? 'Detached HEAD — click to switch'
					: `Branch: ${branch.name} — click to switch (Ctrl+Shift+B)`}
				onclick={() => workspace.openBranchSwitcher()}
			>
				<span class="branch-icon" aria-hidden="true">⎇</span>
				<span class="branch-name">{branchLabel}</span>
			</button>
			<div class="actions">
				{#if changeCount > 0}
					<button
						type="button"
						class="icon-btn danger"
						title="Revert all changes"
						aria-label="Revert all changes"
						disabled={busy}
						onclick={revertAll}
					>
						<RevertIcon />
					</button>
				{/if}
				{#if prUrl !== null}
					{@const prLabel = prIsExisting ? 'View pull request on GitHub' : 'Open pull request on GitHub'}
					<button type="button" class="icon-btn" title={`${prLabel} (${prUrl})`} aria-label={prLabel} onclick={openPr}>
						<PullRequestIcon />
					</button>
				{/if}
				{#if changeCount > 0}
					{@const isDefault = workspace.compareBaseline === 'default'}
					{@const reviewBaselineLabel = isDefault && compareLabel !== null ? compareLabel : 'HEAD'}
					{@const reviewActive = workspace.activePath !== null && isReviewPath(workspace.activePath)}
					<button
						type="button"
						class="icon-btn"
						class:active={reviewActive}
						title={reviewActive
							? 'Close review (jump to file under cursor)'
							: `Open aggregated diff against ${reviewBaselineLabel}`}
						aria-label={reviewActive
							? 'Close review (jump to file under cursor)'
							: `Open aggregated diff against ${reviewBaselineLabel}`}
						aria-pressed={reviewActive}
						onclick={() => void workspace.toggleReviewTab()}
					>
						<ReviewIcon />
					</button>
				{/if}
				{#if compareToggleApplicable}
					{@const isDefault = workspace.compareBaseline === 'default'}
					<button
						type="button"
						class="compare-pill"
						class:active={isDefault}
						title={isDefault
							? `Comparing working tree against ${compareLabel} (click to compare against last commit instead)`
							: `Click to show every file changed since branching off ${compareLabel}`}
						aria-label={isDefault ? `Stop comparing against ${compareLabel}` : `Compare against ${compareLabel}`}
						aria-pressed={isDefault}
						onclick={() => workspace.setCompareBaseline(isDefault ? 'head' : 'default')}
					>
						vs {compareLabel}
					</button>
				{/if}
				{#if mergeMode}
					<!-- Status pill that announces "you are inside a merge". Click
					     is no-op; the affordances live in the reshaped commit row
					     below. Warning colour distinguishes the pill from the
					     regular compare-baseline / changes pills. -->
					<span
						class="merge-pill"
						title={unmergedCount > 0
							? `Merging ${mergeRefLabel} — ${unmergedCount} unresolved`
							: `Merging ${mergeRefLabel} — ready to commit`}
						aria-label={`Merge in progress: ${mergeRefLabel}`}
					>
						Merging {mergeRefLabel}
					</span>
				{/if}
				{#if changeCount > 0 || workspace.scmFilterOn}
					<button
						type="button"
						class="changes-badge"
						class:active={workspace.scmFilterOn}
						title={workspace.scmFilterOn
							? `${changeCount} change${changeCount === 1 ? '' : 's'} (click to show all files)`
							: `${changeCount} change${changeCount === 1 ? '' : 's'} (click to filter to changes only)`}
						aria-label={workspace.scmFilterOn
							? `Showing ${changeCount} changes — click to show all files`
							: `${changeCount} changes — click to filter`}
						aria-pressed={workspace.scmFilterOn}
						onclick={() => workspace.toggleScmFilter()}
					>
						{changeCount}
					</button>
				{/if}
			</div>
		</div>
	{/if}
	<div class="composer">
		<textarea
			bind:this={textarea}
			bind:value={() => workspace.commitDraft, (v) => (workspace.commitDraft = v)}
			use:textInputUndo
			class="input"
			class:input-with-ai={coder.signedIn}
			rows="1"
			placeholder={mergeMode
				? 'Merge commit message'
				: amend
					? 'Amend message (leave empty to keep)'
					: 'Commit message'}
			aria-label="Commit message"
			disabled={busy}
			onkeydown={onKeydown}
			oninput={autoSize}
		></textarea>
		{#if coder.signedIn}
			<button
				type="button"
				class="composer-ai"
				title="Suggest a commit message from the diff"
				aria-label="Suggest a commit message"
				disabled={busy || suggestingMessage}
				onclick={suggestCommitMessage}
			>
				{#if suggestingMessage}
					<span class="spinner" aria-hidden="true"></span>
				{:else}
					<SparklesIcon size={12} />
				{/if}
			</button>
		{/if}
	</div>
	{#if newBranchOpen && !mergeMode}
		<div class="new-branch-row">
			<input
				bind:this={newBranchInput}
				bind:value={newBranchName}
				use:textInputUndo
				class="new-branch-input"
				type="text"
				spellcheck="false"
				autocomplete="off"
				autocapitalize="off"
				placeholder="new-branch-name"
				aria-label="New branch name"
				disabled={busy}
				onkeydown={onNewBranchKey}
			/>
			{#if coder.signedIn}
				<button
					type="button"
					class="new-branch-suggest"
					title="Suggest a branch name from the diff and message"
					aria-label="Suggest a branch name"
					disabled={busy || suggestingBranch}
					onclick={suggestBranchName}
				>
					{#if suggestingBranch}
						<span class="spinner" aria-hidden="true"></span>
					{:else}
						<SparklesIcon size={12} />
					{/if}
				</button>
			{/if}
		</div>
	{/if}
	<!-- Commit row. Two shapes:

	     - Regular: split-button "[Commit ...] [⎇] [✎]" with the
	       new-branch + amend toggles, plus the main label flipping
	       between Commit / Amend / Commit to new branch.
	     - Merge-in-progress: paired "[Commit merge] [Abort merge]"
	       buttons. The toggles drop out (amending into a new
	       branch mid-merge isn't a coherent gesture), and the
	       unresolved-conflict count surfaces as a hint below
	       when the user can't yet commit. -->
	<div class="commit-row" class:busy>
		<button
			type="button"
			class="commit-btn"
			title={mergeMode && unmergedCount > 0
				? `${unmergedCount} unresolved conflict${unmergedCount === 1 ? '' : 's'} — edit the file${unmergedCount === 1 ? '' : 's'} to resolve, then save`
				: commitButtonLabel}
			disabled={!canSubmit}
			onclick={onCommitClick}
		>
			{#if committing}
				<!-- Spinner only fires on actual commit (or revert,
				     which borrows the same flag). Sync / publish
				     leave the commit button quiet — they have their
				     own spinning refresh icon below; doubling up
				     read as "your commit is also in progress" when
				     it isn't. -->
				<span class="spinner spinner-on-accent" aria-hidden="true"></span>
			{/if}
			<span class="commit-btn-label">{commitButtonLabel}</span>
		</button>
		{#if mergeMode}
			<button
				type="button"
				class="commit-btn-toggle merge-abort"
				title={`Abort the merge and restore the pre-merge working tree (git merge --abort)`}
				aria-label="Abort merge"
				disabled={busy}
				onclick={() => void abortMerge()}
			>
				{#if aborting}
					<span class="spinner" aria-hidden="true"></span>
				{:else}
					<span aria-hidden="true">✕</span>
				{/if}
			</button>
		{:else}
			<button
				type="button"
				class="commit-btn-toggle"
				class:active={newBranchOpen}
				title={newBranchOpen
					? 'Cancel commit-to-new-branch'
					: 'Commit on a new branch (creates a fresh branch from HEAD)'}
				aria-label={newBranchOpen ? 'Cancel commit-to-new-branch' : 'Commit on a new branch'}
				aria-pressed={newBranchOpen}
				disabled={busy}
				onclick={() => void setNewBranch(!newBranchOpen)}
			>
				<BranchIcon size={12} />
			</button>
			<button
				type="button"
				class="commit-btn-toggle"
				class:active={amend}
				title={amend ? 'Amend HEAD on next commit (click to disable)' : 'Amend HEAD on next commit'}
				aria-label={amend ? 'Amend mode on' : 'Toggle amend mode'}
				aria-pressed={amend}
				disabled={busy}
				onclick={() => void setAmend(!amend)}
			>
				<span aria-hidden="true">✎</span>
			</button>
		{/if}
	</div>
	{#if mergeMode && unmergedCount > 0}
		<div class="merge-hint" role="status">
			{unmergedCount}
			file{unmergedCount === 1 ? '' : 's'} still
			{unmergedCount === 1 ? 'has' : 'have'}
			unresolved conflict{unmergedCount === 1 ? '' : 's'}.
		</div>
	{/if}
	{#if !mergeMode && needsPublish}
		<button
			type="button"
			class="sync-btn"
			title={busy
				? 'Publishing branch…'
				: `Push the local branch and set its upstream (git push -u origin ${branch.name ?? 'HEAD'})`}
			disabled={busy}
			onclick={publish}
		>
			<span class="sync-btn-icon" class:rotating={busy}><RefreshIcon size={12} /></span>
			<span class="sync-btn-label">{busy ? 'Publishing…' : 'Publish Branch'}</span>
			{#if !busy}
				<span class="sync-btn-arrow" aria-hidden="true">↑</span>
			{/if}
		</button>
	{:else if !mergeMode && canSync}
		<button type="button" class="sync-btn" title={busy ? 'Syncing changes…' : syncTitle} disabled={busy} onclick={sync}>
			<span class="sync-btn-icon" class:rotating={busy}><RefreshIcon size={12} /></span>
			<span class="sync-btn-label">{busy ? 'Syncing…' : 'Sync Changes'}</span>
			{#if !busy && branch.behind > 0}
				<span class="sync-btn-count">
					{branch.behind}<span class="sync-btn-arrow" aria-hidden="true">↓</span>
				</span>
			{/if}
			{#if !busy && branch.ahead > 0}
				<span class="sync-btn-count">
					{branch.ahead}<span class="sync-btn-arrow" aria-hidden="true">↑</span>
				</span>
			{/if}
		</button>
	{/if}
	{#if !mergeMode && canMergeDefault && defaultBranchShortName !== null}
		<!-- "Update from main" — separate button from sync because
		     the underlying op is `git merge origin/main` against the
		     *current* branch, not a pull of the current branch's
		     own upstream. We render it secondary (outlined, accent
		     text) so the primary sync action keeps the loud accent
		     fill when both buttons are visible at once (branch
		     ahead/behind upstream *and* main has new commits). -->
		<button
			type="button"
			class="merge-default-btn"
			title={merging
				? `Merging ${branch.defaultBranchRemoteRef ?? defaultBranchShortName} into current branch…`
				: `Merge ${branch.defaultBranchRemoteRef ?? defaultBranchShortName} into current branch (${branch.defaultBranchBehind} commit${branch.defaultBranchBehind === 1 ? '' : 's'})`}
			disabled={busy}
			onclick={mergeDefault}
		>
			<span class="merge-default-icon" aria-hidden="true">
				{#if merging}
					<!-- Replace the merge glyph with a spinner while
					     in flight rather than rotating the glyph
					     itself: a spinning merge icon reads as
					     "branching" half the time around the
					     rotation, which is the opposite of the
					     gesture. Spinner is the unambiguous "this
					     is busy" signal. -->
					<span class="spinner"></span>
				{:else}
					<MergeIcon size={12} />
				{/if}
			</span>
			<span class="merge-default-label"
				>{merging ? `Merging ${defaultBranchShortName}…` : `Update from ${defaultBranchShortName}`}</span
			>
			{#if !merging}
				<span class="merge-default-count">
					{branch.defaultBranchBehind}<span class="merge-default-arrow" aria-hidden="true">↓</span>
				</span>
			{/if}
		</button>
	{/if}
</section>

<style>
	.scm {
		display: flex;
		flex-direction: column;
		gap: 4px;
		padding: 6px 8px 8px;
		border-bottom: 1px solid var(--m-border);
		flex-shrink: 0;
	}
	.header {
		display: flex;
		align-items: center;
		gap: 6px;
		color: var(--m-fg-muted);
		font-size: 12px;
		min-width: 0;
	}
	.branch {
		flex: 1;
		min-width: 0;
		display: flex;
		align-items: center;
		gap: 6px;
		background: transparent;
		border: 1px solid transparent;
		border-radius: 4px;
		padding: 1px 4px;
		margin: -1px -4px;
		color: inherit;
		font: inherit;
		cursor: pointer;
		text-align: left;
	}
	.branch:hover {
		background: var(--m-bg-2);
		border-color: var(--m-border);
	}
	.branch:focus-visible {
		outline: 2px solid var(--m-accent);
		outline-offset: 1px;
	}
	.branch-icon {
		flex-shrink: 0;
		font-size: 12px;
		line-height: 1;
		opacity: 0.8;
	}
	.branch-name {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-family: var(--m-font-mono, monospace);
	}
	.actions {
		display: flex;
		align-items: center;
		gap: 2px;
		flex-shrink: 0;
	}
	.icon-btn {
		appearance: none;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 22px;
		height: 22px;
		/* Drop the user-agent button padding so the 22×22 box is
		   actually 22×22 of content. Without this the inner SVG
		   (a flex item with default `flex-shrink: 1`) collapses to
		   the leftover content area and renders as a sliver. */
		padding: 0;
		border: none;
		background: transparent;
		color: var(--m-fg-muted);
		cursor: pointer;
		font: inherit;
		font-size: 14px;
		line-height: 1;
		border-radius: 4px;
		flex-shrink: 0;
	}
	.icon-btn:hover:not(:disabled) {
		background: var(--m-bg-2);
		color: var(--m-fg);
	}
	.icon-btn:focus-visible {
		outline: 1px solid var(--m-accent);
		outline-offset: -1px;
	}
	.icon-btn:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}
	/* Pressed/toggled icon: the review button is the only caller
	   today, but the rule is generic so the next toggleable icon
	   in this panel inherits it for free. Match the accent fill
	   we use on `.compare-pill.active` so the two stay visually
	   consistent. */
	.icon-btn.active {
		background: color-mix(in srgb, var(--m-accent) 18%, transparent);
		color: var(--m-accent);
	}
	.icon-btn.active:hover:not(:disabled) {
		background: color-mix(in srgb, var(--m-accent) 26%, transparent);
		color: var(--m-accent);
	}
	/* Destructive icon — revert / discard / delete. Hover reveals
	   the danger color so the user gets a "this is destructive"
	   warning before clicking. The neutral resting colour avoids
	   permanently lighting up the panel like a fire-alarm; the
	   confirm dialog inside `discardPaths` is the actual safety
	   net. */
	.icon-btn.danger:hover:not(:disabled) {
		background: color-mix(in srgb, var(--m-danger) 14%, transparent);
		color: var(--m-danger);
	}
	/* Composer wrapper hosts the textarea plus the AI sparkle
	   inset in the top-right (chat-composer pattern). The
	   textarea stretches the full container; the sparkle is
	   absolutely positioned and the textarea reserves padding-right
	   only when the sparkle actually renders (signed-in users) so
	   the placeholder doesn't waste pixels for everyone else. */
	.composer {
		position: relative;
	}
	.input {
		appearance: none;
		display: block;
		width: 100%;
		box-sizing: border-box;
		min-height: 24px;
		max-height: 240px;
		padding: 4px 6px;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		background: var(--m-bg-1);
		color: var(--m-fg);
		font: inherit;
		font-size: 13px;
		line-height: 1.4;
		resize: none;
		/* `auto` toggled in JS only when scrollHeight exceeds
		   max-height — see `autoSize`. The off-by-one rendering
		   gap between scrollHeight and the actual textarea box is
		   what was painting a phantom scrollbar with `auto` left
		   on permanently. */
		overflow-y: hidden;
	}
	/* Top-right padding clears the AI sparkle (20px button + 4px
	   inset + a hair of slack). Only applied when the sparkle
	   actually renders so the placeholder doesn't waste pixels
	   for signed-out users. */
	.input.input-with-ai {
		padding-right: 28px;
	}
	.input::placeholder {
		color: var(--m-fg-subtle);
	}
	.input:focus-visible {
		outline: 1px solid var(--m-accent);
		outline-offset: 0;
		border-color: transparent;
	}
	.input:disabled {
		opacity: 0.6;
	}
	/* AI suggest-commit-message button, top-right of the textarea.
	   Same visual vocabulary as the new-branch suggest button so
	   a user who's seen one immediately recognises the other. */
	.composer-ai {
		appearance: none;
		position: absolute;
		top: 4px;
		right: 4px;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 20px;
		height: 20px;
		padding: 0;
		border: 1px solid transparent;
		border-radius: 4px;
		background: transparent;
		color: var(--m-fg-muted);
		font: inherit;
		font-size: 12px;
		line-height: 1;
		cursor: pointer;
	}
	.composer-ai:hover:not(:disabled) {
		background: var(--m-bg-2);
		color: var(--m-fg);
	}
	.composer-ai:focus-visible {
		outline: 1px solid var(--m-accent);
		outline-offset: -1px;
	}
	.composer-ai:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}
	/* Split-button row: main commit affordance + branch / amend
	   toggles, rendered as three siblings sharing one visual
	   border. Each child carries its own border + radius so the
	   keyboard focus ring stays per-child; the negative margin
	   collapses adjoining borders so the row reads as one unit. */
	.commit-row {
		display: flex;
		align-items: stretch;
		gap: 0;
		margin-top: 2px;
	}
	.commit-btn {
		appearance: none;
		flex: 1;
		min-width: 0;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		gap: 6px;
		min-height: 28px;
		padding: 4px 12px;
		border: 1px solid var(--m-accent);
		border-top-right-radius: 0;
		border-bottom-right-radius: 0;
		border-top-left-radius: 4px;
		border-bottom-left-radius: 4px;
		background: var(--m-accent);
		color: var(--m-bg);
		font: inherit;
		font-size: 12px;
		font-weight: 600;
		line-height: 1.2;
		cursor: pointer;
	}
	.commit-btn:hover:not(:disabled) {
		filter: brightness(1.1);
	}
	.commit-btn:focus-visible {
		outline: 2px solid var(--m-accent);
		outline-offset: 2px;
		position: relative;
		z-index: 1;
	}
	.commit-btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.commit-btn-label {
		flex-shrink: 0;
	}
	/* Toggles share the commit button's exact resting vocabulary —
	   accent border + accent fill + bg-coloured glyph — so the
	   three children read as one continuous CTA. The "active"
	   state then doesn't have to fight for visual loudness against
	   the loud sibling; instead it signals "I'm currently pressed"
	   with an inset ring in the panel bg colour, which reads
	   unambiguously even at a glance and works the same way in
	   light and dark themes (the ring is always on the contrasting
	   side of the accent). */
	.commit-btn-toggle {
		appearance: none;
		flex-shrink: 0;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 28px;
		min-height: 28px;
		padding: 0;
		border: 1px solid var(--m-accent);
		/* Collapse the adjoining border with the previous sibling
		   so the row visually reads as one button. */
		margin-left: -1px;
		border-radius: 0;
		background: var(--m-accent);
		color: var(--m-bg);
		font: inherit;
		font-size: 12px;
		line-height: 1;
		cursor: pointer;
	}
	.commit-btn-toggle:last-child {
		border-top-right-radius: 4px;
		border-bottom-right-radius: 4px;
	}
	/* Hover lifts the whole control with the same `filter:
	   brightness` the commit button uses, so all three siblings
	   share the same hover idiom. Works on both off and active
	   states — the active inset ring brightens with the rest of
	   the button. */
	.commit-btn-toggle:hover:not(:disabled) {
		filter: brightness(1.1);
	}
	.commit-btn-toggle:focus-visible {
		outline: 2px solid var(--m-accent);
		outline-offset: 2px;
		position: relative;
		z-index: 1;
	}
	/* Active state: keep the accent fill (so the row stays a
	   continuous CTA), but stamp an inset ring in the panel bg
	   colour around the glyph so the toggle reads as "pressed in"
	   / "selected". The double-line — outer accent border, inner
	   bg-coloured ring — is a familiar segmented-control idiom and
	   stays readable next to the loud commit button without
	   needing a different fill. */
	.commit-btn-toggle.active {
		box-shadow: inset 0 0 0 2px var(--m-bg);
	}
	.commit-btn-toggle:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	/* Inline "new branch" row that appears under the composer when
	   the user clicks the branch toggle. Sparkle (suggest) button
	   only renders when the coder is signed in — without HF auth
	   we couldn't call the inference router anyway. */
	.new-branch-row {
		display: flex;
		align-items: stretch;
		gap: 4px;
	}
	.new-branch-input {
		appearance: none;
		flex: 1;
		min-width: 0;
		box-sizing: border-box;
		height: 26px;
		padding: 0 6px;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		background: var(--m-bg-1);
		color: var(--m-fg);
		font: inherit;
		font-family: var(--m-font-mono, monospace);
		font-size: 12px;
		line-height: 1;
	}
	.new-branch-input::placeholder {
		color: var(--m-fg-subtle);
	}
	.new-branch-input:focus-visible {
		outline: 1px solid var(--m-accent);
		outline-offset: 0;
		border-color: transparent;
	}
	.new-branch-input:disabled {
		opacity: 0.6;
	}
	.new-branch-suggest {
		appearance: none;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 26px;
		height: 26px;
		padding: 0;
		border: 1px solid var(--m-border);
		border-radius: 4px;
		background: var(--m-bg-1);
		color: var(--m-fg-muted);
		cursor: pointer;
		flex-shrink: 0;
	}
	.new-branch-suggest:hover:not(:disabled) {
		background: var(--m-bg-2);
		color: var(--m-fg);
	}
	.new-branch-suggest:focus-visible {
		outline: 1px solid var(--m-accent);
		outline-offset: -1px;
	}
	.new-branch-suggest:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	/* Tiny inline spinner used by the suggest buttons while the
	   fast-model call is in flight. Same size + color treatment as
	   the SparklesIcon it replaces so the swap doesn't shift the
	   button height. `spinner-on-accent` is the variant for the
	   accent-filled commit button: the resting `currentColor` is
	   the dark accent text, which would disappear on the accent
	   background, so the variant inverts to the bg colour. */
	.spinner {
		display: inline-block;
		width: 12px;
		height: 12px;
		border: 1.5px solid currentColor;
		border-top-color: transparent;
		border-radius: 50%;
		animation: scm-spin 0.8s linear infinite;
	}
	.spinner-on-accent {
		border-color: var(--m-bg);
		border-top-color: transparent;
	}
	@keyframes scm-spin {
		to {
			transform: rotate(360deg);
		}
	}
	/* Pill badge in the header next to the branch name. Always
	   carries the change count; visible iff the user has changes
	   *or* the filter is on (so a count of 0 with the filter
	   active still surfaces the toggle for "go back to all"). The
	   accent fill makes it the loudest control on the panel —
	   matches the user's "more obvious color" ask. The active
	   state inverts to a hollow ring so the toggle reads as
	   "currently driving the tree" without changing colour
	   weight. */
	.changes-badge {
		appearance: none;
		display: inline-flex;
		align-items: center;
		justify-content: center;
		min-width: 20px;
		height: 20px;
		padding: 0 6px;
		border: 1px solid var(--m-accent);
		border-radius: 999px;
		background: var(--m-accent);
		/* `--m-bg` flips between near-black (dark theme) and
		   near-white (light theme), so it always contrasts well
		   against the accent fill — saves us inventing a new
		   token just for this badge. */
		color: var(--m-bg);
		font: inherit;
		font-size: 11px;
		font-weight: 600;
		line-height: 1;
		cursor: pointer;
		font-variant-numeric: tabular-nums;
		flex-shrink: 0;
	}
	.changes-badge:hover {
		filter: brightness(1.1);
	}
	.changes-badge:focus-visible {
		outline: 2px solid var(--m-accent);
		outline-offset: 2px;
	}
	.changes-badge.active {
		background: transparent;
		color: var(--m-accent);
	}
	/* Compare-against-default-branch pill, sitting next to the
	   change-count badge in the SCM header. Inactive state is a
	   muted outlined pill ("vs main" — click to compare); active
	   state mirrors the changes-badge's accent vocabulary. */
	.compare-pill {
		appearance: none;
		display: inline-flex;
		align-items: center;
		height: 20px;
		padding: 0 8px;
		border: 1px solid var(--m-border);
		border-radius: 999px;
		background: transparent;
		color: var(--m-fg-subtle);
		font-size: 10px;
		font-family: var(--m-font-mono);
		text-transform: lowercase;
		letter-spacing: 0.04em;
		cursor: pointer;
		white-space: nowrap;
	}
	.compare-pill:hover {
		color: var(--m-fg);
		border-color: var(--m-border-strong);
	}
	.compare-pill.active {
		background: var(--m-accent);
		border-color: var(--m-accent);
		color: var(--m-bg);
	}
	.compare-pill.active:hover {
		filter: brightness(0.95);
	}
	/* "Merging <ref>" pill, only present while
	   `workspace.gitMergeState.inProgress`. Warning palette
	   distinguishes it from the regular changes-count + compare
	   pills — the user is in a do-something-now state and the
	   chrome should make that obvious. Inert (no hover / click);
	   the actions live in the commit row below. */
	.merge-pill {
		display: inline-flex;
		align-items: center;
		height: 20px;
		padding: 0 8px;
		border: 1px solid color-mix(in srgb, var(--m-warning) 60%, transparent);
		border-radius: 999px;
		background: color-mix(in srgb, var(--m-warning) 14%, transparent);
		color: var(--m-warning);
		font-size: 10px;
		font-family: var(--m-font-mono);
		text-transform: lowercase;
		letter-spacing: 0.04em;
		white-space: nowrap;
	}
	/* Abort-merge button — sits in the commit-row toggle slot
	   during merge mode. Same footprint as the regular toggles
	   (so the row width stays stable when the panel flips
	   modes), but loud danger colouring so it can't be confused
	   with the harmless `✎` amend toggle. */
	.commit-btn-toggle.merge-abort {
		color: var(--m-danger);
	}
	.commit-btn-toggle.merge-abort:hover:not(:disabled) {
		background: color-mix(in srgb, var(--m-danger) 14%, transparent);
		color: var(--m-danger);
	}
	/* "N files still have unresolved conflicts" hint under the
	   merge-mode commit row. Quiet (small, muted) so it informs
	   without yelling — the buttons above already carry the
	   loud signal. */
	.merge-hint {
		margin-top: 6px;
		padding: 0 2px;
		font-size: 11px;
		color: var(--m-warning);
	}
	/* Full-width sync button beneath the composer — the unified
	   alternative to separate push/pull icons. Hidden when the
	   branch has nothing to push or pull (`syncLabel === null`).
	   Accent fill matches the badge's "this is the loud control
	   right now" vocabulary; the label rewrites itself based on
	   ahead/behind state ("Sync changes ↓N ↑M" / "Push N commits"
	   / "Pull N commits"). */
	.sync-btn {
		appearance: none;
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 6px;
		width: 100%;
		min-height: 26px;
		margin-top: 2px;
		padding: 4px 10px;
		border: 1px solid var(--m-accent);
		border-radius: 4px;
		background: var(--m-accent);
		color: var(--m-bg);
		font: inherit;
		font-size: 12px;
		font-weight: 600;
		line-height: 1.2;
		cursor: pointer;
		font-variant-numeric: tabular-nums;
	}
	.sync-btn-label {
		flex-shrink: 0;
	}
	/* Wrapper around the RefreshIcon so we can rotate the SVG via
	   a CSS class on the parent (the icon component itself doesn't
	   take a className). `rotating` is on while `busy` so the user
	   gets a visible "this is doing something" signal during the
	   pull / push roundtrip. */
	.sync-btn-icon {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		flex-shrink: 0;
	}
	.sync-btn-icon.rotating {
		animation: scm-spin 0.8s linear infinite;
	}
	/* Per-direction `N↓` / `M↑` chip. The number sits flush against
	   the arrow (no whitespace) so they read as a single token; the
	   gap on the parent flex spaces neighbouring chips apart. */
	.sync-btn-count {
		display: inline-flex;
		align-items: baseline;
		gap: 1px;
	}
	.sync-btn-arrow {
		font-weight: 700;
		opacity: 0.9;
	}
	.sync-btn:hover:not(:disabled) {
		filter: brightness(1.1);
	}
	.sync-btn:focus-visible {
		outline: 2px solid var(--m-accent);
		outline-offset: 2px;
	}
	.sync-btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	/* "Update from main" — secondary affordance, rendered outlined
	   rather than filled so when it stacks under a primary sync
	   button (branch ahead/behind upstream *and* main has new
	   commits) the eye reads the sync as the main CTA and merge
	   as the "also worth knowing" follow-up. When sync isn't
	   showing this button is alone in the slot and reads as the
	   primary call to action by default. Same shape + size as
	   `.sync-btn` so toggling between the two doesn't shift the
	   panel layout. */
	.merge-default-btn {
		appearance: none;
		display: flex;
		align-items: center;
		justify-content: center;
		gap: 6px;
		width: 100%;
		min-height: 26px;
		margin-top: 2px;
		padding: 4px 10px;
		border: 1px solid var(--m-accent);
		border-radius: 4px;
		background: transparent;
		color: var(--m-accent);
		font: inherit;
		font-size: 12px;
		font-weight: 600;
		line-height: 1.2;
		cursor: pointer;
		font-variant-numeric: tabular-nums;
	}
	.merge-default-icon {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		flex-shrink: 0;
		/* Horizontal flip — the Lucide `git-merge` glyph defaults
		   to "target spine on the left, source branch arcing in
		   from upper-right". For *this* specific button the
		   gesture is "merge main → current branch", and the
		   user reads the affordance left-to-right as `main → me`,
		   so we want main (the source) on the left and the
		   current-branch tip on the right. Mirroring with
		   `scaleX(-1)` is the cheapest way to get it without
		   shipping a second icon component, and it doesn't apply
		   while the spinner is showing because the spinner is
		   rotation-symmetric. */
		transform: scaleX(-1);
	}
	.merge-default-label {
		flex-shrink: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.merge-default-count {
		display: inline-flex;
		align-items: baseline;
		gap: 1px;
	}
	.merge-default-arrow {
		font-weight: 700;
		opacity: 0.9;
	}
	.merge-default-btn:hover:not(:disabled) {
		background: color-mix(in srgb, var(--m-accent) 12%, transparent);
	}
	.merge-default-btn:focus-visible {
		outline: 2px solid var(--m-accent);
		outline-offset: 2px;
	}
	.merge-default-btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
</style>
