import { describe, expect, it } from 'vitest';
import type { SessionListItem } from '@bindings/SessionListItem';
import type { JsonValue } from '@bindings/serde_json/JsonValue';
import {
	accountTrafficWarning,
	branchOf,
	bucketInSection,
	colorHueOf,
	dimGroupsOf,
	DIM_NONE_KEY,
	draftEditPrefill,
	draftPayload,
	draftPreview,
	draftPromptPreview,
	draftSavedAt,
	editDraftNeedsConfirm,
	fmtWhen,
	formatAgo,
	groupChildren,
	groupRows,
	inEnabledSections,
	isDimension,
	isSection,
	matchesLabelFilter,
	matchesUnreadFilter,
	parseLabelFilter,
	parseSections,
	parseHiddenSections,
	serializeHiddenSections,
	toggleHiddenSection,
	pickFreshSession,
	queueOrder,
	rangeIds,
	scriptPrefill,
	sectionsOf,
	spawnRequestFromSlot,
	sessionDebugRows,
	sessionHrefFor,
	hrefWithoutDiagnose,
	sessionIdFromLocation,
	sortSessions,
	nextSort,
	idsForSection,
	toolActivity,
	TOOL_ASLEEP_AFTER_MS,
	type Section
} from './sessions.logic';
import type { Label } from '@bindings/Label';

function session(over: Partial<SessionListItem>): SessionListItem {
	return { id: 'sess-abc', labels: [], working_dir: '', unread_count: 0, ...over } as SessionListItem;
}

const label = (id: string, name: string, color = ''): Label => ({ id, name, color });

describe('branchOf', () => {
	it('reads metadata.git_branch, null when absent or not a string', () => {
		expect(branchOf(session({ metadata: { git_branch: 'wave/1-webui' } }))).toBe('wave/1-webui');
		expect(branchOf(session({ metadata: { git_branch: 7 } }))).toBeNull();
		expect(branchOf(session({ metadata: {} }))).toBeNull();
		expect(branchOf(session({ metadata: null }))).toBeNull();
	});
});

describe('accountTrafficWarning', () => {
	it('warns only for an account-bound session with no observed gateway traffic', () => {
		// Bound + never observed → warn (may be riding ambient creds).
		expect(accountTrafficWarning(session({ account_name: 'work', account_traffic_observed: false }))).toBe(
			true
		);
		// Bound + observed → no warning.
		expect(accountTrafficWarning(session({ account_name: 'work', account_traffic_observed: true }))).toBe(
			false
		);
		// Unbound (no account) → nothing to warn about, whatever the flag.
		expect(accountTrafficWarning(session({ account_name: null, account_traffic_observed: false }))).toBe(
			false
		);
		// Field absent (older server) → never a false warning.
		expect(accountTrafficWarning(session({ account_name: 'work' }))).toBe(false);
	});
});

describe('unread section', () => {
	it('recognises "unread" as a section', () => {
		expect(isSection('unread')).toBe(true);
	});

	it('round-trips through parseSections', () => {
		expect(parseSections('live,unread')).toEqual(new Set<Section>(['live', 'unread']));
	});

	it('is off by default (never in the fallback set)', () => {
		expect(parseSections(null).has('unread')).toBe(false);
		expect(parseSections('').has('unread')).toBe(false);
	});

	describe('hidden sections', () => {
		it('round-trips a persisted set, dropping empties', () => {
			expect(parseHiddenSections('live,drafts')).toEqual(new Set(['live', 'drafts']));
			expect(parseHiddenSections(null)).toEqual(new Set());
			expect(parseHiddenSections('')).toEqual(new Set());
			expect(parseHiddenSections(',live,')).toEqual(new Set(['live']));
			expect(serializeHiddenSections(new Set(['drafts', 'live']))).toBe('drafts,live');
			expect(serializeHiddenSections(new Set())).toBe('');
			expect(parseHiddenSections(serializeHiddenSections(new Set(['archived', 'dim:x'])))).toEqual(
				new Set(['archived', 'dim:x'])
			);
		});

		it('toggles one section without mutating the previous set', () => {
			const before = new Set(['live']);
			const on = toggleHiddenSection(before, 'drafts');
			expect(on).toEqual(new Set(['live', 'drafts']));
			expect(before).toEqual(new Set(['live']));
			expect(toggleHiddenSection(on, 'live')).toEqual(new Set(['drafts']));
		});
	});

	describe('matchesUnreadFilter', () => {
		const on = new Set<Section>(['live', 'unread']);
		const off = new Set<Section>(['live']);

		it('passes every row when the filter is off', () => {
			expect(matchesUnreadFilter(session({ unread_count: 0 }), off)).toBe(true);
			expect(matchesUnreadFilter(session({ unread_count: 3 }), off)).toBe(true);
		});

		it('keeps only rows with unread messages when on', () => {
			expect(matchesUnreadFilter(session({ unread_count: 0 }), on)).toBe(false);
			expect(matchesUnreadFilter(session({ unread_count: 1 }), on)).toBe(true);
		});

		it('treats a missing count as zero unread', () => {
			expect(matchesUnreadFilter(session({ unread_count: undefined }), on)).toBe(false);
		});
	});
});

