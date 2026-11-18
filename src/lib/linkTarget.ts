// Which web address a right-click is on, for the app-wide link menu
// (`LinkMenu.svelte`).

/** `value` when it is an http(s) address, else `null`. */
export function webHref(value: string | null | undefined): string | null {
	const text = value?.trim() ?? '';
	return /^https?:\/\/\S+$/i.test(text) ? text : null;
}

/** The web address under `target`: the nearest link's, or that of a
 *  control standing in for one (`data-href`, as a tool row's URL
 *  button carries). Links to files and in-page anchors are not ours. */
export function linkAt(target: EventTarget | null): string | null {
	if (!(target instanceof Element)) {
		return null;
	}
	const holder = target.closest('a[href], [data-href]');
	if (holder === null) {
		return null;
	}
	return webHref(holder.getAttribute('data-href') ?? holder.getAttribute('href'));
}
