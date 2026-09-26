export type PresetId = 'later' | 'tomorrow' | 'monday';

export interface SchedulePreset {
	id: PresetId;
	at: Date;
}

export const MAX_SCHEDULE_DAYS = 30;
const LATER_TODAY_CUTOFF_HOUR = 20;
const MORNING_HOUR = 9;
const SUNDAY = 0;
const MONDAY = 1;

function atHour(base: Date, dayOffset: number, hour: number): Date {
	return new Date(base.getFullYear(), base.getMonth(), base.getDate() + dayOffset, hour, 0, 0, 0);
}

/** Preset delivery times in the browser's local timezone. */
export function schedulePresets(now: Date): SchedulePreset[] {
	const out: SchedulePreset[] = [];
	if (now.getHours() < LATER_TODAY_CUTOFF_HOUR) {
		const later = atHour(now, 0, now.getHours() + 4);
		if (later.getDate() === now.getDate()) out.push({ id: 'later', at: later });
	}
	out.push({ id: 'tomorrow', at: atHour(now, 1, MORNING_HOUR) });
	const day = now.getDay();
	if (day !== SUNDAY && day !== MONDAY) {
		out.push({ id: 'monday', at: atHour(now, (8 - day) % 7, MORNING_HOUR) });
	}
	return out;
}

const pad = (n: number) => String(n).padStart(2, '0');

/** `YYYY-MM-DDTHH:mm` in local time, the `datetime-local` input format. */
export function toLocalInput(d: Date): string {
	return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** Earliest and latest values the custom picker accepts. */
export function customBounds(now: Date): { min: string; max: string } {
	const min = new Date(now.getTime() + 60_000);
	const max = new Date(now.getTime() + MAX_SCHEDULE_DAYS * 86_400_000);
	return { min: toLocalInput(min), max: toLocalInput(max) };
}

/** Parse a `datetime-local` value; `null` unless it lies in the schedulable window. */
export function parseCustom(value: string, now: Date): Date | null {
	const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})$/.exec(value.trim());
	if (!m) return null;
	const [y, mo, d, h, mi] = m.slice(1).map(Number);
	const at = new Date(y, mo - 1, d, h, mi, 0, 0);
	if (at.getTime() <= now.getTime()) return null;
	if (at.getTime() > now.getTime() + MAX_SCHEDULE_DAYS * 86_400_000) return null;
	return at;
}
