import type { MsgCategory, MsgFilter, MsgGroup, QuickFilterId, ViewOpts } from './types';

export const MSG_GROUPS: { id: MsgGroup; categories: MsgCategory[] }[] = [
	{ id: 'assistant', categories: ['assistant', 'thinking', 'redacted', 'attachment'] },
	{ id: 'user', categories: ['user', 'poll', 'peer', 'system'] },
	{ id: 'tools', categories: ['tool', 'mcp', 'server_tool', 'result', 'server_result', 'error'] },
	{ id: 'session', categories: ['marker', 'summary', 'compact', 'reset'] }
];

export const MSG_CATEGORIES: MsgCategory[] = MSG_GROUPS.flatMap((g) => g.categories);

export const QUICK_FILTERS: { id: QuickFilterId; categories: MsgCategory[] }[] = [
	{ id: 'assistant', categories: ['assistant'] },
	{ id: 'user', categories: ['user'] },
	{ id: 'tools', categories: ['tool', 'mcp', 'server_tool', 'result', 'server_result', 'error'] }
];

const HIDDEN_BY_DEFAULT: MsgCategory[] = ['mcp', 'marker'];

export function defaultFilter(): MsgFilter {
	return Object.fromEntries(
		MSG_CATEGORIES.map((c) => [c, !HIDDEN_BY_DEFAULT.includes(c)])
	) as MsgFilter;
}

export function allFilter(on: boolean): MsgFilter {
	return Object.fromEntries(MSG_CATEGORIES.map((c) => [c, on])) as MsgFilter;
}

export function quickCategories(id: QuickFilterId): MsgCategory[] {
	return QUICK_FILTERS.find((q) => q.id === id)?.categories ?? [];
}

export function quickOn(f: MsgFilter, id: QuickFilterId): boolean {
	return quickCategories(id).every((c) => f[c]);
}

export function quickPartial(f: MsgFilter, id: QuickFilterId): boolean {
	const cats = quickCategories(id);
	return cats.some((c) => f[c]) && !cats.every((c) => f[c]);
}

export function withQuick(f: MsgFilter, id: QuickFilterId, on: boolean): MsgFilter {
	const next = { ...f };
	for (const c of quickCategories(id)) next[c] = on;
	return next;
}

// Superseded persisted shape: values were 'off' | 'include' | 'exclude', and one
// 'include' anywhere meant "show only the included categories".
const LEGACY_HEIRS: Record<string, MsgCategory[]> = {
	assistant: ['assistant'],
	thinking: ['thinking', 'redacted'],
	user: ['user', 'poll'],
	system: ['system'],
	tool: ['tool'],
	mcp: ['mcp'],
	result: ['result'],
	summary: ['summary']
};

function fromLegacy(raw: Record<string, unknown>): MsgFilter {
	const anyIncluded = Object.values(raw).some((v) => v === 'include');
	const out = defaultFilter();
	for (const [key, heirs] of Object.entries(LEGACY_HEIRS)) {
		const state = raw[key];
		if (typeof state !== 'string') continue;
		const on = state === 'exclude' ? false : anyIncluded ? state === 'include' : true;
		for (const c of heirs) out[c] = on;
	}
	return out;
}

export function normalizeFilter(raw: unknown): MsgFilter {
	if (!raw || typeof raw !== 'object') return defaultFilter();
	const rec = raw as Record<string, unknown>;
	if (Object.values(rec).some((v) => typeof v === 'string')) return fromLegacy(rec);
	const out = defaultFilter();
	for (const c of MSG_CATEGORIES) {
		const v = rec[c];
		if (typeof v === 'boolean') out[c] = v;
	}
	return out;
}

export function parseViewOpts(raw: string): ViewOpts {
	let saved: Record<string, unknown> = {};
	try {
		const parsed: unknown = JSON.parse(raw || '{}');
		if (parsed && typeof parsed === 'object') saved = parsed as Record<string, unknown>;
	} catch {
		saved = {};
	}
	const width = saved.paneWidth;
	return {
		msgFilter: normalizeFilter(saved.msgFilter ?? saved.typeFilter),
		prettyJson: true,
		prettyDiff: true,
		prettyTables: true,
		paneWidth: typeof width === 'number' && Number.isFinite(width) ? width : null
	};
}