describe('tool activity — asleep vs. grinding', () => {
	const NOW = 1_000_000_000_000;
	const working = (over: Partial<SessionListItem>) =>
		session({ bucket: 'working', status: 'active', ...over });

	it('is hidden with no tool activity and no headline', () => {
		expect(toolActivity(working({}), NOW).show).toBe(false);
	});

	it('shows the headline even before any tool call', () => {
		const a = toolActivity(working({ activity_detail: 'compiling…' }), NOW);
		expect(a.show).toBe(true);
		expect(a.detail).toBe('compiling…');
		expect(a.ageMs).toBeNull();
		expect(a.asleep).toBe(false);
	});

	it('reads as grinding when the last tool call is fresh', () => {
		const a = toolActivity(
			working({ last_tool_at: new Date(NOW - 10_000).toISOString(), tool_use_count: 42 }),
			NOW
		);
		expect(a.show).toBe(true);
		expect(a.count).toBe(42);
		expect(a.ageMs).toBe(10_000);
		expect(a.asleep).toBe(false);
	});

	it('reads as asleep once tool calls stop past the threshold', () => {
		const a = toolActivity(
			working({ last_tool_at: new Date(NOW - TOOL_ASLEEP_AFTER_MS - 1_000).toISOString() }),
			NOW
		);
		expect(a.asleep).toBe(true);
		expect(a.show).toBe(true);
	});

	it('derives done/total and the in_progress activeForm from the task list', () => {
		const a = toolActivity(
			working({
				todos: [
					{ content: 'parse it', status: 'completed', active_form: 'Parsing it' },
					{ content: 'wire it', status: 'completed', active_form: 'Wiring it' },
					{ content: 'ship it', status: 'in_progress', active_form: 'Wiring the parser' },
					{ content: 'test it', status: 'pending', active_form: 'Testing it' }
				]
			}),
			NOW
		);
		expect(a.todoDone).toBe(2);
		expect(a.todoTotal).toBe(4);
		expect(a.todoActive).toBe('Wiring the parser');
		expect(a.show).toBe(true);
	});

	it('falls back to the entry content when the harness sent no activeForm', () => {
		const a = toolActivity(
			working({ todos: [{ content: 'change the code', status: 'in_progress' }] }),
			NOW
		);
		expect(a.todoActive).toBe('change the code');
		expect(a.todoDone).toBe(0);
		expect(a.todoTotal).toBe(1);
	});

	it('reports no in_progress step when every task is pending or done', () => {
		const a = toolActivity(
			working({
				todos: [
					{ content: 'a', status: 'completed' },
					{ content: 'b', status: 'pending' }
				]
			}),
			NOW
		);
		expect(a.todoActive).toBeNull();
		expect(a.todoDone).toBe(1);
		expect(a.todoTotal).toBe(2);
	});

	it('renders no badge at all for a session that never wrote a task list', () => {
		const a = toolActivity(working({ todos: [] }), NOW);
		expect(a.todoTotal).toBe(0);
		expect(a.todoActive).toBeNull();
		expect(a.show).toBe(false);
	});

	it('surfaces the task list even outside the working bucket', () => {
		const a = toolActivity(
			session({
				bucket: 'done',
				status: 'active',
				todos: [{ content: 'a', status: 'completed' }]
			}),
			NOW
		);
		expect(a.show).toBe(true);
		expect(a.todoDone).toBe(1);
		expect(a.count).toBe(0);
	});

	it('is never asleep for a non-working bucket', () => {
		const a = toolActivity(
			session({
				bucket: 'done',
				status: 'active',
				last_tool_at: new Date(NOW - TOOL_ASLEEP_AFTER_MS - 1_000).toISOString()
			}),
			NOW
		);
		expect(a.asleep).toBe(false);
		expect(a.show).toBe(false);
	});
});

