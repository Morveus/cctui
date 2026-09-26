import { describe, expect, it } from 'vitest';
import {
	MSG_CATEGORIES,
	MSG_GROUPS,
	QUICK_FILTERS,
	allFilter,
	defaultFilter,
	normalizeFilter,
	parseViewOpts,
	quickOn,
	quickPartial,
	withQuick
} from './filters';
import { msgCategoryLabel, msgGroupLabel, quickFilterLabel, type ViewOpts } from './types';

describe('category catalogue', () => {
	it('lists every category exactly once across the groups', () => {
		expect(new Set(MSG_CATEGORIES).size).toBe(MSG_CATEGORIES.length);
		expect(MSG_CATEGORIES).toEqual(MSG_GROUPS.flatMap((g) => g.categories));
	});

	it('carries a translated label for every category, group and quick filter', () => {
		for (const c of MSG_CATEGORIES) expect(msgCategoryLabel(c)).toBeTruthy();
		for (const g of MSG_GROUPS) expect(msgGroupLabel(g.id)).toBeTruthy();
		for (const q of QUICK_FILTERS) expect(quickFilterLabel(q.id)).toBeTruthy();
	});

	it('only ever quick-toggles real categories', () => {
		for (const q of QUICK_FILTERS) {
			for (const c of q.categories) expect(MSG_CATEGORIES).toContain(c);
		}
	});

	it('defaults to everything visible but the two noisy categories', () => {
		const f = defaultFilter();
		expect(Object.keys(f).sort()).toEqual([...MSG_CATEGORIES].sort());
		expect(MSG_CATEGORIES.filter((c) => !f[c])).toEqual(['mcp', 'marker']);
	});
});

describe('quick filters', () => {
	it('reads on only when every member is on', () => {
		const f = allFilter(true);
		expect(quickOn(f, 'tools')).toBe(true);
		expect(quickPartial(f, 'tools')).toBe(false);
	});

	it('reads off — and partial — when only some members are on', () => {
		const f = { ...allFilter(true), mcp: false };
		expect(quickOn(f, 'tools')).toBe(false);
		expect(quickPartial(f, 'tools')).toBe(true);
	});

	it('is neither on nor partial when every member is off', () => {
		const f = allFilter(false);
		expect(quickOn(f, 'tools')).toBe(false);
		expect(quickPartial(f, 'tools')).toBe(false);
	});

	it('sets every member and leaves the rest alone', () => {
		const off = withQuick(allFilter(true), 'tools', false);
		expect([off.tool, off.mcp, off.result]).toEqual([false, false, false]);
		expect(off.assistant).toBe(true);
		expect(off.thinking).toBe(true);

		const on = withQuick(off, 'tools', true);
		expect(quickOn(on, 'tools')).toBe(true);
	});

	it('turning a partial group on fills in the missing members', () => {
		const partial = { ...allFilter(false), tool: true };
		expect(quickPartial(partial, 'tools')).toBe(true);
		expect(quickOn(withQuick(partial, 'tools', true), 'tools')).toBe(true);
	});
});

describe('normalizeFilter', () => {
	it('falls back to the defaults for junk', () => {
		for (const junk of [null, undefined, 42, 'nope', []]) {
			expect(normalizeFilter(junk)).toEqual(defaultFilter());
		}
	});

	it('keeps known booleans and defaults everything else', () => {
		const f = normalizeFilter({ assistant: false, mcp: true, bogus: false });
		expect(f.assistant).toBe(false);
		expect(f.mcp).toBe(true);
		expect(f.summary).toBe(true);
		expect('bogus' in f).toBe(false);
	});
});

