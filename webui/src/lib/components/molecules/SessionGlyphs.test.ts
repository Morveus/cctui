import { afterEach, describe, expect, it, vi } from 'vitest';
import { flushSync, mount, unmount } from 'svelte';
import type { SessionListItem } from '@bindings/SessionListItem';
import SessionGlyphs from './SessionGlyphs.svelte';

vi.mock('$lib/queries', () => ({ useAccounts: () => ({ data: [] }) }));

let comp: ReturnType<typeof mount> | null = null;
afterEach(() => {
	if (comp) unmount(comp);
	comp = null;
	document.body.innerHTML = '';
});

const session = (over: Partial<SessionListItem> = {}) =>
	({
		id: 's1',
		status: 'active',
		liveness: 'active',
		pinned: false,
		machine_id: 'm-123456789',
		machine_name: 'devbox1',
		machine_hue: null,
		account_name: 'personal',
		...over
	}) as unknown as SessionListItem;

function render(props: Record<string, unknown>) {
	comp = mount(SessionGlyphs, {
		target: document.body,
		props: { session: session(), livenessClass: 'live', ...props } as never
	});
	flushSync();
	return document.querySelector('[data-tsu="GlyphStack"]') as HTMLElement;
}

describe('SessionGlyphs', () => {
	it('pins on click without letting the click reach the row', () => {
		const onTogglePin = vi.fn();
		render({ onTogglePin });
		const star = document.querySelector('[aria-pressed]') as HTMLElement;
		expect(star.textContent).toBe('☆');
		const ev = new MouseEvent('click', { bubbles: true, cancelable: true });
		star.dispatchEvent(ev);
		expect(onTogglePin).toHaveBeenCalledOnce();
		expect(ev.cancelBubble).toBe(true);
	});

	it('shows no star without a pin handler', () => {
		render({});
		expect(document.querySelector('[aria-pressed]')).toBeNull();
	});

	it('folds into the stacked form with the machine as a tile only when asked', () => {
		const stacked = render({ stack: 'always' });
		expect(stacked.dataset.stacked).toBe('true');
		if (comp) unmount(comp);
		comp = null;
		const inline = render({ stack: 'never' });
		expect(inline.dataset.stacked).toBeUndefined();
		expect(document.querySelector('.mach-tile')).toBeNull();
	});

	it('renders the machine as a numbered initial tile for the stacked form', () => {
		render({ stack: 'always' });
		const tile = document.querySelector('.mach-tile') as HTMLElement;
		expect(tile.textContent).toBe('D1');
		expect(tile.getAttribute('title')).toBe('devbox1');
		expect(tile.getAttribute('style')).toContain('--mh:');
	});

	it('drops the machine when told to', () => {
		render({ showMachine: false, stack: 'always' });
		expect(document.querySelector('.mach-tile')).toBeNull();
	});

	function tapHit(): MouseEvent {
		const ev = new MouseEvent('click', { bubbles: true, cancelable: true });
		(document.querySelector('.glyph-hit') as HTMLElement).dispatchEvent(ev);
		return ev;
	}

	it('keeps taps on the folded stack from reaching the row', () => {
		render({ stack: 'always' });
		expect(tapHit().cancelBubble).toBe(true);
	});

	it('lets taps through while the stack is inline', () => {
		render({ stack: 'never' });
		expect(tapHit().cancelBubble).toBe(false);
	});
});
