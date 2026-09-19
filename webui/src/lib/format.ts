import { getLocale } from './paraglide/runtime';

/** Compact token count: 1234 → "1.2k", 1_200_000 → "1.2M". */
export function compact(n: number): string {
	if (n < 1000) return String(n);
	if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
	return `${(n / 1_000_000).toFixed(1)}M`;
}

/** Relative time from an ISO datetime, in the active locale ("3m ago" / "il y a
 *  3 min"); null → "". Falls back to a localized date past ~30 days. */
export function relativeTime(iso: string | null | undefined): string {
	if (!iso) return '';
	const then = new Date(iso).getTime();
	if (Number.isNaN(then)) return '';
	const rtf = new Intl.RelativeTimeFormat(getLocale(), { numeric: 'always', style: 'narrow' });
	const secs = Math.max(0, Math.floor((Date.now() - then) / 1000));
	if (secs < 60) return rtf.format(-secs, 'second');
	const mins = Math.floor(secs / 60);
	if (mins < 60) return rtf.format(-mins, 'minute');
	const hrs = Math.floor(mins / 60);
	if (hrs < 24) return rtf.format(-hrs, 'hour');
	const days = Math.floor(hrs / 24);
	if (days < 30) return rtf.format(-days, 'day');
	return new Date(iso).toLocaleDateString(getLocale());
}

/** Human uptime from seconds: "2d 3h", "4h 10m", "12m". */
export function uptime(secs: number): string {
	const d = Math.floor(secs / 86400);
	const h = Math.floor((secs % 86400) / 3600);
	const m = Math.floor((secs % 3600) / 60);
	if (d > 0) return `${d}d ${h}h`;
	if (h > 0) return `${h}h ${m}m`;
	return `${m}m`;
}

/** Map a session status to a badge color class (active = green, etc.). */
export function statusBadgeTone(status: string): 'ok' | 'info' | 'warn' | 'danger' | 'neutral' {
	switch (status) {
		case 'queued':
			return 'warn';
		case 'active':
			return 'ok';
		case 'new':
			return 'info';
		case 'archived':
			return 'danger';
		default:
			return 'neutral';
	}
}

/** Short model label for the detailed footer: drop the vendor prefix
 *  ("claude-opus-4-8" → "opus-4-8", "gpt-5-codex" stays) so a long id stops
 *  shoving the provider logo out of the row. Pay-per-token providers qualify
 *  their ids with a path ("fireworks-ai/accounts/fireworks/models/kimi-k3");
 *  only the last segment names the model. */
export function modelShort(model: string): string {
	const leaf = model.split('/').filter(Boolean).pop() ?? model;
	return leaf.replace(/^(claude|anthropic)-/i, '');
}

/** Model FAMILY only — the one word that survives in the compact list row
 *  ("claude-opus-4-8" → "opus", "gpt-5-codex" → "gpt"). Falls back to the first
 *  segment of the (prefix-stripped) id for engines we don't enumerate. */
const FAMILIES = ['opus', 'sonnet', 'haiku', 'fable', 'gpt', 'o1', 'o3', 'o4', 'gemini', 'grok'];

export function modelFamily(model: string): string {
	const m = model.toLowerCase();
	for (const fam of FAMILIES) {
		if (m.includes(fam)) return fam;
	}
	return modelShort(model).split(/[-\s]/)[0] || model;
}

/** Codex codenames all share the "gpt" family word, so they are matched ahead of
 *  it — otherwise Sol, Terra, Luna and Astra are one indistinguishable label. */
const CODENAMES = ['sol', 'terra', 'luna', 'astra'];

const VOCABULARY = [...FAMILIES, ...CODENAMES];

/** Shortest prefix, two letters up, unique across VOCABULARY: Sonnet and Sol
 *  both start "so" and would otherwise render identically. */
function distinctPrefix(word: string): string {
	let n = 2;
	while (n < word.length && VOCABULARY.some((w) => w !== word && w.startsWith(word.slice(0, n)))) n++;
	return word.slice(0, n);
}

/** The narrowest a model can still be named in ("claude-opus-4-8" → "Op.",
 *  "gpt-5.6-terra" → "Te.", "claude-sonnet-5" → "Son."). */
export function modelAbbrev(model: string): string {
	const m = model.toLowerCase();
	const word = CODENAMES.find((c) => new RegExp(`(^|[^a-z])${c}([^a-z]|$)`).test(m)) ?? modelFamily(model);
	if (word === 'gpt') return 'GPT';
	return `${distinctPrefix(word).replace(/^./, (c) => c.toUpperCase())}.`;
}

/** The machine badge's smallest legible form. A fleet numbers its machines
 *  (`ci-runner-01`, `dev2`), so a bare first letter renders them identically;
 *  any trailing number is kept, without its padding. */
export function machineInitial(label: string): string {
	const head = label.match(/[a-z0-9]/i)?.[0];
	if (!head) return '?';
	const tail = label.match(/(\d+)\s*$/)?.[1];
	return tail === undefined ? head.toUpperCase() : `${head.toUpperCase()}${Number(tail)}`;
}

export function usd(n: number): string {
	if (!Number.isFinite(n) || n <= 0) return '$0.00';
	if (n < 0.01) return `$${n.toFixed(4)}`;
	if (n < 100) return `$${n.toFixed(2)}`;
	return `$${Math.round(n)}`;
}

/** Deterministic accent color for a machine label (badge tinting). */
export function hashHue(s: string): number {
	let h = 0;
	for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) >>> 0;
	return h % 360;
}
