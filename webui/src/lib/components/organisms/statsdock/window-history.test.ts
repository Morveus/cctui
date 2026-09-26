import { describe, it, expect } from 'vitest';
import type { UsageHistorySample } from '$lib/queries';
import { polyline, wastedPct, windowLines } from './window-history';

const sample = (sampledAt: string, resetsAt: string, utilization: number): UsageHistorySample => ({
	window_key: 'session',
	utilization,
	amount_usd: null,
	resets_at: resetsAt,
	sampled_at: sampledAt,
	source: 'poll'
});

describe('windowLines', () => {
	it('groups samples by window instance and places them on the elapsed fraction', () => {
		const lines = windowLines(
			[
				sample('2026-09-11T14:00:00Z', '2026-09-11T17:00:00.300Z', 30),
				sample('2026-09-11T09:30:00Z', '2026-09-11T12:00:00Z', 50),
				sample('2026-09-11T12:30:00Z', '2026-09-11T17:00:00Z', 10),
				{ ...sample('2026-09-11T12:30:00Z', '2026-09-11T17:00:00Z', 99), window_key: 'weekly_all' }
			],
			'session'
		);
		expect(lines.map((l) => l.resetsAt)).toEqual(['2026-09-11T12:00:00Z', '2026-09-11T17:00:00Z']);
		expect(lines[0].points).toEqual([{ x: 0.5, y: 50 }]);
		expect(lines[1].points.map((p) => p.y)).toEqual([10, 30]);
		expect(lines[1].points[0].x).toBeCloseTo(0.1);
	});

	it('ignores unknown windows and samples without a reset', () => {
		expect(windowLines([sample('2026-09-11T14:00:00Z', '2026-09-11T17:00:00Z', 1)], 'nope')).toEqual([]);
		expect(windowLines([{ ...sample('2026-09-11T14:00:00Z', '', 1), resets_at: null }], 'session')).toEqual([]);
	});
});

describe('polyline', () => {
	it('maps to a 100x100 viewBox and caps overage', () => {
		expect(polyline({ resetsAt: '', points: [{ x: 0.5, y: 25 }, { x: 1, y: 120 }] })).toBe('50.00,75.00 100.00,0.00');
	});
});

describe('wastedPct', () => {
	it('rounds the mean for the requested window and is null when nothing closed', () => {
		const summary = [{ window_key: 'session', windows: 3, mean_wasted_pct: 37.6 }];
		expect(wastedPct(summary, 'session')).toBe(38);
		expect(wastedPct(summary, 'weekly_all')).toBeNull();
		expect(wastedPct(undefined, 'session')).toBeNull();
	});
});