describe('debug tooltip rows', () => {
	const NOW = 1_000_000_000_000;

	it('renders nulls as "—", never "null"', () => {
		const rows = sessionDebugRows(session({ liveness: 'dead' }), NOW);
		const values = Object.fromEntries(rows.map((r) => [r.label, r.value]));
		expect(values.account).toBe('—');
		expect(values.created).toBe('—');
		expect(values.machine).toBe('—');
		expect(values.keepalive).toBe('—');
		expect(JSON.stringify(rows)).not.toContain('null');
	});

	it('shows account, machine (+ non-persistent kind) and credential state', () => {
		const rows = sessionDebugRows(
			session({
				account_name: 'work',
				machine_name: 'runner-1',
				machine_kind: 'dispatch',
				has_token_credentials: true
			}),
			NOW
		);
		const values = Object.fromEntries(rows.map((r) => [r.label, r.value]));
		expect(values.account).toBe('work');
		expect(values.machine).toBe('runner-1 (dispatch)');
		expect(values.creds).toBe('live token binding');
	});

	it('omits the kind suffix for persistent machines', () => {
		const rows = sessionDebugRows(
			session({ machine_name: 'laptop', machine_kind: 'persistent' }),
			NOW
		);
		expect(rows.find((r) => r.label === 'machine')?.value).toBe('laptop');
	});

	it('flags an account with no live token binding', () => {
		const rows = sessionDebugRows(
			session({ account_name: 'work', has_token_credentials: false }),
			NOW
		);
		expect(rows.find((r) => r.label === 'creds')?.value).toBe(
			'account only (token revoked/absent)'
		);
	});

	it('reports hibernated as the status word', () => {
		const rows = sessionDebugRows(session({ hibernated: true, liveness: 'dead' }), NOW);
		expect(rows.find((r) => r.label === 'status')?.value).toBe('hibernated');
	});
});

describe('dimension color / group', () => {
	it('recognises the dimension enum values', () => {
		expect(isDimension('none')).toBe(true);
		expect(isDimension('label')).toBe(true);
		expect(isDimension('working_dir')).toBe(true);
		expect(isDimension('machine')).toBe(true);
		expect(isDimension('nope')).toBe(false);
	});

	describe('dimGroupsOf', () => {
		it('splits a session into one membership per label', () => {
			const s = session({ labels: [label('l1', 'infra'), label('l2', 'urgent')] });
			const gs = dimGroupsOf(s, 'label');
			expect(gs.map((g) => g.label)).toEqual(['infra', 'urgent']);
			expect(gs.map((g) => g.key)).toEqual(['label:l1', 'label:l2']);
		});

		it('routes an unlabelled session to the "—" bucket', () => {
			const gs = dimGroupsOf(session({ labels: [] }), 'label');
			expect(gs).toEqual([{ key: DIM_NONE_KEY, label: '—', hue: null }]);
		});

		it('groups working_dir by its basename', () => {
			const gs = dimGroupsOf(session({ working_dir: '/home/dev/cctui' }), 'working_dir');
			expect(gs).toHaveLength(1);
			expect(gs[0].label).toBe('cctui');
			expect(gs[0].key).toBe('dir:/home/dev/cctui');
		});

		it('sends a session with no working dir to "—"', () => {
			expect(dimGroupsOf(session({ working_dir: '' }), 'working_dir')[0].key).toBe(DIM_NONE_KEY);
		});

		it('prefers an operator-set machine hue over the name hash', () => {
			const gs = dimGroupsOf(session({ machine_name: 'runner', machine_hue: 200 }), 'machine');
			expect(gs[0].label).toBe('runner');
			expect(gs[0].hue).toBe(200);
		});

		it('sends a machineless session to "—"', () => {
			expect(dimGroupsOf(session({ machine_name: null }), 'machine')[0].key).toBe(DIM_NONE_KEY);
		});
	});

	describe('colorHueOf', () => {
		it('is null for the none dimension', () => {
			expect(colorHueOf(session({ working_dir: '/a/b' }), 'none')).toBeNull();
		});

		it('is null when the session is missing the dimension', () => {
			expect(colorHueOf(session({ labels: [] }), 'label')).toBeNull();
		});

		it('is deterministic and stable for the same working dir', () => {
			const a = colorHueOf(session({ working_dir: '/home/dev/cctui' }), 'working_dir');
			const b = colorHueOf(session({ working_dir: '/home/dev/cctui' }), 'working_dir');
			expect(a).toBe(b);
			expect(a).not.toBeNull();
			expect(a as number).toBeGreaterThanOrEqual(0);
			expect(a as number).toBeLessThan(360);
		});

		it('gives different working dirs distinct hues', () => {
			expect(colorHueOf(session({ working_dir: '/x/api' }), 'working_dir')).not.toBe(
				colorHueOf(session({ working_dir: '/x/web' }), 'working_dir')
			);
		});

		it('takes the primary (first) label hue', () => {
			const s = session({ labels: [label('l1', 'a', '120'), label('l2', 'b', '240')] });
			expect(colorHueOf(s, 'label')).toBe(120);
		});
	});

	describe('groupRows', () => {
		it('wraps every row in one unlabelled section for none', () => {
			const rows = [session({ id: 'a' }), session({ id: 'b' })];
			const gs = groupRows(rows, 'none');
			expect(gs).toHaveLength(1);
			expect(gs[0].sessions).toHaveLength(2);
		});

		it('partitions by working dir and sorts groups by name', () => {
			const rows = [
				session({ id: 'a', working_dir: '/x/web' }),
				session({ id: 'b', working_dir: '/x/api' }),
				session({ id: 'c', working_dir: '/x/api' })
			];
			const gs = groupRows(rows, 'working_dir');
			expect(gs.map((g) => g.label)).toEqual(['api', 'web']);
			expect(gs[0].sessions.map((s) => s.id)).toEqual(['b', 'c']);
		});

		it('puts the "—" bucket last regardless of name', () => {
			const rows = [
				session({ id: 'a', machine_name: null }),
				session({ id: 'b', machine_name: 'zeta' })
			];
			const gs = groupRows(rows, 'machine');
			expect(gs.map((g) => g.label)).toEqual(['zeta', '—']);
			expect(gs[gs.length - 1].key).toBe(DIM_NONE_KEY);
		});

		it('lists a multi-labelled session under each of its labels', () => {
			const rows = [session({ id: 'a', labels: [label('l1', 'infra'), label('l2', 'urgent')] })];
			const gs = groupRows(rows, 'label');
			expect(gs.map((g) => g.label)).toEqual(['infra', 'urgent']);
			expect(gs.every((g) => g.sessions[0].id === 'a')).toBe(true);
		});

		it('preserves incoming row order within a group', () => {
			const rows = [
				session({ id: 'first', working_dir: '/x/api' }),
				session({ id: 'second', working_dir: '/x/api' })
			];
			expect(groupRows(rows, 'working_dir')[0].sessions.map((s) => s.id)).toEqual([
				'first',
				'second'
			]);
		});
	});
});

