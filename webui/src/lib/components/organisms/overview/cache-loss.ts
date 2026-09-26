import type { DailyCacheLoss } from '$lib/queries';

export const CACHE_LOSS_DAYS = 7;

export const CACHE_LOSS_REASONS = ['ttl_expired', 'gateway_rewrote_body', 'unknown'] as const;
export type CacheLossReason = (typeof CACHE_LOSS_REASONS)[number];

export interface CacheLossRow {
	day: string;
	total: number;
	/** Each reason's share of the widest day's bar, in percent. */
	widths: Record<CacheLossReason, number>;
}

/** Newest day first, bars scaled against the costliest day. */
export function cacheLossRows(days: DailyCacheLoss[]): CacheLossRow[] {
	const peak = Math.max(0, ...days.map((d) => d.total));
	return [...days]
		.sort((a, b) => b.day.localeCompare(a.day))
		.map((d) => ({
			day: d.day,
			total: d.total,
			widths: Object.fromEntries(
				CACHE_LOSS_REASONS.map((r) => [r, peak > 0 ? (d[r] / peak) * 100 : 0])
			) as Record<CacheLossReason, number>
		}));
}

/** Range totals per reason, plus the grand total. */
export function cacheLossTotals(days: DailyCacheLoss[]): Record<CacheLossReason | 'total', number> {
	const out = { ttl_expired: 0, gateway_rewrote_body: 0, unknown: 0, total: 0 };
	for (const d of days) {
		for (const r of CACHE_LOSS_REASONS) out[r] += d[r];
		out.total += d.total;
	}
	return out;
}
