import { describe, expect, it } from 'vitest';
import {
	DEFAULT_MAX_TICKS,
	defaultIntervalSecs,
	formatIntervalSecs,
	intervalOptionsSecs,
	isCacheWarm,
	keepaliveActive,
	looksKeepaliveTick,
	warmUntilMs
} from './keepalive';

const T0 = Date.UTC(2026, 8, 24, 12, 0, 0);
const iso = (ms: number) => new Date(ms).toISOString();

describe('defaultIntervalSecs', () => {
	it('is the provider TTL minus two minutes', () => {
		expect(defaultIntervalSecs('claude-code', 'claude-opus-5')).toBe(58 * 60);
		expect(defaultIntervalSecs('codex', 'gpt-5.6-codex')).toBe(28 * 60);
		expect(defaultIntervalSecs('codex', 'gpt-5.5')).toBe(3 * 60);
		expect(defaultIntervalSecs(null, null)).toBe(3 * 60);
	});

	it('is always offered by the picker, which never exceeds the TTL', () => {
		for (const [a, mdl] of [
			['claude-code', null],
			['codex', 'gpt-5.6'],
			['codex', null]
		] as const) {
			const opts = intervalOptionsSecs(a, mdl);
			expect(opts).toContain(defaultIntervalSecs(a, mdl));
			expect([...opts].sort((x, y) => x - y)).toEqual(opts);
			expect(new Set(opts).size).toBe(opts.length);
		}
		expect(intervalOptionsSecs('codex', null)).toEqual([3 * 60]);
	});
});

describe('formatIntervalSecs', () => {
	it('renders minutes and hours compactly', () => {
		expect(formatIntervalSecs(180)).toBe('3m');
		expect(formatIntervalSecs(58 * 60)).toBe('58m');
		expect(formatIntervalSecs(3600)).toBe('1h');
		expect(formatIntervalSecs(90 * 60)).toBe('1h30');
	});
});

describe('looksKeepaliveTick', () => {
	it('recognises the server marker only', () => {
		expect(looksKeepaliveTick('[cctui keep-alive 2026-09-24T12:00:00Z] Cache keep-alive tick.')).toBe(true);
		expect(looksKeepaliveTick('  [cctui keep-alive x]')).toBe(true);
		expect(looksKeepaliveTick('keep-alive please')).toBe(false);
		expect(looksKeepaliveTick('')).toBe(false);
	});
});

describe('keepaliveActive', () => {
	it('stops at the tick budget or the projected end', () => {
		const base = { interval_secs: 600, max_ticks: DEFAULT_MAX_TICKS, ticks_sent: 0, until: iso(T0 + 3600e3) };
		expect(keepaliveActive(base, T0)).toBe(true);
		expect(keepaliveActive({ ...base, ticks_sent: 6 }, T0)).toBe(false);
		expect(keepaliveActive(base, T0 + 3600e3)).toBe(false);
		expect(keepaliveActive({ ...base, max_ticks: 0, ticks_sent: 99, until: null }, T0)).toBe(true);
	});
});

describe('warmUntilMs', () => {
	const claude = { adapter_id: 'claude-code', model: 'claude-opus-5' } as const;
	const ttl = 60 * 60e3;

	it('is null before any turn and last turn + TTL without keep-alive', () => {
		expect(warmUntilMs({ ...claude, last_activity_at: null, keepalive: null }, T0)).toBeNull();
		expect(warmUntilMs({ ...claude, last_activity_at: iso(T0), keepalive: null }, T0)).toBe(T0 + ttl);
		expect(isCacheWarm({ ...claude, last_activity_at: iso(T0), keepalive: null }, T0 + ttl + 1)).toBe(false);
	});

	it('extends to the schedule end plus TTL while keep-alive runs', () => {
		const until = T0 + 6 * 58 * 60e3;
		const s = {
			...claude,
			last_activity_at: iso(T0),
			keepalive: { interval_secs: 58 * 60, max_ticks: 6, ticks_sent: 1, until: iso(until) }
		};
		expect(warmUntilMs(s, T0 + 1000)).toBe(until + ttl);
		expect(isCacheWarm(s, T0 + 3 * ttl)).toBe(true);
	});

	it('is unbounded for an indefinite schedule and natural once exhausted', () => {
		const forever = {
			...claude,
			last_activity_at: iso(T0),
			keepalive: { interval_secs: 600, max_ticks: 0, ticks_sent: 40, until: null }
		};
		expect(warmUntilMs(forever, T0)).toBe(Number.POSITIVE_INFINITY);
		const done = {
			...claude,
			last_activity_at: iso(T0),
			keepalive: { interval_secs: 600, max_ticks: 6, ticks_sent: 6, until: iso(T0 + 3600e3) }
		};
		expect(warmUntilMs(done, T0)).toBe(T0 + ttl);
	});
});