describe('fmtWhen', () => {
	const NOW = 1_000_000_000_000;
	it('returns "—" for missing or unparseable timestamps', () => {
		expect(fmtWhen(null, NOW)).toBe('—');
		expect(fmtWhen(undefined, NOW)).toBe('—');
		expect(fmtWhen('not-a-date', NOW)).toBe('—');
	});
	it('pairs a relative age with the raw ISO', () => {
		const iso = new Date(NOW - 90_000).toISOString();
		expect(fmtWhen(iso, NOW)).toBe(`1m ago · ${iso}`);
	});
});

describe('formatAgo', () => {
	it('formats seconds, minutes, hours', () => {
		expect(formatAgo(5_000)).toBe('5s');
		expect(formatAgo(90_000)).toBe('1m');
		expect(formatAgo(3 * 3600_000)).toBe('3h');
		expect(formatAgo(-100)).toBe('0s');
	});
});

describe('sessionIdFromLocation', () => {
	const search = (qs = '') => new URL(`http://x${qs}`).searchParams;

	it('reads the id from the shallow-routed /sessions/<id> path', () => {
		expect(sessionIdFromLocation('/sessions/abc-123', search())).toBe('abc-123');
	});

	it('decodes a percent-encoded path id', () => {
		expect(sessionIdFromLocation('/sessions/a%2Fb', search())).toBe('a/b');
	});

	it('falls back to the ?session= query param when the path is bare', () => {
		expect(sessionIdFromLocation('/sessions', search('?session=q-9'))).toBe('q-9');
	});

	it('prefers the path id over the query param', () => {
		expect(sessionIdFromLocation('/sessions/path-id', search('?session=query-id'))).toBe('path-id');
	});

	it('is null with neither a path id nor a query param', () => {
		expect(sessionIdFromLocation('/sessions', search())).toBeNull();
	});
});

describe('sessionHrefFor', () => {
	it('builds the /sessions/<id> path and drops a stale ?session=', () => {
		expect(sessionHrefFor('http://h/sessions?session=old', 'new-1')).toBe(
			'http://h/sessions/new-1'
		);
	});

	it('encodes the id in the path', () => {
		expect(sessionHrefFor('http://h/sessions', 'a/b')).toBe('http://h/sessions/a%2Fb');
	});

	it('closes to /sessions when id is null', () => {
		expect(sessionHrefFor('http://h/sessions/abc', null)).toBe('http://h/sessions');
	});

	it('is null (no navigation) when the target already matches the current href', () => {
		expect(sessionHrefFor('http://h/sessions/abc', 'abc')).toBeNull();
		expect(sessionHrefFor('http://h/sessions', null)).toBeNull();
	});

	it('drops ?diagnose when moving to another session', () => {
		expect(sessionHrefFor('http://h/sessions/abc?diagnose=1', 'def')).toBe('http://h/sessions/def');
		expect(sessionHrefFor('http://h/sessions/abc?diagnose=1', null)).toBe('http://h/sessions');
	});

	it('keeps ?diagnose while the url already points at that session', () => {
		expect(sessionHrefFor('http://h/sessions/abc?diagnose=1', 'abc')).toBeNull();
	});
});

