import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { toasts } from './toast.svelte';

describe('toasts (the kit store, wrapped to dedupe errors)', () => {
	beforeEach(() => {
		vi.useFakeTimers();
		toasts.reset();
	});
	afterEach(() => vi.useRealTimers());

	it('keeps an action toast on screen longer than a plain one', () => {
		toasts.ok('plain');
		toasts.ok('undoable', undefined, { label: 'Undo', run: () => {} });
		expect(toasts.items).toHaveLength(2);
		vi.advanceTimersByTime(4000);
		expect(toasts.items.map((t) => t.message)).toEqual(['undoable']);
		vi.advanceTimersByTime(3000);
		expect(toasts.items).toHaveLength(0);
	});

	it('runs the action once and dismisses the toast when it settles', async () => {
		const run = vi.fn();
		const id = toasts.ok('archived', undefined, { label: 'Undo', run });
		void toasts.act(id);
		void toasts.act(id);
		await vi.runAllTimersAsync();
		expect(run).toHaveBeenCalledTimes(1);
		expect(toasts.items).toHaveLength(0);
	});

	it('collapses an error repeated within the dedupe window', () => {
		toasts.error('file not found');
		toasts.error('file not found');
		expect(toasts.items).toHaveLength(1);

		toasts.error('something else');
		expect(toasts.items).toHaveLength(2);
	});

	it('lets the same error through again once the window has passed', () => {
		toasts.error('file not found');
		expect(toasts.items).toHaveLength(1);

		vi.advanceTimersByTime(2500);
		for (const t of [...toasts.items]) toasts.dismiss(t.id);
		toasts.error('file not found');
		expect(toasts.items).toHaveLength(1);
	});

	it('surfaces a failing action as an error toast', async () => {
		const id = toasts.ok('archived', undefined, {
			label: 'Undo',
			run: async () => {
				throw new Error('boom');
			}
		});
		void toasts.act(id);
		await vi.advanceTimersByTimeAsync(0);
		expect(toasts.items.map((t) => [t.tone, t.message])).toEqual([['error', 'boom']]);
	});
});