describe('migration from the tri-state shape', () => {
	const legacy = {
		assistant: 'off',
		thinking: 'off',
		user: 'off',
		tool: 'off',
		mcp: 'exclude',
		system: 'off',
		result: 'off',
		summary: 'off'
	};

	it('maps exclude → off and off → on', () => {
		const f = normalizeFilter(legacy);
		expect(f.mcp).toBe(false);
		expect(f.assistant).toBe(true);
		expect(f.tool).toBe(true);
		expect(f.summary).toBe(true);
	});

	it('gives categories that did not exist yet their defaults', () => {
		const f = normalizeFilter(legacy);
		expect(f.attachment).toBe(true);
		expect(f.compact).toBe(true);
		expect(f.reset).toBe(true);
		expect(f.marker).toBe(false);
	});

	it('splits the old thinking tag across thinking and redacted thinking', () => {
		const f = normalizeFilter({ ...legacy, thinking: 'exclude' });
		expect(f.thinking).toBe(false);
		expect(f.redacted).toBe(false);
	});

	it('honours the exclusive include semantics', () => {
		const f = normalizeFilter({ ...legacy, user: 'include' });
		expect(f.user).toBe(true);
		expect(f.assistant).toBe(false);
		expect(f.tool).toBe(false);
		expect(f.mcp).toBe(false);
	});

	it('never yields an all-off filter for a partial legacy payload', () => {
		const f = normalizeFilter({ mcp: 'exclude' });
		expect(MSG_CATEGORIES.some((c) => f[c])).toBe(true);
	});
});

describe('parseViewOpts', () => {
	it('round-trips what the drawer persists, with formatting always on', () => {
		const view: ViewOpts = {
			msgFilter: { ...defaultFilter(), thinking: false, marker: true },
			prettyJson: false,
			prettyDiff: true,
			prettyTables: false,
			paneWidth: 640
		};
		expect(parseViewOpts(JSON.stringify(view))).toEqual({
			...view,
			prettyJson: true,
			prettyTables: true
		});
	});

	it('defaults on empty, missing or corrupt storage', () => {
		const fresh: ViewOpts = {
			msgFilter: defaultFilter(),
			prettyJson: true,
			prettyDiff: true,
			prettyTables: true,
			paneWidth: null
		};
		expect(parseViewOpts('')).toEqual(fresh);
		expect(parseViewOpts('{')).toEqual(fresh);
		expect(parseViewOpts('"a string"')).toEqual(fresh);
	});

	it('migrates a blob written by the tri-state build', () => {
		const stored = JSON.stringify({
			typeFilter: { assistant: 'off', mcp: 'exclude', result: 'exclude', summary: 'off' },
			prettyJson: false,
			prettyDiff: true,
			prettyTables: true,
			paneWidth: 900
		});
		const view = parseViewOpts(stored);
		expect(view.msgFilter.result).toBe(false);
		expect(view.msgFilter.mcp).toBe(false);
		expect(view.msgFilter.assistant).toBe(true);
		expect(view.prettyJson).toBe(true);
		expect(view.paneWidth).toBe(900);
		expect(Object.keys(view).sort()).toEqual([
			'msgFilter',
			'paneWidth',
			'prettyDiff',
			'prettyJson',
			'prettyTables'
		]);
	});

	it('drops a non-numeric pane width', () => {
		expect(parseViewOpts('{"paneWidth":"wide"}').paneWidth).toBeNull();
		expect(parseViewOpts('{"paneWidth":null}').paneWidth).toBeNull();
	});
});

describe('poll category', () => {
	it('lives in the user group and is a known category', () => {
		const user = MSG_GROUPS.find((g) => g.id === 'user');
		expect(user?.categories).toContain('poll');
		expect(MSG_CATEGORIES).toContain('poll');
	});

	it('is on by default, so a session never looks like it lost turns', () => {
		expect(defaultFilter().poll).toBe(true);
	});

	it('is not swept by the USER quick filter, so it can be hidden alone', () => {
		expect(QUICK_FILTERS.find((q) => q.id === 'user')?.categories).not.toContain('poll');
		const off = withQuick(defaultFilter(), 'user', false);
		expect(off.poll).toBe(true);
		expect(off.user).toBe(false);
	});

	it('inherits the legacy user toggle', () => {
		expect(normalizeFilter({ user: 'exclude' }).poll).toBe(false);
	});
});
