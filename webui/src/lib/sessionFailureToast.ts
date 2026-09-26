import type { SessionEndedEvent } from '$lib/ws.svelte';
import { FAILED_START_REASONS, endReasonLabel } from '$lib/sessionEnd';
import { toasts } from '$lib/toast.svelte';
import { m } from '$lib/paraglide/messages';

const TOAST_DETAIL_MAX = 240;

export function sessionHref(sessionId: string): string {
	return `/sessions/${encodeURIComponent(sessionId)}`;
}

/** Error toast for a session that failed to start or crashed; other ends are silent. */
export function sessionFailureToast(ev: SessionEndedEvent, navigate: (href: string) => void): boolean {
	if (ev.reason !== 'crashed' && !FAILED_START_REASONS.has(ev.reason)) return false;
	const raw = ev.detail?.trim() || m.spawn_error_unknown();
	const detail = raw.length > TOAST_DETAIL_MAX ? `${raw.slice(0, TOAST_DETAIL_MAX - 1)}…` : raw;
	toasts.error(m.sessions_end_failed_toast({ label: endReasonLabel(ev.reason), detail }), undefined, {
		label: m.sessions_end_open(),
		run: () => navigate(sessionHref(ev.session_id))
	});
	return true;
}
