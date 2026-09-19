import { filters, parse, type FilterNode, type Schema, type ValueOption } from '@dorsk/tsumikit';
import type { SessionListItem } from '@bindings/SessionListItem';
import { m } from '$lib/paraglide/messages';

export const SERVER_FIELDS = new Set([
	'machine',
	'account',
	'tag',
	'title',
	'status',
	'model',
	'effort',
	'adapter',
	'pinned',
	'dir'
]);

/** The free-text search bar placeholder, in the active locale. A function (not a
 *  const) so it re-reads the locale each render. */
export const sessionSearchPlaceholder = (): string => m.search_bar_placeholder();

export type FetchValues = (field: string, q: string) => Promise<string[]>;

function providerFor(field: string, fetchValues: FetchValues) {
	return async (q: string): Promise<ValueOption[]> => {
		const vals = await fetchValues(field, q);
		return vals.map((v) => ({ value: v, label: v }));
	};
}

export function buildSessionSearchSchema(fetchValues: FetchValues): Schema {
	return {
		fields: [
			{
				name: 'title',
				label: m.search_field_title(),
				type: 'string',
				aliases: ['name'],
				operators: ['contains', 'not_contains'],
				valuePlaceholder: m.search_placeholder_title()
			},
			{
				name: 'machine',
				label: m.search_field_machine(),
				type: 'id',
				aliases: ['m'],
				operators: ['eq', 'ne', 'in'],
				provider: providerFor('machine', fetchValues)
			},
			{
				name: 'account',
				label: m.search_field_account(),
				type: 'id',
				aliases: ['acct'],
				operators: ['eq', 'ne', 'in'],
				provider: providerFor('account', fetchValues)
			},
			{
				name: 'tag',
				label: m.search_field_label(),
				type: 'enum',
				aliases: ['label'],
				operators: ['eq', 'ne', 'in'],
				provider: providerFor('tag', fetchValues)
			},
			{
				name: 'status',
				label: m.search_field_status(),
				type: 'enum',
				operators: ['eq', 'ne', 'in'],
				options: ['new', 'active', 'inactive', 'archived', 'draft', 'queued'].map((v) => ({
					value: v,
					label: v
				}))
			},
			{
				name: 'model',
				label: m.search_field_model(),
				type: 'string',
				operators: ['contains', 'not_contains'],
				provider: providerFor('model', fetchValues)
			},
			{
				name: 'effort',
				label: m.search_field_effort(),
				type: 'enum',
				operators: ['eq', 'ne'],
				options: ['low', 'high'].map((v) => ({ value: v, label: v }))
			},
			{
				name: 'adapter',
				label: m.search_field_adapter(),
				type: 'enum',
				operators: ['eq', 'ne'],
				options: ['claude-code', 'codex'].map((v) => ({ value: v, label: v }))
			},
			{
				name: 'pinned',
				label: m.search_field_pinned(),
				type: 'bool',
				aliases: ['starred'],
				operators: ['eq'],
				options: [
					{ value: 'true', label: 'true' },
					{ value: 'false', label: 'false' }
				]
			},
			{
				name: 'dir',
				label: m.search_field_dir(),
				type: 'string',
				aliases: ['cwd'],
				operators: ['contains', 'not_contains'],
				valuePlaceholder: '/path/fragment…'
			},
			{
				name: 'id',
				label: m.search_field_id(),
				type: 'string',
				operators: ['contains', 'not_contains'],
				valuePlaceholder: m.search_placeholder_id()
			},
			{
				name: 'created',
				label: m.search_field_created(),
				type: 'date',
				aliases: ['time'],
				operators: ['gte', 'lte', 'gt', 'lt', 'range'],
				valuePlaceholder: 'YYYY-MM-DD'
			}
		]
	};
}

/** The rest of the search bar as a server-parseable `context` for value
 *  autocomplete: the raw query with `field`'s clause (the one being completed)
 *  and all client-only clauses stripped by span, so suggestions co-occur with
 *  what's already typed. */
export function contextForField(raw: string, schema: Schema, field: string): string {
	const ast = parse(raw, schema);
	const drop = filters(ast).filter((f) => f.field === field || !SERVER_FIELDS.has(f.field));
	let out = raw;
	for (const f of [...drop].sort((a, b) => b.span[0] - a.span[0])) {
		const [a, b] = f.span;
		let end = b;
		while (out[end] === ' ') end++;
		out = out.slice(0, a) + out.slice(end);
	}
	return out.replace(/\s{2,}/g, ' ').trim();
}

export interface SplitQuery {
	serverQuery: string;
	clientFilters: FilterNode[];
}

// Client-only clauses are peeled off the raw string: the server treats an
// unknown `field:value` as literal free text (matching nothing), so `id`/
// `created` must be stripped before send and applied to the loaded list.
export function splitQuery(raw: string, schema: Schema): SplitQuery {
	const ast = parse(raw, schema);
	const clientFilters = filters(ast).filter((f) => !SERVER_FIELDS.has(f.field));
	let serverQuery = raw;
	for (const f of [...clientFilters].sort((a, b) => b.span[0] - a.span[0])) {
		const [a, b] = f.span;
		let end = b;
		while (serverQuery[end] === ' ') end++;
		serverQuery = serverQuery.slice(0, a) + serverQuery.slice(end);
	}
	return { serverQuery: serverQuery.replace(/\s{2,}/g, ' ').trim(), clientFilters };
}

const DAY_MS = 86_400_000;

function matchesDate(t: number, f: FilterNode): boolean {
	const at = Date.parse(f.values[0] ?? '');
	if (Number.isNaN(at)) return true;
	switch (f.op) {
		case 'gt':
			return t >= at + DAY_MS;
		case 'gte':
			return t >= at;
		case 'lt':
			return t < at;
		case 'lte':
			return t < at + DAY_MS;
		case 'range': {
			const bt = Date.parse(f.values[1] ?? '');
			return t >= at && (Number.isNaN(bt) || t < bt + DAY_MS);
		}
		default:
			return true;
	}
}

function matchesOne(s: SessionListItem, f: FilterNode): boolean {
	if (f.field === 'id') {
		const hit = f.values.some((v) => s.id.toLowerCase().includes(v.toLowerCase()));
		return f.op === 'not_contains' ? !hit : hit;
	}
	if (f.field === 'created') {
		const t = s.registered_at ? Date.parse(s.registered_at) : Number.NaN;
		return Number.isNaN(t) ? false : matchesDate(t, f);
	}
	return true;
}

/** AND-narrow a session against the client-only clauses from {@link splitQuery}. */
export function matchesClientFilters(s: SessionListItem, clientFilters: FilterNode[]): boolean {
	return clientFilters.every((f) => matchesOne(s, f));
}
