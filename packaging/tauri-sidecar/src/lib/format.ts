// Number and text formatting for the UI.

/** Atomic units per XKR (CRYPTONOTE_DISPLAY_DECIMAL_POINT = 5). */
export const ATOMIC = 100_000;
export const DECIMALS = 5;

/** `123456` atomic units as `1.23456`. */
export function xkr(atomic: number | null | undefined, decimals = DECIMALS): string {
	if (typeof atomic !== 'number' || !Number.isFinite(atomic)) return '–';
	return (atomic / ATOMIC).toLocaleString('en-US', {
		minimumFractionDigits: decimals,
		maximumFractionDigits: decimals
	});
}

/** A typed amount (`1.5`, `1,000.25`) in atomic units, or `null` if it does not parse. */
export function parseXkr(text: string): number | null {
	const t = text.replace(/[\s,]/g, '');
	if (!/^\d+(\.\d{0,5})?$/.test(t)) return null;
	const [whole, frac = ''] = t.split('.');
	const atomic = Number(whole) * ATOMIC + Number(frac.padEnd(DECIMALS, '0'));
	return Number.isSafeInteger(atomic) ? atomic : null;
}

export function int(n: number | null | undefined): string {
	return typeof n === 'number' ? n.toLocaleString('en-US') : '–';
}

export function rate(h: number | null | undefined): string {
	if (typeof h !== 'number') return '–';
	const units = ['H/s', 'kH/s', 'MH/s', 'GH/s', 'TH/s'];
	let i = 0;
	while (h >= 1000 && i < units.length - 1) {
		h /= 1000;
		i++;
	}
	return `${h.toFixed(i === 0 ? 0 : 2)} ${units[i]}`;
}

export function bytes(n: number | null | undefined): string {
	if (typeof n !== 'number') return '–';
	const units = ['B', 'KB', 'MB', 'GB', 'TB'];
	let i = 0;
	while (n >= 1000 && i < units.length - 1) {
		n /= 1000;
		i++;
	}
	return `${n.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

export function duration(secs: number | null | undefined): string {
	if (typeof secs !== 'number') return '–';
	const d = Math.floor(secs / 86400);
	const h = Math.floor((secs % 86400) / 3600);
	const m = Math.floor((secs % 3600) / 60);
	if (d) return `${d}d ${h}h`;
	if (h) return `${h}h ${m}m`;
	return `${m}m ${Math.floor(secs % 60)}s`;
}

export function short(s: string | null | undefined, keep = 8): string {
	if (!s) return '–';
	return s.length > keep * 2 + 1 ? `${s.slice(0, keep)}…${s.slice(-keep)}` : s;
}

export function date(unix: number): string {
	if (!unix) return 'pending';
	return new Date(unix * 1000).toLocaleString(undefined, {
		year: 'numeric',
		month: 'short',
		day: 'numeric',
		hour: '2-digit',
		minute: '2-digit'
	});
}

export function percent(x: number, digits = 1): string {
	return `${(Math.min(1, Math.max(0, x)) * 100).toFixed(digits)} %`;
}
