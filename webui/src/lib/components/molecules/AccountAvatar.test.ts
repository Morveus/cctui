import { afterEach, describe, expect, it } from 'vitest';
import { mount, unmount } from 'svelte';
import AccountAvatar from './AccountAvatar.svelte';

let comp: ReturnType<typeof mount> | null = null;
afterEach(() => {
	if (comp) unmount(comp);
	comp = null;
	document.body.innerHTML = '';
});

const open = (props: Record<string, unknown>) => {
	if (comp) unmount(comp);
	document.body.innerHTML = '';
	comp = mount(AccountAvatar, {
		target: document.body,
		props: { name: 'dorsk', id: 'acc-1', ...props }
	});
	const el = document.querySelector('[data-tsu="Avatar"]');
	if (!el) throw new Error('avatar not found');
	return el as HTMLElement;
};

describe('AccountAvatar', () => {
	it('shows the emoji when the account has one', () => {
		expect(open({ emoji: '🐙' }).textContent?.trim()).toBe('🐙');
	});

	it('falls back to the upper-cased first grapheme of the name', () => {
		expect(open({}).textContent?.trim()).toBe('D');
		expect(open({ emoji: '   ' }).textContent?.trim()).toBe('D');
	});

	it('names the account when it is the only label', () => {
		const el = open({});
		expect(el.getAttribute('role')).toBe('img');
		expect(el.getAttribute('aria-label')).toBe('dorsk');
		expect(el.getAttribute('title')).toBe('dorsk');
	});

	it('stays out of the accessibility tree when decorative', () => {
		const el = open({ decorative: true });
		expect(el.getAttribute('aria-hidden')).toBe('true');
		expect(el.getAttribute('role')).toBeNull();
		expect(el.getAttribute('aria-label')).toBeNull();
		expect(el.getAttribute('title')).toBeNull();
	});

	it('keeps the box square and sized in px', () => {
		const el = open({ size: 28 });
		expect(el.style.getPropertyValue('--avatar-size')).toBe('28px');
		expect(el.classList.contains('square')).toBe(true);
	});

	it('drops the fill behind an emoji', () => {
		expect(open({ emoji: '🐙' }).classList.contains('bare')).toBe(true);
		expect(open({}).classList.contains('bare')).toBe(false);
	});

	it('tints the same account identically wherever it is drawn', () => {
		const a = open({ size: 20 }).style.getPropertyValue('--avatar-hue');
		const b = open({ name: 'other name', size: 40 }).style.getPropertyValue('--avatar-hue');
		expect(a).toBe(b);
		expect(a).not.toBe('');
	});
});
