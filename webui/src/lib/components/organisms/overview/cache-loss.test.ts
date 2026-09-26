import { describe, it, expect } from 'vitest';
import type { DailyCacheLoss } from '$lib/queries';
import { cacheLossRows, cacheLossTotals } from './cache-loss';

const day = (d: string, ttl: number, gw: number, unk: number): DailyCacheLoss => ({
	day: d,
	ttl_expired: ttl,
	gateway_rewrote_body: gw,
	unknown: unk,
	total: ttl + gw + unk,
	busts: 1
});

describe('cacheLossRows', () => {
	it('orders newest first and scales every reason against the costliest day', () => {
		const rows = cacheLossRows([day('2026-09-19', 1, 0, 1), day('2026-09-20', 2, 1, 1)]);
		expect(rows.map((r) => r.day)).toEqual(['2026-09-20', '2026-09-19']);
		expect(rows[0].widths).toEqual({ ttl_expired: 50, gateway_rewrote_body: 25, unknown: 25 });
		expect(rows[1].widths).toEqual({ ttl_expired: 25, gateway_rewrote_body: 0, unknown: 25 });
	});

	it('draws empty bars when nothing was lost', () => {
		expect(cacheLossRows([day('2026-09-20', 0, 0, 0)])[0].widths.ttl_expired).toBe(0);
		expect(cacheLossRows([])).toEqual([]);
	});
});

describe('cacheLossTotals', () => {
	it('sums each reason and the total over the range', () => {
		expect(cacheLossTotals([day('2026-09-19', 1, 0, 1), day('2026-09-20', 2, 1, 1)])).toEqual({
			ttl_expired: 3,
			gateway_rewrote_body: 1,
			unknown: 2,
			total: 6
		});
	});
});
