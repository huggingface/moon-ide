// Desktop Companion panel state (Phase 13.4b). Holds whether the
// pairing modal is open, plus a lightly-polled snapshot of the
// bridge's status so the status-bar item can show a live pip
// (running? how many devices paired?) without opening the modal.

import { ipc, type CompanionStatus, type RemoteBridgeStatus } from './ipc';

class CompanionState {
	modalOpen = $state(false);
	/** Remote-bridge enroll modal open (Phase 14.3). */
	remoteModalOpen = $state(false);
	/** Latest bridge status, or null before the first poll. */
	status = $state<CompanionStatus | null>(null);
	/** Latest remote-bridge connection status (Phase 14.3). */
	remoteStatus = $state<RemoteBridgeStatus | null>(null);

	#pollTimer: ReturnType<typeof setInterval> | null = null;
	#refs = 0;

	open(): void {
		this.modalOpen = true;
	}

	close(): void {
		this.modalOpen = false;
	}

	openRemote(): void {
		this.remoteModalOpen = true;
		void this.refreshRemote();
	}

	closeRemote(): void {
		this.remoteModalOpen = false;
	}

	toggle(): void {
		this.modalOpen = !this.modalOpen;
	}

	get running(): boolean {
		return this.status?.running ?? false;
	}

	get deviceCount(): number {
		return this.status?.devices.length ?? 0;
	}

	/** This IDE holds a live connection to a remote relay bridge. */
	get remoteConnected(): boolean {
		return this.remoteStatus?.connected ?? false;
	}

	/** The remote connection exists but is currently failing. */
	get remoteErrored(): boolean {
		return !this.remoteConnected && Boolean(this.remoteStatus?.error);
	}

	/**
	 * Something is actually usable, not just "a process exists": the
	 * remote relay link is up, or the local bridge has at least one
	 * paired phone. The status-bar pip goes green on this — a running
	 * bridge with nothing paired stays neutral.
	 */
	get active(): boolean {
		return this.remoteConnected || (this.running && this.deviceCount > 0);
	}

	/** Count of phone WS sessions currently connected to the bridge.
	 * From the remote relay's `Register` reply (remote mode) or the
	 * local bridge's control-socket status (local mode). */
	get connectedPhoneCount(): number {
		return this.remoteStatus?.connected_phones ?? this.status?.connected_phones ?? 0;
	}

	async refresh(): Promise<void> {
		try {
			this.status = await ipc.companion.status();
		} catch {
			this.status = null;
		}
		// The pip reflects the remote relay link too, so the poll keeps
		// both halves fresh (not just when the modal opens).
		await this.refreshRemote();
	}

	async refreshRemote(): Promise<void> {
		try {
			this.remoteStatus = await ipc.companion.remoteStatus();
		} catch {
			this.remoteStatus = null;
		}
	}

	/**
	 * Begin polling the bridge status. Ref-counted so the status bar
	 * and the open modal can both ask for it without fighting; polling
	 * stops when the last caller releases. The poll is a cheap local
	 * file read every few seconds.
	 */
	startPolling(): void {
		this.#refs += 1;
		if (this.#pollTimer !== null) {
			return;
		}
		void this.refresh();
		this.#pollTimer = setInterval(() => void this.refresh(), 4000);
	}

	stopPolling(): void {
		this.#refs = Math.max(0, this.#refs - 1);
		if (this.#refs === 0 && this.#pollTimer !== null) {
			clearInterval(this.#pollTimer);
			this.#pollTimer = null;
		}
	}
}

export const companion = new CompanionState();
