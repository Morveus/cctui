import { beforeEach, describe, expect, it, vi } from 'vitest';
import { QueryClient } from '@tanstack/svelte-query';
import { createProbes, isLive } from './probes';

const api = vi.hoisted(() => ({
	me: vi.fn(),
	accounts: vi.fn(),
	accountPools: vi.fn(),
	allMachines: vi.fn(),
	machines: vi.fn(),
	sessions: vi.fn(),
	sessionStats: vi.fn()
}));
vi.mock('$lib/queries/endpoints', () => ({ endpoints: api }));

const machine = (over: Record<string, unknown> = {}) => ({
	id: 'm1',
	kind: 'persistent',
	liveness: 'online',
	revoked_at: null,
	...over
});
const session = (over: Record<string, unknown> = {}) => ({
	id: 's1',
	status: 'active',
	liveness: 'active',
	...over
});

let probes: ReturnType<typeof createProbes>;

beforeEach(() => {
	for (const fn of Object.values(api)) fn.mockReset();
	api.me.mockResolvedValue({ role: 'admin', user_id: 'u1', user_name: 'admin' });
	api.accounts.mockResolvedValue([]);
	api.accountPools.mockResolvedValue([]);
	api.allMachines.mockResolvedValue([]);
	api.machines.mockResolvedValue([]);
	api.sessions.mockResolvedValue({ sessions: [] });
	api.sessionStats.mockResolvedValue({ total: 0, live: 0, needs_input: 0, archived: 0 });
	probes = createProbes(new QueryClient({ defaultOptions: { queries: { retry: false } } }));
});

describe('probes', () => {
	it('are all false on an empty instance except the admin role', async () => {
		expect(await probes['me.admin']()).toBe(true);
		expect(await probes.accounts()).toBe(false);
		expect(await probes.pools()).toBe(false);
		expect(await probes['machines.online']()).toBe(false);
		expect(await probes['machines.enrolled']()).toBe(0);
		expect(await probes.sessions()).toBe(false);
		expect(await probes['sessions.drafts']()).toBe(0);
		expect(await probes['sessions.queued']()).toBe(0);
		expect(await probes['sessions.live']()).toBe(false);
	});

	it('reads the fleet-wide machine list as admin', async () => {
		api.allMachines.mockResolvedValue([machine(), machine({ id: 'm2', liveness: 'offline' })]);
		expect(await probes['machines.online']()).toBe(true);
		expect(await probes['machines.enrolled']()).toBe(2);
		expect(api.machines).not.toHaveBeenCalled();
	});

	it('reads only the caller machines otherwise', async () => {
		api.me.mockResolvedValue({ role: 'user', user_id: 'u2', user_name: 'bob' });
		api.machines.mockResolvedValue([
			machine({ liveness: 'stale' }),
			machine({ id: 'e', kind: 'ephemeral', liveness: 'offline' }),
			machine({ id: 'r', revoked_at: 'x' })
		]);
		expect(await probes['machines.online']()).toBe(false);
		expect(await probes['machines.enrolled']()).toBe(1);
		expect(api.machines).toHaveBeenCalledWith('u2');
		expect(api.allMachines).not.toHaveBeenCalled();
	});

	it('counts drafts as sessions', async () => {
		api.sessions.mockResolvedValue({ sessions: [session({ status: 'draft' })] });
		expect(await probes.sessions()).toBe(true);
		expect(await probes['sessions.drafts']()).toBe(1);
		expect(await probes['sessions.live']()).toBe(false);
	});

	it('counts queued spawns apart from drafts', async () => {
		api.sessions.mockResolvedValue({
			sessions: [session({ status: 'queued' }), session({ id: 'd', status: 'draft' })]
		});
		expect(await probes['sessions.queued']()).toBe(1);
		expect(await probes['sessions.drafts']()).toBe(1);
	});

	it('reads live from the stats endpoint, accounts and pools from their lists', async () => {
		api.sessionStats.mockResolvedValue({ total: 1, live: 1, needs_input: 0, archived: 0 });
		api.accounts.mockResolvedValue([{ id: 'a', name: 'main' }]);
		api.accountPools.mockResolvedValue([{ id: 'p', name: 'prod', members: [] }]);
		expect(await probes['sessions.live']()).toBe(true);
		expect(await probes.accounts()).toBe(true);
		expect(await probes.pools()).toBe(true);
	});

	it('re-reads state on every call instead of caching a flag', async () => {
		const qc = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 0 } } });
		probes = createProbes(qc);
		expect(await probes.accounts()).toBe(false);
		api.accounts.mockResolvedValue([{ id: 'a', name: 'main' }]);
		await qc.invalidateQueries({ queryKey: ['accounts'] });
		expect(await probes.accounts()).toBe(true);
	});
});

describe('isLive', () => {
	it('accepts registry sessions that still answer', () => {
		expect(isLive(session() as never)).toBe(true);
		expect(isLive(session({ status: 'new', liveness: 'stale' }) as never)).toBe(true);
		expect(isLive(session({ liveness: 'dead' }) as never)).toBe(false);
		expect(isLive(session({ status: 'draft' }) as never)).toBe(false);
		expect(isLive(session({ status: 'archived' }) as never)).toBe(false);
	});
});
