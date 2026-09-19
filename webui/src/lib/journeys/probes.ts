import type { QueryClient } from '@tanstack/svelte-query';
import type { MachineRow } from '@bindings/MachineRow';
import { endpoints } from '$lib/queries/endpoints';
import { qk } from '$lib/queries/keys';
import { isLive } from './live';

export { isLive };

export type Probe = () => Promise<boolean | number>;
export type Probes = Record<string, Probe>;

/** The tours and the first-run checklist read app state through the same
 *  query cache the pages use, so a guide never disagrees with the screen. */
export function createProbes(qc: QueryClient): Probes {
	const me = () => qc.fetchQuery({ queryKey: ['me'], queryFn: endpoints.me, staleTime: 5 * 60_000 });
	const accounts = () => qc.fetchQuery({ queryKey: ['accounts'], queryFn: endpoints.accounts });
	const pools = () => qc.fetchQuery({ queryKey: ['account-pools'], queryFn: endpoints.accountPools });
	const sessions = () =>
		qc.fetchQuery({ queryKey: qk.sessions(false), queryFn: () => endpoints.sessions(false) });
	const stats = () => qc.fetchQuery({ queryKey: qk.sessionStats, queryFn: () => endpoints.sessionStats(Intl.DateTimeFormat().resolvedOptions().timeZone) });
	const machines = async (): Promise<MachineRow[]> => {
		const who = await me();
		if (who.role === 'admin') {
			return qc.fetchQuery({ queryKey: ['machines', 'all'], queryFn: endpoints.allMachines });
		}
		const userId = who.user_id;
		if (!userId) return [];
		return qc.fetchQuery({ queryKey: qk.machines(userId), queryFn: () => endpoints.machines(userId) });
	};
	return {
		'me.admin': async () => (await me()).role === 'admin',
		accounts: async () => (await accounts()).length > 0,
		pools: async () => (await pools()).length > 0,
		'machines.online': async () => (await machines()).some((m) => !m.revoked_at && m.liveness === 'online'),
		'machines.enrolled': async () =>
			(await machines()).filter((m) => m.kind !== 'ephemeral' && !m.revoked_at).length,
		sessions: async () => (await sessions()).sessions.length > 0,
		'sessions.drafts': async () => (await sessions()).sessions.filter((s) => s.status === 'draft').length,
		'sessions.queued': async () => (await sessions()).sessions.filter((s) => s.status === 'queued').length,
		'sessions.live': async () => (await stats()).live > 0
	};
}
