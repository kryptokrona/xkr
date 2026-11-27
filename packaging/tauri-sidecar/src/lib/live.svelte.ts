// Polls the node and the wallet every 2 s; every page reads from here.
import { api, type NodeInfo, type NodeStatus, type WalletOverview } from './api.js';

class Live {
	info = $state<NodeInfo | null>(null);
	status = $state<NodeStatus | null>(null);
	wallet = $state<WalletOverview>({ open: false });
	/** A settings change (xkrd restart) is running. */
	applying = $state(false);
	error = $state<string | null>(null);
	loaded = $state(false);
	/** Seed words of a wallet just created, for the backup dialog. */
	backup = $state<{ address: string; mnemonic: string } | null>(null);
	#timer: ReturnType<typeof setInterval> | null = null;

	start() {
		if (this.#timer) return;
		void this.refresh();
		this.#timer = setInterval(() => void this.refresh(), 2000);
	}

	async refresh() {
		try {
			const info = await api.nodeInfo();
			this.info = info;
			this.status = info.phase === 'running' ? await api.nodeStatus().catch(() => null) : null;
			this.wallet = await api.walletOverview();
			this.error = null;
		} catch (e) {
			this.error = String(e);
		} finally {
			this.loaded = true;
		}
	}

	/** Applies settings (restarts xkrd when they changed). */
	async apply(next: import('./api.js').Settings) {
		this.applying = true;
		try {
			await api.updateSettings(next);
		} finally {
			this.applying = false;
			await this.refresh();
		}
	}

	get phase(): string {
		if (this.applying && this.info?.phase === 'running') return 'restarting';
		return this.info?.phase ?? 'starting';
	}
}

export const live = new Live();