describe('hrefWithoutDiagnose', () => {
	it('strips the diagnose param and keeps the rest', () => {
		expect(hrefWithoutDiagnose('http://h/sessions/abc?q=x&diagnose=1')).toBe(
			'http://h/sessions/abc?q=x'
		);
	});

	it('is null when there is no diagnose param', () => {
		expect(hrefWithoutDiagnose('http://h/sessions/abc')).toBeNull();
	});
});

describe('pickFreshSession', () => {
	it('is null when nothing is open', () => {
		expect(pickFreshSession(null, [session({ id: 'a' })])).toBeNull();
	});

	it('returns the live copy from the pools, not the stale fallback', () => {
		const stale = session({ id: 'a', name: 'old' });
		const fresh = session({ id: 'a', name: 'new' });
		expect(pickFreshSession(stale, [session({ id: 'b' }), fresh])?.name).toBe('new');
	});

	it('falls back to the held object when the id is not in any pool', () => {
		const held = session({ id: 'a', name: 'held' });
		expect(pickFreshSession(held, [session({ id: 'b' })])).toBe(held);
	});
});

describe('label filter', () => {
	it('parseLabelFilter splits a comma list and drops empties', () => {
		expect(parseLabelFilter('l1,l2')).toEqual(['l1', 'l2']);
		expect(parseLabelFilter('')).toEqual([]);
		expect(parseLabelFilter(null)).toEqual([]);
		expect(parseLabelFilter('l1,,l2,')).toEqual(['l1', 'l2']);
	});

	it('matchesLabelFilter passes every row when the filter is empty', () => {
		expect(matchesLabelFilter(session({ labels: [label('l1', 'a')] }), new Set())).toBe(true);
	});

	it('matchesLabelFilter keeps rows carrying at least one selected label (OR)', () => {
		const s = session({ labels: [label('l1', 'a'), label('l2', 'b')] });
		expect(matchesLabelFilter(s, new Set(['l2']))).toBe(true);
		expect(matchesLabelFilter(s, new Set(['l9']))).toBe(false);
	});
});

describe('sortSessions', () => {
	const dated = () => [
		session({ id: 'old', registered_at: '2020-01-01T00:00:00Z' }),
		session({ id: 'new', registered_at: '2024-01-01T00:00:00Z' })
	];
	const named = () => [
		session({ id: 'z', name: 'zebra' }),
		session({ id: 'a', name: 'apple' }),
		session({ id: 'm', name: '', working_dir: '/x/mango' })
	];

	it('keeps the server order for activity desc (same reference)', () => {
		const rows = [session({ id: 'a' }), session({ id: 'b' })];
		expect(sortSessions(rows, 'activity')).toBe(rows);
		expect(sortSessions(rows, 'activity', 'desc')).toBe(rows);
	});

	it('reverses a copy for activity asc, leaving the input untouched', () => {
		const rows = [session({ id: 'a' }), session({ id: 'b' })];
		const out = sortSessions(rows, 'activity', 'asc');
		expect(out).not.toBe(rows);
		expect(out.map((s) => s.id)).toEqual(['b', 'a']);
		expect(rows.map((s) => s.id)).toEqual(['a', 'b']);
	});

	it('sorts created newest-first by default and oldest-first ascending', () => {
		expect(sortSessions(dated(), 'created').map((s) => s.id)).toEqual(['new', 'old']);
		expect(sortSessions(dated(), 'created', 'desc').map((s) => s.id)).toEqual(['new', 'old']);
		expect(sortSessions(dated(), 'created', 'asc').map((s) => s.id)).toEqual(['old', 'new']);
	});

	it('sorts by name A→Z by default, Z→A descending, falling back to working-dir basename then id', () => {
		expect(sortSessions(named(), 'name').map((s) => s.id)).toEqual(['a', 'm', 'z']);
		expect(sortSessions(named(), 'name', 'asc').map((s) => s.id)).toEqual(['a', 'm', 'z']);
		expect(sortSessions(named(), 'name', 'desc').map((s) => s.id)).toEqual(['z', 'm', 'a']);
	});

	it('does not mutate the input array', () => {
		const rows = [session({ id: 'b', name: 'b' }), session({ id: 'a', name: 'a' })];
		sortSessions(rows, 'name');
		sortSessions(rows, 'created', 'asc');
		expect(rows.map((s) => s.id)).toEqual(['b', 'a']);
	});
});

