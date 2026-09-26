import { describe, expect, it } from 'vitest';
import { customBounds, parseCustom, schedulePresets, toLocalInput } from './scheduleTimes';

// 2026-09-24 is a Thursday.
const at = (d: number, h: number, mi = 0) => new Date(2026, 8, d, h, mi);
const ids = (now: Date) => schedulePresets(now).map((p) => p.id);
const preset = (now: Date, id: string) => schedulePresets(now).find((p) => p.id === id)?.at;

describe('schedulePresets', () => {
	it('offers later today at the next whole hour plus three', () => {
		expect(preset(at(24, 10, 15), 'later')).toEqual(at(24, 14));
		expect(preset(at(24, 10, 0), 'later')).toEqual(at(24, 14));
		expect(preset(at(24, 19, 59), 'later')).toEqual(at(24, 23));
	});

	it('hides later today from 20:00', () => {
		expect(ids(at(24, 20, 0))).not.toContain('later');
		expect(ids(at(24, 23, 30))).not.toContain('later');
	});

	it('always offers tomorrow at 9:00', () => {
		expect(preset(at(24, 10), 'tomorrow')).toEqual(at(25, 9));
		expect(preset(at(30, 23, 59), 'tomorrow')).toEqual(new Date(2026, 9, 1, 9));
	});

	it('offers next Monday at 9:00 except on Sunday and Monday', () => {
		expect(preset(at(24, 10), 'monday')).toEqual(at(28, 9));
		expect(preset(at(22, 10), 'monday')).toEqual(at(28, 9));
		expect(preset(at(26, 10), 'monday')).toEqual(at(28, 9));
		expect(ids(at(27, 10))).not.toContain('monday');
		expect(ids(at(28, 10))).not.toContain('monday');
	});

	it('lists presets in order', () => {
		expect(ids(at(24, 10))).toEqual(['later', 'tomorrow', 'monday']);
	});
});

describe('custom time', () => {
	const now = at(24, 10, 15);

	it('formats datetime-local values in local time', () => {
		expect(toLocalInput(at(5, 9, 7))).toBe('2026-09-05T09:07');
	});

	it('bounds the picker to the next minute and 30 days out', () => {
		expect(customBounds(now)).toEqual({ min: '2026-09-24T10:16', max: '2026-10-24T10:15' });
	});

	it('accepts only future times within 30 days', () => {
		expect(parseCustom('2026-09-24T11:00', now)).toEqual(at(24, 11));
		expect(parseCustom('2026-09-24T10:15', now)).toBeNull();
		expect(parseCustom('2026-09-01T10:00', now)).toBeNull();
		expect(parseCustom('2026-10-25T10:00', now)).toBeNull();
		expect(parseCustom('', now)).toBeNull();
		expect(parseCustom('garbage', now)).toBeNull();
	});
});
