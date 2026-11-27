// An editable copy of the settings for a page, applied with one restart of xkrd.
import { toast } from 'svelte-sonner';
import type { Settings } from './api.js';
import { live } from './live.svelte.js';

const clone = (s: Settings): Settings => JSON.parse(JSON.stringify(s));

export class Draft {
	value = $state<Settings | null>(null);

	/** Call during component setup. */
	constructor() {
		$effect(() => {
			if (!this.value && live.info) this.value = clone(live.info.settings);
		});
	}

	get dirty(): boolean {
		return !!this.value && !!live.info && JSON.stringify(this.value) !== JSON.stringify(live.info.settings);
	}

	reset() {
		if (live.info) this.value = clone(live.info.settings);
	}

	async apply(done = 'Settings applied') {
		if (!this.value) return;
		const id = toast.loading('Restarting the node with the new settings…');
		try {
			await live.apply(this.value);
			toast.success(done, { id });
		} catch (e) {
			toast.error(String(e), { id, duration: 12000 });
		}
		this.reset();
	}
}
