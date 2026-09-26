import { readdirSync, readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { QueryClient } from '@tanstack/svelte-query';
import { compile, type Journey } from '@dorsk/journey';
import accountsPools from '../../../journeys/accounts-pools.journey';
import enrollMachine from '../../../journeys/enroll-machine.journey';
import followSession from '../../../journeys/follow-session.journey';
import searchSessions from '../../../journeys/search-sessions.journey';
import sessionsList from '../../../journeys/sessions-list.journey';
import settingsTour from '../../../journeys/settings-tour.journey';
import spawnSession from '../../../journeys/spawn-session.journey';
import usageOverview from '../../../journeys/usage-overview.journey';
import welcome from '../../../journeys/welcome.journey';
import {
	DONE_PROBES,
	entryRoute,
	PUBLIC_JOURNEYS,
	READINESS,
	readinessHint,
	requiredParams
} from '../journey';
import { createProbes } from './probes';

const SPECS: Journey[] = [
	welcome,
	enrollMachine,
	accountsPools,
	spawnSession,
	followSession,
	usageOverview,
	sessionsList,
	settingsTour,
	searchSessions
];
const byId = (id: string) => SPECS.find((j) => j.id === id)!;
const pub = (id: string) => compile(byId(id), { public: true });
const book = (id: string) => compile(byId(id));
const captures = (ir: ReturnType<typeof compile>) =>
	ir.steps.flatMap((s) => (s.capture ? [s.capture.name] : []));
// Must mirror the keys guideParams() fills.
const HOST_PARAMS = ['fixture.me', 'account', 'pool', 'fixture.session'];
const FILL_PARAMS = ['var.label', 'var.prompt'];
const PROBES = Object.keys(createProbes(new QueryClient()));

/** The expectation keys whose value addresses the DOM; `url`, `probe` and
 *  `event` carry strings that are not paths. */
const TARGET_KEYS = ['visible', 'hidden', 'enabled', 'disabled', 'text', 'value', 'checked', 'count'];

/** `a/b[key]` addresses `[data-journey="a"] [data-journey="b"][data-journey-key="key"]`,
 *  so every segment name of every path a step walks has to exist in the markup. */
function anchorNames(step: { target?: unknown; expect?: unknown[] }): string[] {
	const out = new Set<string>();
	const walk = (value: unknown) => {
		const path =
			typeof value === 'string' ? value : (value as { within?: string } | null)?.within;
		if (typeof path !== 'string') return;
		for (const segment of path.split('/')) {
			const name = segment.split('[')[0]!.trim();
			if (name && !name.includes('{')) out.add(name);
		}
	};
	walk(step.target);
	for (const e of step.expect ?? []) {
		for (const [key, v] of Object.entries(e as Record<string, unknown>)) {
			if (!TARGET_KEYS.includes(key)) continue;
			walk(Array.isArray(v) ? v[0] : v);
		}
	}
	return [...out];
}

describe('public journey set', () => {
	it('registers every curriculum guide, in curriculum order', () => {
		expect(PUBLIC_JOURNEYS).toEqual([
			'welcome',
			'sessions-list',
			'accounts-pools',
			'enroll-machine',
			'spawn-session',
			'follow-session',
			'search-sessions',
			'usage-overview',
			'settings-tour'
		]);
		expect(pub('search-sessions').steps.map((s) => s.id)).toEqual(['box', 'facets', 'combine']);
	});

	it('has a public tour behind every guide it registers', () => {
		for (const id of PUBLIC_JOURNEYS) {
			expect(byId(id), id).toBeDefined();
			expect(pub(id).steps.length, id).toBeGreaterThan(0);
		}
	});

	it('strips every qaOnly step and qa.* probe from the public IR', () => {
		for (const j of SPECS) {
			const ir = compile(j, { public: true });
			for (const step of ir.steps) {
				expect(step.qaOnly, `${j.id}/${step.id}`).toBeUndefined();
				for (const e of step.expect ?? []) {
					if ('probe' in e) expect(e.probe, `${j.id}/${step.id}`).not.toMatch(/^qa\./);
				}
			}
		}
	});

	it('only references probes the host registers', () => {
		for (const id of PUBLIC_JOURNEYS) {
			for (const step of pub(id).steps) {
				for (const e of step.expect ?? []) {
					if ('probe' in e) expect(PROBES, `${id}/${step.id}`).toContain(e.probe);
				}
			}
		}
		for (const p of Object.values(READINESS)) expect(PROBES).toContain(p);
		for (const p of Object.values(DONE_PROBES)) expect(PROBES).toContain(p);
	});

	it('states what a guide is waiting for in words, for every guide that waits', () => {
		for (const id of Object.keys(READINESS)) {
			expect(PUBLIC_JOURNEYS, id).toContain(id);
			const hint = readinessHint(id);
			expect(hint, id).toBeTruthy();
			expect(hint, id).not.toContain(id);
		}
		for (const id of PUBLIC_JOURNEYS) {
			if (!(id in READINESS)) expect(readinessHint(id), id).toBeUndefined();
		}
	});

	it('addresses real-instance names only through params the host supplies', () => {
		for (const id of PUBLIC_JOURNEYS) {
			const ir = pub(id);
			for (const p of requiredParams(ir)) expect(HOST_PARAMS, `${id} {${p}}`).toContain(p);
			for (const step of ir.steps) {
				for (const [what, json] of [
					['target', JSON.stringify(step.target ?? '')],
					['expect', JSON.stringify(step.expect ?? [])]
				] as const) {
					const where = `${id}/${step.id} ${what}`;
					expect(json, where).not.toMatch(/admin|acme-research|production|a0000000/);
					expect(json, where).not.toMatch(/Machines \d/);
				}
			}
		}
	});

	it('anchors every public step to a data-journey name the app still renders', () => {
		const anchors = new Set<string>();
		for (const file of readdirSync('src', { recursive: true, encoding: 'utf8' })) {
			if (!/\.(svelte|ts)$/.test(file) || file.endsWith('.test.ts')) continue;
			for (const m of readFileSync(`src/${file}`, 'utf8').matchAll(/data-journey="([^"]+)"/g)) {
				anchors.add(m[1]);
			}
		}
		expect(anchors.size).toBeGreaterThan(0);
		for (const id of PUBLIC_JOURNEYS) {
			for (const step of pub(id).steps) {
				for (const name of anchorNames(step)) {
					expect(anchors, `${id}/${step.id} anchors "${name}"`).toContain(name);
				}
			}
		}
	});

	it('can open every anchored guide from the guides page', () => {
		for (const id of PUBLIC_JOURNEYS) {
			const ir = pub(id);
			if (!ir.steps.some((s) => s.target !== undefined)) continue;
			expect(entryRoute(ir), id).toBeTruthy();
		}
	});

	it('opens the welcome tour on the landing route it anchors', () => {
		const tour = pub('welcome');
		expect(tour.steps.every((s) => s.target !== undefined)).toBe(true);
		expect(entryRoute(tour)).toBe('/');
	});

	it('never asks the user to type a prescribed string', () => {
		for (const id of PUBLIC_JOURNEYS) {
			for (const step of pub(id).steps) {
				if (step.do.kind !== 'fill') continue;
				expect(typeof step.do.value, `${id}/${step.id}`).toBe('object');
				expect(FILL_PARAMS).toContain((step.do.value as { $param: string }).$param);
				for (const e of step.expect ?? []) expect('value' in e, `${id}/${step.id}`).toBe(false);
			}
		}
	});

	it('gates the enroll probe wait on a long timeout', () => {
		const enroll = pub('enroll-machine').steps.find((s) => s.id === 'enroll')!;
		expect(enroll.timeout).toBe(600000);
		expect(enroll.expect).toContainEqual({ probe: 'machines.online' });
	});

	it('keeps the follow-session public tour free of mutations', () => {
		for (const step of pub('follow-session').steps) {
			expect(step.do.kind, step.id).not.toBe('fill');
		}
		expect(pub('follow-session').steps.map((s) => s.id)).toEqual([
			'open',
			'header',
			'meta',
			'actions',
			'kinds',
			'line-actions',
			'filters',
			'filter-menu',
			'reply'
		]);
	});

	it('teaches the drawer without depending on a session that may end mid-tour', () => {
		expect(requiredParams(pub('follow-session'))).toEqual([]);
		const open = pub('follow-session').steps[0];
		expect(open.expect).not.toContainEqual({ visible: 'conversation/line[assistant]' });
	});

	it('keeps sessions-list on its own surface and off the theme picker', () => {
		expect(pub('sessions-list').steps.map((s) => s.id)).toEqual([
			'list',
			'search',
			'sections',
			'starred',
			'options',
			'grouping',
			'view',
			'group-sort',
			'group-actions'
		]);
		for (const step of pub('sessions-list').steps) {
			expect(JSON.stringify(step.target ?? ''), step.id).not.toMatch(/data-tsu|theme/i);
		}
	});

	it('lets the group steps skip on an instance that has no groups yet', () => {
		for (const id of ['group-sort', 'group-actions']) {
			const step = pub('sessions-list').steps.find((s) => s.id === id)!;
			expect(step.optional, id).toBe(true);
			for (const e of step.expect ?? []) {
				expect(e, id).toMatchObject({ visible: { nth: 0 } });
			}
		}
	});

	it('indexes every anchor whose component repeats, in both lane specs', () => {
		const indexed: Record<string, string[]> = {
			'sessions-list': ['view', 'group-sort', 'group-actions'],
			'follow-session': ['open', 'kinds', 'line-actions']
		};
		for (const [id, stepIds] of Object.entries(indexed)) {
			for (const stepId of stepIds) {
				const step = pub(id).steps.find((s) => s.id === stepId)!;
				expect(step.target, `${id}/${stepId}`).toMatchObject({ nth: 0 });
			}
		}
	});

	it('counts rather than indexes when an expectation means "all of them"', () => {
		const tools = book('follow-session').steps.find((s) => s.id === 'tools-only')!;
		for (const e of tools.expect ?? []) {
			expect(JSON.stringify(e), 'tools-only').not.toMatch(/nth/);
		}
		expect(tools.expect).toContainEqual({ hidden: 'conversation/line[assistant]' });
	});

	it('adds an account only after the book has captured the board', () => {
		expect(pub('accounts-pools').steps.map((s) => s.id)).toEqual(['board', 'card', 'pools', 'add']);
		const ids = book('accounts-pools').steps.map((s) => s.id);
		expect(ids).toEqual(['board', 'card', 'pool', 'handle', 'menu', 'pools', 'add']);
		expect(ids.indexOf('add')).toBeGreaterThan(ids.lastIndexOf('menu'));
	});

	it('walks spawn-session through the machine and folder before the fills', () => {
		const ids = pub('spawn-session').steps.map((s) => s.id);
		expect(ids).toEqual([
			'open',
			'where',
			'name',
			'prompt',
			'profiles',
			'profile-new',
			'save',
			'sections',
			'show-drafts'
		]);
		const open = pub('spawn-session').steps[0];
		expect(open.expect).not.toContainEqual({ enabled: 'draft' });
		// The draft button only enables once machine+folder are set, so no step
		// may block on it.
		for (const step of pub('spawn-session').steps) {
			expect(step.expect ?? [], step.id).not.toContainEqual({ enabled: 'draft' });
		}
	});
});