describe('nextSort', () => {
	it('re-selecting the active field flips the direction', () => {
		expect(nextSort({ sort: 'activity', sortDir: 'desc' }, 'activity')).toEqual({
			sort: 'activity',
			sortDir: 'asc'
		});
		expect(nextSort({ sort: 'activity', sortDir: 'asc' }, 'activity')).toEqual({
			sort: 'activity',
			sortDir: 'desc'
		});
	});

	it('selecting another field resets to its natural direction', () => {
		expect(nextSort({ sort: 'activity', sortDir: 'asc' }, 'name')).toEqual({
			sort: 'name',
			sortDir: 'asc'
		});
		expect(nextSort({ sort: 'name', sortDir: 'desc' }, 'created')).toEqual({
			sort: 'created',
			sortDir: 'desc'
		});
	});
});

describe('idsForSection', () => {
	it('lists the top-level rows plus every nested subagent, without duplicates', () => {
		const parent = session({ id: 'p' });
		const kidA = session({ id: 'a', parent_id: 'p' });
		const kidB = session({ id: 'b', parent_id: 'p' });
		const grand = session({ id: 'g', parent_id: 'a' });
		const childGroups = new Map([
			[
				'p',
				[{ key: 'plain', runId: null, label: '', agents: [kidA, kidB], running: 0 }]
			],
			[
				'a',
				[{ key: 'plain', runId: null, label: '', agents: [grand, kidB], running: 0 }]
			]
		]);
		expect(idsForSection([parent, session({ id: 'q' })], childGroups)).toEqual([
			'p',
			'a',
			'g',
			'b',
			'q'
		]);
	});

	it('is empty for an empty section', () => {
		expect(idsForSection([], new Map())).toEqual([]);
	});
});

describe('bucketInSection', () => {
	it('maps the pinned bucket to the starred section toggle', () => {
		expect(bucketInSection('pinned', new Set<Section>(['starred']))).toBe(true);
		expect(bucketInSection('pinned', new Set<Section>(['live']))).toBe(false);
	});

	it('maps the dispatched bucket to the dispatched toggle', () => {
		expect(bucketInSection('dispatched', new Set<Section>(['dispatched']))).toBe(true);
		expect(bucketInSection('dispatched', new Set<Section>(['live']))).toBe(false);
	});

	it('maps every other bucket to the live toggle', () => {
		expect(bucketInSection('working', new Set<Section>(['live']))).toBe(true);
		expect(bucketInSection('blocked', new Set<Section>(['live']))).toBe(true);
		expect(bucketInSection('done', new Set<Section>(['starred']))).toBe(false);
	});
});

describe('draft payload + prefills', () => {
	it('draftPayload reads the draft object off metadata, else empty', () => {
		expect(draftPayload(session({ metadata: { draft: { prompt: 'hi' } } }))).toEqual({
			prompt: 'hi'
		});
		expect(draftPayload(session({ metadata: null }))).toEqual({});
		expect(draftPayload(session({ metadata: { draft: 'nope' } }))).toEqual({});
	});

	it('draftPromptPreview returns the stored prompt string, else empty', () => {
		expect(draftPromptPreview(session({ metadata: { draft: { prompt: 'go' } } }))).toBe('go');
		expect(draftPromptPreview(session({ metadata: { draft: {} } }))).toBe('');
	});

	it('scriptPrefill seeds the claude model field by default', () => {
		const p = scriptPrefill(
			session({ machine_id: 'm1', working_dir: '/w', model: 'opus', adapter_id: 'claude-code' })
		);
		expect(p).toEqual({
			machine_id: 'm1',
			working_dir: '/w',
			adapter_id: 'claude-code',
			name: '',
			model_claude: 'opus'
		});
	});

	it('scriptPrefill seeds the codex model field for a codex session', () => {
		const p = scriptPrefill(session({ machine_id: 'm', working_dir: '/w', adapter_id: 'codex', model: 'gpt' }));
		expect(p.model_codex).toBe('gpt');
		expect(p.adapter_id).toBe('codex');
	});

	it('draftEditPrefill prefers stored payload, falling back to the row', () => {
		const p = draftEditPrefill(
			session({
				machine_id: 'row-m',
				working_dir: '/row',
				adapter_id: 'claude-code',
				metadata: { draft: { name: 'd', prompt: 'p', model: 'sonnet', effort: 'high', working_dir: '/draft' } }
			})
		);
		expect(p.name).toBe('d');
		expect(p.prompt).toBe('p');
		expect(p.model_claude).toBe('sonnet');
		expect(p.effort_claude).toBe('high');
		expect(p.working_dir).toBe('/draft');
		expect(p.machine_id).toBe('row-m');
	});

	it('draftEditPrefill routes model/effort to codex fields when the draft is codex', () => {
		const p = draftEditPrefill(
			session({ machine_id: 'm', working_dir: '/w', metadata: { draft: { adapter_id: 'codex', model: 'gpt', effort: 'low' } } })
		);
		expect(p.model_codex).toBe('gpt');
		expect(p.effort_codex).toBe('low');
	});

	it('draftEditPrefill omits empty fields and carries the draft id + env keys', () => {
		const p = draftEditPrefill(
			session({
				id: 'd-1',
				machine_id: 'm',
				working_dir: '/w',
				metadata: { draft: { prompt: 'p', name: '', env_keys: ['TOKEN', 'API_KEY'] } }
			})
		);
		expect(p).toEqual({
			draft_id: 'd-1',
			machine_id: 'm',
			working_dir: '/w',
			adapter_id: 'claude-code',
			prompt: 'p',
			env_keys: 'TOKEN,API_KEY'
		});
		expect('name' in p).toBe(false);
	});

	it('draftSavedAt prefers the autosave stamp over the creation time', () => {
		const at = '2026-09-04T10:00:00Z';
		expect(draftSavedAt(session({ registered_at: '2026-09-01T00:00:00Z', metadata: { draft_saved_at: at } }))).toBe(at);
		expect(draftSavedAt(session({ registered_at: '2026-09-01T00:00:00Z', metadata: { draft: {} } }))).toBe('2026-09-01T00:00:00Z');
	});

	it('draftPreview leads with "autosaved <when>"', () => {
		const s = session({ registered_at: new Date(Date.now() - 120_000).toISOString(), metadata: { draft: { prompt: 'fix it' } } });
		expect(draftPreview(s)).toMatch(/^autosaved .*ago — fix it$/);
	});
});

