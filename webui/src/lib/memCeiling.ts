// Per-machine RAM ceiling for spawns (Settings › Resource monitoring) and the
// figures a `queued` session carries in `metadata.queued`. The server works in
// bytes; the UI edits and shows GiB, formatted per locale ("45,5 Go").
import type { SessionListItem } from '@bindings/SessionListItem';
import type { SpawnResponse } from '@bindings/SpawnResponse';
import { m } from '$lib/paraglide/messages';
import { getLocale, type Locale } from '$lib/paraglide/runtime';
import { toasts } from '$lib/toast.svelte';

export const GIB = 1024 ** 3;
/** The server refuses a ceiling below one session's estimate (1.5 GiB). */
export const MIN_CEILING_BYTES = 1.5 * GIB;

export type CeilingParse =
	| { ok: true; bytes: number | null }
	| { ok: false; reason: 'invalid' | 'too_small' };

/** Parse what the user typed in GiB. Empty = no ceiling (`null`); a comma is
 *  accepted as the decimal separator. */
export function parseCeilingGiB(raw: string): CeilingParse {
	const t = raw.replace(/\s+/g, '').replace(',', '.');
	if (t === '') return { ok: true, bytes: null };
	if (!/^\d+(\.\d+)?$/.test(t)) return { ok: false, reason: 'invalid' };
	const bytes = Math.round(Number(t) * GIB);
	if (!Number.isFinite(bytes)) return { ok: false, reason: 'invalid' };
	if (bytes < MIN_CEILING_BYTES) return { ok: false, reason: 'too_small' };
	return { ok: true, bytes };
}

/** Bytes as a GiB number for the locale, at most one decimal ("45", "47,2"). */
export function gibNumber(bytes: number, locale: Locale = getLocale()): string {
	return new Intl.NumberFormat(locale, { maximumFractionDigits: 1 }).format(bytes / GIB);
}

/** Bytes as "47,2 Go" / "47.2 GB". */
export function fmtGiB(bytes: number, locale: Locale = getLocale()): string {
	return m.mem_gib({ value: gibNumber(bytes, locale) }, { locale });
}

/** The value the ceiling input starts from: empty for no ceiling. */
export function ceilingInputValue(bytes: number | null, locale: Locale = getLocale()): string {
	if (bytes === null) return '';
	// No grouping: an English "1,024" would read back as 1.024 GiB.
	return new Intl.NumberFormat(locale, { maximumFractionDigits: 2, useGrouping: false }).format(
		bytes / GIB
	);
}

/** The figures that held a queued spawn back, from `metadata.queued`. */
export interface QueuedFigures {
	mem_used_bytes: number;
	mem_total_bytes: number;
	recent_bytes: number;
	ceiling_bytes: number;
	estimate_bytes: number;
	checked_at: string | null;
}

const num = (v: unknown): number | null =>
	typeof v === 'number' && Number.isFinite(v) ? v : null;

export function queuedFigures(s: Pick<SessionListItem, 'metadata'>): QueuedFigures | null {
	const md = s.metadata as Record<string, unknown> | null;
	const q = md?.queued;
	if (!q || typeof q !== 'object') return null;
	const r = q as Record<string, unknown>;
	const used = num(r.mem_used_bytes);
	const ceiling = num(r.ceiling_bytes);
	if (used === null || ceiling === null) return null;
	return {
		mem_used_bytes: used,
		mem_total_bytes: num(r.mem_total_bytes) ?? 0,
		recent_bytes: num(r.recent_bytes) ?? 0,
		ceiling_bytes: ceiling,
		estimate_bytes: num(r.estimate_bytes) ?? 0,
		checked_at: typeof r.checked_at === 'string' ? r.checked_at : null
	};
}

/** "47,2 Go utilisés (+3 Go lancés à l'instant) · plafond 45 Go". */
export function queuedSummary(f: QueuedFigures, locale: Locale = getLocale()): string {
	const opts = { locale };
	const used = fmtGiB(f.mem_used_bytes, locale);
	const ceiling = fmtGiB(f.ceiling_bytes, locale);
	return f.recent_bytes > 0
		? m.queued_figures_with_recent({ used, recent: fmtGiB(f.recent_bytes, locale), ceiling }, opts)
		: m.queued_figures({ used, ceiling }, opts);
}

/** Card / banner line for a queued session: its figures, or the bare reason
 *  when the row carries none. */
export function queuedPreview(s: Pick<SessionListItem, 'metadata'>): string {
	const f = queuedFigures(s);
	return f ? queuedSummary(f) : m.queued_reason();
}

/** The server held the spawn back instead of dispatching it: there is no
 *  command result to wait for. */
export const isQueuedSpawn = (res: Pick<SpawnResponse, 'status'>): boolean =>
	res.status === 'queued';

/** Long enough to reach the toast's "open" action. */
const QUEUED_TOAST_MS = 8_000;

/** Tell the user a spawn was held back instead of failing, with a way to its
 *  queued session page when the server named one. */
export function notifyQueuedSpawn(
	res: Pick<SpawnResponse, 'session_id'>,
	open?: (sessionId: string) => void
): void {
	const sid = res.session_id ?? null;
	toasts.info(
		m.spawn_toast_queued(),
		QUEUED_TOAST_MS,
		sid && open ? { label: m.spawn_toast_queued_open(), run: () => open(sid) } : undefined
	);
}
