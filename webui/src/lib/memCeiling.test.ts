import { describe, expect, it } from 'vitest';
import {
	GIB,
	MIN_CEILING_BYTES,
	ceilingInputValue,
	fmtGiB,
	isQueuedSpawn,
	parseCeilingGiB,
	queuedFigures,
	queuedPreview,
	queuedSummary
} from './memCeiling';

describe('parseCeilingGiB', () => {
	it('reads empty as no ceiling', () => {
		expect(parseCeilingGiB('')).toEqual({ ok: true, bytes: null });
		expect(parseCeilingGiB('   ')).toEqual({ ok: true, bytes: null });
	});

	it('reads GiB with a dot or a comma', () => {
		expect(parseCeilingGiB('45')).toEqual({ ok: true, bytes: 45 * GIB });
		expect(parseCeilingGiB('45,5')).toEqual({ ok: true, bytes: 45.5 * GIB });
		expect(parseCeilingGiB(' 1.5 ')).toEqual({ ok: true, bytes: MIN_CEILING_BYTES });
	});

	it('refuses less than one session and anything not a number', () => {
		expect(parseCeilingGiB('1.4')).toEqual({ ok: false, reason: 'too_small' });
		expect(parseCeilingGiB('0')).toEqual({ ok: false, reason: 'too_small' });
		expect(parseCeilingGiB('-4')).toEqual({ ok: false, reason: 'invalid' });
		expect(parseCeilingGiB('abc')).toEqual({ ok: false, reason: 'invalid' });
		expect(parseCeilingGiB('4,5,6')).toEqual({ ok: false, reason: 'invalid' });
	});
});

describe('ceilingInputValue', () => {
	it('is empty without a ceiling and round-trips through the parser', () => {
		expect(ceilingInputValue(null, 'fr')).toBe('');
		expect(ceilingInputValue(45.5 * GIB, 'fr')).toBe('45,5');
		expect(ceilingInputValue(2048 * GIB, 'en')).toBe('2048');
		expect(parseCeilingGiB(ceilingInputValue(2048 * GIB, 'en'))).toEqual({
			ok: true,
			bytes: 2048 * GIB
		});
	});
});

describe('fmtGiB', () => {
	it('formats per locale with one decimal at most', () => {
		expect(fmtGiB(47.23 * GIB, 'fr')).toBe('47,2 Go');
		expect(fmtGiB(47.23 * GIB, 'en')).toBe('47.2 GB');
		expect(fmtGiB(45 * GIB, 'fr')).toBe('45 Go');
	});
});

const queued = (q: unknown) => ({ metadata: { queued: q } as never });

describe('queuedFigures / queuedSummary', () => {
	const figures = {
		mem_used_bytes: 47.2 * GIB,
		mem_total_bytes: 64 * GIB,
		recent_bytes: 3 * GIB,
		ceiling_bytes: 45 * GIB,
		estimate_bytes: 1.5 * GIB,
		checked_at: '2026-09-19T10:00:00Z'
	};

	it('reads the figures off the row metadata', () => {
		expect(queuedFigures(queued(figures))).toEqual(figures);
		expect(queuedFigures({ metadata: null })).toBeNull();
		expect(queuedFigures(queued({ mem_used_bytes: 'x', ceiling_bytes: 1 }))).toBeNull();
	});

	it('says what is used, what just launched and the ceiling', () => {
		expect(queuedSummary(figures, 'fr')).toBe(
			"47,2 Go utilisés (+3 Go lancés à l'instant) · plafond 45 Go"
		);
		expect(queuedSummary({ ...figures, recent_bytes: 0 }, 'fr')).toBe(
			'47,2 Go utilisés · plafond 45 Go'
		);
		expect(queuedSummary(figures, 'en')).toBe(
			'47.2 GB in use (+3 GB just launched) · ceiling 45 GB'
		);
	});

	it('falls back to the bare reason without figures', () => {
		expect(queuedPreview({ metadata: {} as never })).toBe(
			'Waiting for RAM: the machine is over its ceiling'
		);
	});
});

describe('isQueuedSpawn', () => {
	it('tells a held-back spawn from a dispatched one', () => {
		expect(isQueuedSpawn({ status: 'queued' })).toBe(true);
		expect(isQueuedSpawn({ status: 'dispatched' })).toBe(false);
	});
});