describe('editDraftNeedsConfirm', () => {
	it('asks when the mounted form is dirty with another draft', () => {
		expect(editDraftNeedsConfirm('d-2', { dirty: true, draftId: 'd-1' }, null)).toBe(true);
		expect(editDraftNeedsConfirm('d-2', { dirty: true, draftId: null }, null)).toBe(true);
	});

	it('opens straight away when the form is clean or already this draft', () => {
		expect(editDraftNeedsConfirm('d-2', { dirty: false, draftId: null }, null)).toBe(false);
		expect(editDraftNeedsConfirm('d-1', { dirty: true, draftId: 'd-1' }, null)).toBe(false);
	});

	it('falls back to the stored slot when the form is closed', () => {
		expect(editDraftNeedsConfirm('d-2', null, { prompt: 'typed', draftId: null })).toBe(true);
		expect(editDraftNeedsConfirm('d-2', null, { prompt: 'typed', draftId: 'd-2' })).toBe(false);
		expect(editDraftNeedsConfirm('d-2', null, { prompt: '', machine_id: 'm', working_dir: '/w' })).toBe(false);
		expect(editDraftNeedsConfirm('d-2', null, null)).toBe(false);
	});
});

describe('spawnRequestFromSlot', () => {
	it('needs a machine, a cwd and a prompt', () => {
		expect(spawnRequestFromSlot({ machine_id: 'm', working_dir: '/w', prompt: ' ' })).toBeNull();
		expect(spawnRequestFromSlot({ machine_id: '', working_dir: '/w', prompt: 'p' })).toBeNull();
	});

	it('carries env keys and attachment names, never values', () => {
		const r = spawnRequestFromSlot({
			machine_id: 'm',
			working_dir: '/w/',
			prompt: 'p',
			adapter_id: 'codex',
			model_codex: 'gpt',
			effort_codex: 'low',
			envRows: [{ key: 'TOKEN', value: 'secret' }, { key: ' ', value: '' }],
			attachmentNames: ['a.txt']
		});
		expect(r).toMatchObject({
			machine_id: 'm',
			working_dir: '/w',
			adapter_id: 'codex',
			model: 'gpt',
			effort: 'low',
			env: {},
			env_keys: ['TOKEN'],
			attachment_names: ['a.txt']
		});
		expect(JSON.stringify(r)).not.toContain('secret');
	});
});

describe('rangeIds', () => {
	const order = ['a', 'b', 'c', 'd', 'e'];
	const all = new Set(order);

	it('returns the inclusive span downwards', () => {
		expect(rangeIds(order, 'b', 'd', all)).toEqual(['b', 'c', 'd']);
	});

	it('returns the same span when clicked upwards', () => {
		expect(rangeIds(order, 'd', 'b', all)).toEqual(['b', 'c', 'd']);
	});

	it('drops ids that are not selectable', () => {
		expect(rangeIds(order, 'a', 'e', new Set(['a', 'c', 'e']))).toEqual(['a', 'c', 'e']);
	});

	it('is empty when an endpoint is not on screen', () => {
		expect(rangeIds(order, 'z', 'c', all)).toEqual([]);
		expect(rangeIds(order, 'c', 'z', all)).toEqual([]);
	});

	it('handles a single-row range', () => {
		expect(rangeIds(order, 'c', 'c', all)).toEqual(['c']);
	});
});

