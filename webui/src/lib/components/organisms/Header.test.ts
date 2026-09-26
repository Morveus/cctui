import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { mount, unmount, flushSync } from 'svelte';
import type { SessionListItem } from '@bindings/SessionListItem';

const versionData = {
	version: '0.11.0',
	commit_url: 'https://example.test/commit',
	instance_name: null as string | null,
	latest_version: null as string | null,
	latest_url: null as string | null,
	repo_url: 'https://example.test',
	self_update_ready: false,
	self_update_hook: null
};
const sessionsData = { sessions: [] as SessionListItem[] };

vi.mock('$lib/queries', () => ({
	qk: { sessions: (a: boolean) => ['sessions', a] },
	useMe: () => ({ data: { user_name: 'dorsk', role: 'admin' } }),
	useVersion: () => ({ data: versionData }),
	useSessions: () => ({ data: sessionsData }),
	useAccounts: () => ({ data: [] }),
	useAllAccountsUsage: () => ({ data: [] }),
	useRedirectChips: () => ({ data: [] }),
	useMachineResources: () => ({ data: [] })
}));
vi.mock('@tanstack/svelte-query', () => ({
	useQueryClient: () => ({ invalidateQueries: vi.fn(), setQueryData: vi.fn() })
}));
const { goto } = vi.hoisted(() => ({ goto: vi.fn() }));
vi.mock('$app/navigation', () => ({ goto }));
vi.mock('$lib/ws.svelte', () => ({
	ws: {
		status: 'open',
		changeTick: 0,
		onListPatch: () => () => {},
		onMachineResources: () => () => {},
		onAccountUsage: () => () => {}
	}
}));
vi.mock('$lib/toast.svelte', () => ({
	toasts: { ok: vi.fn(), info: vi.fn(), error: vi.fn() }
}));

vi.stubGlobal('__CLIENT_VERSION__', '0.11.0');

import { notify } from '$lib/notify.svelte';
import Header from './Header.svelte';

let comp: ReturnType<typeof mount> | null = null;
function cleanup() {
	if (comp) unmount(comp);
	comp = null;
	document.body.innerHTML = '';
}
afterEach(cleanup);

beforeEach(() => {
	goto.mockClear();
	versionData.latest_version = null;
	sessionsData.sessions = [];
	notify.enabled = false;
});

const render = () => {
	comp = mount(Header, { target: document.body });
	flushSync();
};

async function openUserMenu() {
	const panel = [...document.querySelectorAll('[popover]')].at(-1) as HTMLElement;
	panel.dispatchEvent(Object.assign(new Event('toggle'), { newState: 'open' }));
	flushSync();
	await Promise.resolve();
	flushSync();
}

const menuRows = () =>
	[...document.querySelectorAll('[role="menuitem"], [role="menuitemcheckbox"]')] as HTMLElement[];
const rowByText = (text: string) =>
	menuRows().find((r) => r.textContent?.includes(text)) ?? null;

describe('the system bar no longer carries the ? or the bell (CCT-1012)', () => {
	it('has no standalone guides button in the tail', () => {
		render();
		const tail = document.querySelector('.tail') as HTMLElement;
		expect(tail.textContent).not.toContain('?');
		expect(tail.querySelector('a[href="/settings/guides"]')).toBeNull();
	});

	it('keeps Getting started reachable from the user menu', async () => {
		render();
		await openUserMenu();
		const row = rowByText('Getting started');
		expect(row).not.toBeNull();
		row?.click();
		expect(goto).toHaveBeenCalledWith('/settings/guides');
	});

	it('offers the notification toggle in the menu and reflects its pressed state', async () => {
		render();
		await openUserMenu();
		const row = rowByText('Notify me when a session needs input');
		expect(row).not.toBeNull();
		expect(row?.getAttribute('role')).toBe('menuitemcheckbox');
		expect(row?.getAttribute('aria-checked')).toBe('false');
	});

	it('gives every menu row an icon, Settings included', async () => {
		render();
		await openUserMenu();
		const rows = menuRows();
		expect(rows.length).toBeGreaterThan(0);
		for (const r of rows) expect(r.querySelector('svg')).not.toBeNull();
		expect(rowByText('Settings')?.querySelector('svg')).not.toBeNull();
	});

	it('routes Settings through the canonical settings href', async () => {
		render();
		await openUserMenu();
		rowByText('Settings')?.click();
		expect(goto).toHaveBeenCalledWith('/settings/appearance');
	});
});

describe('notifications', () => {
	it('keep no indicator on the pill; the toggle lives in the user menu', async () => {
		notify.enabled = false;
		render();
		expect(document.querySelector('.pill .bell')).toBeNull();
		await openUserMenu();
		expect(rowByText('Notify me when a session needs input')).toBeTruthy();
	});
});

describe('the version block sheds its third line (CCT-1013)', () => {
	it('renders ui and srv only, with the update state left to the avatar dot', () => {
		versionData.latest_version = '0.12.0';
		render();
		const vers = document.querySelector('.vers') as HTMLElement;
		expect(vers.textContent).toContain('ui v');
		expect(vers.textContent).toContain('srv v0.11.0');
		expect(vers.textContent).not.toContain('v0.12.0');
		expect(document.querySelector('.pill [data-tsu="Avatar"] [data-tsu="Dot"]')).not.toBeNull();
	});

	it('drops the alert dot when there is no update', () => {
		render();
		expect(document.querySelector('.pill [data-tsu="Avatar"] [data-tsu="Dot"]')).toBeNull();
	});
});

describe('the user pill avatar', () => {
	it('carries the user initial and stays out of the accessibility tree', () => {
		render();
		const av = document.querySelector('.pill [data-tsu="Avatar"]') as HTMLElement;
		expect(av.textContent?.trim()).toBe('D');
		expect(av.getAttribute('aria-hidden')).toBe('true');
		expect(av.getAttribute('role')).toBeNull();
	});
});
