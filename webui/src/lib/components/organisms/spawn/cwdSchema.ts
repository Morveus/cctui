import type { Query, Schema, ValueOption } from '@dorsk/tsumikit';
import { filters } from '@dorsk/tsumikit';
import { endpoints } from '$lib/queries';

export function dirFromQuery(q: Query): string {
	return filters(q).find((f) => f.field === 'cwd')?.values[0] ?? '';
}

export function makeCwdSchema(
	machineId: () => string,
	recentDirs: () => string[],
	label: string
): Schema {
	const provider = async (query: string): Promise<ValueOption[]> => {
		const id = machineId();
		const out: ValueOption[] = [];
		const seen = new Set<string>();
		const push = (v: string) => {
			if (v && !seen.has(v)) {
				seen.add(v);
				out.push({ value: v, label: v });
			}
		};
		if (!query) for (const d of recentDirs()) push(d);
		if (!id) return out;
		const i = query.lastIndexOf('/');
		const parent = i < 0 ? '' : i === 0 ? '/' : query.slice(0, i);
		const prefix = (i < 0 ? query : query.slice(i + 1)).toLowerCase();
		// Request only once the user crosses a `/`, so a listing fires per level.
		if (!parent) return out;
		try {
			const { dirs } = await endpoints.machineDirs(id, parent);
			const showHidden = prefix.startsWith('.');
			for (const d of dirs) {
				if (!showHidden && d.startsWith('.')) continue;
				if (!d.toLowerCase().startsWith(prefix)) continue;
				push(`${parent === '/' ? '' : parent}/${d}`);
				if (out.length >= 50) break;
			}
		} catch {
			/* transient FS error shouldn't break typing */
		}
		return out;
	};
	return {
		fields: [{ name: 'cwd', label, type: 'string', valuePlaceholder: '/home/user/project', provider }]
	};
}
