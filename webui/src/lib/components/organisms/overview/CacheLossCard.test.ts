import { afterEach, describe, expect, it, vi } from 'vitest';
import { mount, unmount } from 'svelte';
import type { DailyCacheLoss } from '$lib/queries';

const requested: number[] = [];
const query = { isLoading: false, data: [] as DailyCacheLoss[] };
vi.mock('$lib/queries', () => ({
	useCacheLoss: (days: () => number) => {
		requested.push(days());
		return query;
	}
}));

import CacheLossCard from './CacheLossCard.svelte';

let comp: ReturnType<typeof mount> | null = null;
afterEach(() => {
	if (comp) unmount(comp);
	comp = null;
	document.body.innerHTML = '';
	requested.length = 0;
});

function render(props: { days?: number }) {
	const host = document.createElement('div');
	document.body.appendChild(host);
	comp = mount(CacheLossCard, { target: host, props });
	return host;
}

describe('CacheLossCard', () => {
	it('requests 7 days when the section range is 30', () => {
		render({ days: 30 });
		expect(requested).toEqual([7]);
	});

	it('requests 7 days when no range is given', () => {
		render({});
		expect(requested).toEqual([7]);
	});

	it('keeps a shorter range as-is', () => {
		render({ days: 1 });
		expect(requested).toEqual([1]);
	});
});
