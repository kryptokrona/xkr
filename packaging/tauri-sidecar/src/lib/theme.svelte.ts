// Theme, as in Aesir: "I'm blue" (blue), "Hacker" (dark), "Flashbang" (flashbang), kept per machine in localStorage.
export type Theme = 'dark' | 'blue' | 'flashbang';
export const THEMES: { id: Theme; label: string; swatch: string; bg: string }[] = [
	{ id: 'blue', label: "I'm blue", swatch: '#5a8bdb', bg: '#1a1c23' },
	{ id: 'dark', label: 'Hacker', swatch: '#0eff6b', bg: '#121212' },
	{ id: 'flashbang', label: 'Flashbang', swatch: '#ff5e00', bg: '#fcfcfc' }
];

const KEY = 'xkr-theme';

class ThemeState {
	current = $state<Theme>('dark');

	init() {
		try {
			const t = localStorage.getItem(KEY) as Theme | null;
			if (t && THEMES.some((x) => x.id === t)) this.current = t;
		} catch {
			// no storage: keep the default
		}
		this.#apply();
	}

	set(t: Theme) {
		this.current = t;
		try {
			localStorage.setItem(KEY, t);
		} catch {
			// not kept, still applied
		}
		this.#apply();
	}

	get dark() {
		return this.current !== 'flashbang';
	}

	#apply() {
		const c = document.documentElement.classList;
		c.toggle('dark', this.current !== 'flashbang');
		c.toggle('blue', this.current === 'blue');
	}
}

export const theme = new ThemeState();