describe('book fidelity', () => {
	it('keeps every screenshot capture the docs are built from', () => {
		expect(captures(book('welcome'))).toEqual([
			'overview',
			'sessions',
			'accounts',
			'access',
			'guides'
		]);
		expect(captures(book('enroll-machine'))).toEqual([
			'access',
			'command',
			'enroll',
			'user',
			'machines'
		]);
		expect(captures(book('accounts-pools'))).toEqual(['board', 'pool', 'handle', 'menu']);
		expect(captures(book('spawn-session'))).toEqual([
			'dialog',
			'filled',
			'profiles',
			'saved',
			'draft'
		]);
		expect(captures(book('follow-session'))).toEqual([
			'drawer',
			'header',
			'timeline',
			'line',
			'tools',
			'reply'
		]);
		expect(captures(book('sessions-list'))).toEqual([
			'list',
			'search',
			'sections',
			'options',
			'group'
		]);
		expect(captures(book('search-sessions'))).toEqual(['box', 'before', 'text', 'facet']);
		expect(captures(book('usage-overview'))).toEqual(['tiles', 'periods', 'windows', 'analytics']);
		expect(captures(book('settings-tour'))).toEqual([
			'appearance',
			'theme',
			'sessions',
			'execution',
			'privacy'
		]);
	});

	it('keeps the fixture assertions in the book compile', () => {
		const list = book('sessions-list').steps.find((s) => s.id === 'list-fixture')!;
		expect(list.qaOnly).toBe(true);
		expect(list.expect).toContainEqual({ count: ['session', { min: 4 }] });
		const machines = book('enroll-machine').steps.find((s) => s.id === 'machines')!;
		expect(machines.target).toEqual({ role: 'tab', name: 'Machines 2' });
		expect(book('search-sessions').steps).toHaveLength(6);
	});
	/** Guide mode runs `humanActor`. The engine offers a Next affordance only when
	 *  `step.guide === 'next'`; otherwise the step advances by the user performing
	 *  `step.do`. A step with neither awaits a promise nothing resolves, so the
	 *  tour dead-ends on it and Esc is the only way out. */
	it('leaves no step a user cannot advance past', () => {
		const stuck = PUBLIC_JOURNEYS.flatMap((id) =>
			pub(id)
				.steps.filter((s) => s.guide !== 'next' && s.do === undefined)
				.map((s) => `${id}/${s.id}`)
		);
		expect(stuck).toEqual([]);
	});
});