describe('sectionsOf / inEnabledSections', () => {
	const dispatched = { machine_kind: 'dispatch' } as Partial<SessionListItem>;

	it('maps live rows to their single owning section', () => {
		expect(sectionsOf(session({ status: 'active' }))).toEqual(['live']);
		expect(sectionsOf(session({ status: 'active', pinned: true }))).toEqual(['starred']);
		expect(sectionsOf(session({ status: 'active', ...dispatched }))).toEqual(['dispatched']);
		expect(sectionsOf(session({ status: 'draft' }))).toEqual(['drafts']);
		expect(sectionsOf(session({ status: 'queued' }))).toEqual(['live']);
	});

	it('keeps starred/dispatched ownership on archived rows', () => {
		expect(sectionsOf(session({ status: 'archived' }))).toEqual(['archived']);
		expect(sectionsOf(session({ status: 'archived', pinned: true }))).toEqual([
			'archived',
			'starred'
		]);
		expect(sectionsOf(session({ status: 'archived', ...dispatched }))).toEqual([
			'archived',
			'dispatched'
		]);
	});

	it('pinned wins over dispatched, matching the live buckets', () => {
		expect(sectionsOf(session({ status: 'archived', pinned: true, ...dispatched }))).toEqual([
			'archived',
			'starred'
		]);
	});

	it('requires every owning section to be enabled', () => {
		const archivedDispatched = session({ status: 'archived', ...dispatched });
		expect(inEnabledSections(archivedDispatched, new Set<Section>(['archived', 'dispatched']))).toBe(
			true
		);
		expect(inEnabledSections(archivedDispatched, new Set<Section>(['archived']))).toBe(false);
		expect(inEnabledSections(archivedDispatched, new Set<Section>(['dispatched']))).toBe(false);
		expect(inEnabledSections(session({ status: 'archived' }), new Set<Section>(['archived']))).toBe(
			true
		);
		expect(
			inEnabledSections(session({ status: 'archived', pinned: true }), new Set<Section>(['archived']))
		).toBe(false);
	});
});

describe('groupChildren', () => {
	const kid = (id: string, metadata: Record<string, JsonValue>) =>
		session({ id, metadata, status: 'active', liveness: 'active' });

	it('folds every non-workflow child into the single plain group', () => {
		const groups = groupChildren([
			kid('a', { subagent: true, agent_type: 'general-purpose' }),
			kid('b', { subagent: true, agent_type: 'Explore' }),
			kid('c', { subagent: true })
		]);
		expect(groups.map((g) => g.key)).toEqual(['plain']);
		expect(groups[0].agents.map((s) => s.id)).toEqual(['a', 'b', 'c']);
		expect(groups[0].label).toBe('subagents');
		expect(groups[0].running).toBe(3);
	});

	it('keeps workflow children in their run group and never splits by agent type', () => {
		const groups = groupChildren([
			kid('a', { subagent: true }),
			kid('b', { subagent: true, agent_type: 'Explore' }),
			kid('c', { subagent: true, workflow_run_id: 'wf_1', workflow_name: 'deploy' }),
			kid('d', {
				subagent: true,
				workflow_run_id: 'wf_1',
				workflow_name: 'deploy',
				agent_type: 'workflow-subagent'
			})
		]);
		expect(groups.map((g) => g.key)).toEqual(['plain', 'wf:wf_1']);
		expect(groups[0].agents.map((s) => s.id)).toEqual(['a', 'b']);
		expect(groups[1].agents.map((s) => s.id)).toEqual(['c', 'd']);
		expect(groups[1].runId).toBe('wf_1');
	});

	it('has no groups for a parent with no children', () => {
		expect(groupChildren([])).toEqual([]);
	});
});

describe('queueOrder', () => {
	it('lists queued spawns oldest first, as the server launches them', () => {
		const rows = [
			session({ id: 'b', status: 'queued', registered_at: '2026-09-19T10:05:00Z' }),
			session({ id: 'a', status: 'queued', registered_at: '2026-09-19T10:00:00Z' }),
			session({ id: 'c', status: 'queued', registered_at: null })
		];
		expect(queueOrder(rows).map((s) => s.id)).toEqual(['c', 'a', 'b']);
		expect(rows.map((s) => s.id)).toEqual(['b', 'a', 'c']);
	});
});
