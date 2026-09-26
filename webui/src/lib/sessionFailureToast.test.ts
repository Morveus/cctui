import { beforeEach, describe, expect, it, vi } from 'vitest';
import { toasts } from './toast.svelte';
import { sessionFailureToast, sessionHref } from './sessionFailureToast';

describe('sessionFailureToast', () => {
	beforeEach(() => {
		toasts.reset();
	});

	it('toasts a failed start with its detail and an action opening the session', () => {
		const navigate = vi.fn();
		const shown = sessionFailureToast(
			{ session_id: 's/1', reason: 'spawn_failed', detail: 'unknown model gpt-nope; available: a, b' },
			navigate
		);
		expect(shown).toBe(true);
		expect(toasts.items).toHaveLength(1);
		const t = toasts.items[0];
		expect(t.tone).toBe('error');
		expect(t.message).toContain('unknown model gpt-nope; available: a, b');
		expect(t.action?.label).toBe('Open session');
		t.action?.run();
		expect(navigate).toHaveBeenCalledWith(sessionHref('s/1'));
		expect(sessionHref('s/1')).toBe('/sessions/s%2F1');
	});

	it('keeps the toast short and stays silent for a normal end', () => {
		const navigate = vi.fn();
		sessionFailureToast(
			{ session_id: 's', reason: 'crashed', detail: 'x'.repeat(1000) },
			navigate
		);
		expect(toasts.items[0].message.length).toBeLessThan(300);
		expect(sessionFailureToast({ session_id: 's', reason: 'completed', detail: null }, navigate)).toBe(false);
		expect(toasts.items).toHaveLength(1);
	});
});
