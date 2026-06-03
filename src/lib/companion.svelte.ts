// Desktop Companion panel state (Phase 13.4b). Holds whether the
// pairing modal is open. The modal itself polls `ipc.companion.status`
// for the bridge's published pairing payload + device list.

class CompanionState {
	modalOpen = $state(false);

	open(): void {
		this.modalOpen = true;
	}

	close(): void {
		this.modalOpen = false;
	}

	toggle(): void {
		this.modalOpen = !this.modalOpen;
	}
}

export const companion = new CompanionState();
