import type { TokenUsage } from '@bindings/TokenUsage';

/** When Σ renders: always, only in the compact form (Σ + $ is that
 *  form, so a `showSum={false}` mount still needs it there), or never (no total). */
export type SumMode = 'always' | 'compact-only' | 'never';

export interface TokenUsageLayout {
	total: number;
	cacheTotal: number;
	cost: number;
	sumMode: SumMode;
	showCache: boolean;
	showCost: boolean;
	showCold: boolean;
	showBust: boolean;
}

export function tokenUsageLayout(
	usage: TokenUsage,
	{ sum = null, showSum = true, cold = false }: { sum?: number | null; showSum?: boolean; cold?: boolean } = {}
): TokenUsageLayout {
	const cacheTotal = Number(usage.cache_read_tokens) + Number(usage.cache_creation_tokens);
	const total = sum ?? Number(usage.tokens_in) + Number(usage.tokens_out) + cacheTotal;
	const cost = Number(usage.cost_usd) || 0;
	return {
		total,
		cacheTotal,
		cost,
		sumMode: total > 0 ? (showSum ? 'always' : 'compact-only') : 'never',
		showCache: cacheTotal > 0,
		showCost: cost > 0,
		showCold: cold,
		showBust: Boolean(usage.cache_bust)
	};
}

/** Reason code → the i18n key naming it. Unrecognised codes read as unknown. */
export function bustReasonKey(reason: string): 'ttl_expired' | 'gateway_rewrote_body' | 'unknown' {
	return reason === 'ttl_expired' || reason === 'gateway_rewrote_body' ? reason : 'unknown';
}

export function tokenUsageTitle(
	usage: TokenUsage,
	layout: TokenUsageLayout,
	fmt: { num: (n: number) => string; usd: (n: number) => string }
): string {
	const parts = [
		layout.sumMode === 'never' ? '' : `Σ${fmt.num(layout.total)}`,
		`↑${fmt.num(Number(usage.tokens_in))}`,
		`↓${fmt.num(Number(usage.tokens_out))}`,
		layout.showCache ? `⚡${fmt.num(layout.cacheTotal)}` : '',
		layout.showCost ? fmt.usd(layout.cost) : '',
		layout.showCold ? '❄️' : '',
		layout.showBust ? '💥' : ''
	];
	return parts.filter(Boolean).join(' ');
}
