import type { KeepaliveState } from '@bindings/KeepaliveState';
import type { SessionListItem } from '@bindings/SessionListItem';
import { cacheTtlMs } from './cacheTtl';

const MIN = 60;
const TTL_MARGIN_SECS = 2 * MIN;
export const DEFAULT_MAX_TICKS = 6;
export const MIN_INTERVAL_SECS = MIN;

/** Must match the server's `keepalive::TICK_MARKER`. */
export const TICK_MARKER = '[cctui keep-alive';

export function looksKeepaliveTick(text: string): boolean {
	return text.trimStart().startsWith(TICK_MARKER);
}

export function defaultIntervalSecs(adapterId: string | null, model: string | null): number {
	return Math.max(MIN_INTERVAL_SECS, cacheTtlMs(adapterId, model) / 1000 - TTL_MARGIN_SECS);
}

export function intervalOptionsSecs(adapterId: string | null, model: string | null): number[] {
	const ttlSecs = cacheTtlMs(adapterId, model) / 1000;
	const fixed = [3 * MIN, 5 * MIN, 10 * MIN, 15 * MIN, 28 * MIN, 45 * MIN, 58 * MIN];
	const all = new Set([defaultIntervalSecs(adapterId, model), ...fixed.filter((s) => s < ttlSecs)]);
	return [...all].sort((a, b) => a - b);
}

export function formatIntervalSecs(secs: number): string {
	if (secs % 3600 === 0) return `${secs / 3600}h`;
	if (secs >= 3600) return `${Math.floor(secs / 3600)}h${String(Math.round((secs % 3600) / 60)).padStart(2, '0')}`;
	return `${Math.round(secs / 60)}m`;
}

export const MAX_TICKS_OPTIONS = [1, 3, 6, 12, 24, 0] as const;

export type WarmSession = Pick<
	SessionListItem,
	'adapter_id' | 'model' | 'last_activity_at' | 'keepalive'
>;

/** `Infinity` while an indefinite schedule runs; `null` before any turn. */
export function warmUntilMs(session: WarmSession, nowMs: number): number | null {
	const last = session.last_activity_at ? new Date(session.last_activity_at).getTime() : null;
	const ttl = cacheTtlMs(session.adapter_id, session.model ?? null);
	const natural = last === null ? null : last + ttl;
	const ka = session.keepalive ?? null;
	if (ka && keepaliveActive(ka, nowMs)) {
		if (!ka.until) return Number.POSITIVE_INFINITY;
		return Math.max(natural ?? 0, new Date(ka.until).getTime() + ttl);
	}
	return natural;
}

export function keepaliveActive(ka: KeepaliveState, nowMs: number): boolean {
	if (ka.max_ticks > 0 && ka.ticks_sent >= ka.max_ticks) return false;
	if (ka.until && new Date(ka.until).getTime() <= nowMs) return false;
	return true;
}

export function isCacheWarm(session: WarmSession, nowMs: number): boolean {
	const until = warmUntilMs(session, nowMs);
	return until !== null && until > nowMs;
}
