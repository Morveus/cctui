import { describe, expect, it } from 'vitest';
import { QueryClient } from '@tanstack/svelte-query';
import type { AgentEvent } from '@bindings/AgentEvent';
import { mergeLiveEvent } from './stream.svelte';

const SID = 's1';
const key = ['conversation', SID] as const;

const text = (seq: number, content: string): AgentEvent => ({
	type: 'text',
	content,
	meta: false,
	ts: seq * 1000,
	seq
});

const bodies = (evs: AgentEvent[] | undefined) =>
	(evs ?? []).map((e) => (e.type === 'text' ? e.content : e.type));

// A real QueryClient on the app's global defaults (+layout.svelte), driving the
// `after=lastSeq` delta of `useConversation`'s queryFn against a fake server.
function harness() {
	const server: AgentEvent[] = [];
	const qc = new QueryClient({
		defaultOptions: { queries: { retry: 1, staleTime: 5_000, refetchOnWindowFocus: false } }
	});
	const queryFn = async () => {
		const prev = qc.getQueryData<AgentEvent[]>(key);
		const lastSeq = (prev ?? []).reduce((m, e) => Math.max(m, e.seq ?? 0), 0);
		if (prev?.length && lastSeq > 0) {
			const delta = server.filter((e) => (e.seq ?? 0) > lastSeq);
			return delta.length ? [...prev, ...delta] : prev;
		}
		return [...server];
	};
	return {
		server,
		qc,
		// Mounting the drawer: TanStack honours `staleTime` unless the mount is
		// forced, which is what `refetchOnMount: "always"` does.
		mount: (force: boolean) => qc.fetchQuery({ queryKey: key, queryFn, staleTime: force ? 0 : 5_000 }),
		// Exactly what the drawer's `mergeIntoCache` opt does per ws event.
		live: (ev: AgentEvent) =>
			qc.setQueryData<AgentEvent[]>(key, (prev) => mergeLiveEvent(prev, ev)),
		cached: () => qc.getQueryData<AgentEvent[]>(key)
	};
}

describe('live events merged into the conversation cache', () => {
	it('leaves no gap when the drawer reopens inside the staleTime window', async () => {
		const h = harness();
		h.server.push(text(1, 'one'));
		await h.mount(true);
		expect(bodies(h.cached())).toEqual(['one']);

		for (const ev of [text(2, 'two'), text(3, 'three')]) {
			h.server.push(ev);
			h.live(ev);
		}

		// Close: `ws.unsubscribe` + `ws.clearStream` discard the live buffer, so
		// the cache is the only surviving copy. Reopen inside the 5s window, with
		// the refetch suppressed — the tail must still be whole.
		await h.mount(false);
		expect(bodies(h.cached())).toEqual(['one', 'two', 'three']);
	});

	it('reopens after the window without duplicating merged events', async () => {
		const h = harness();
		h.server.push(text(1, 'one'));
		await h.mount(true);
		const ev = text(2, 'two');
		h.server.push(ev);
		h.live(ev);
		h.server.push(text(3, 'three'));

		// The delta overlaps what the ws already merged: seq 2 is cached, so the
		// cursor asks for 3 only and no bubble is repeated.
		await h.mount(true);
		expect(bodies(h.cached())).toEqual(['one', 'two', 'three']);
	});

	it('does not re-fetch events the ws already merged', async () => {
		const h = harness();
		h.server.push(text(1, 'one'));
		await h.mount(true);
		const ev = text(2, 'two');
		h.server.push(ev);
		h.live(ev);
		await h.mount(true);
		expect(bodies(h.cached())).toEqual(['one', 'two']);
	});
});

describe('mergeLiveEvent', () => {
	const base = [text(1, 'one'), text(3, 'three')];

	it('inserts by seq rather than arrival order', () => {
		expect(bodies(mergeLiveEvent(base, text(2, 'two')))).toEqual(['one', 'two', 'three']);
	});

	it('appends an event past the tail', () => {
		expect(bodies(mergeLiveEvent(base, text(4, 'four')))).toEqual(['one', 'three', 'four']);
	});

	it('returns prev by identity for a seq already present', () => {
		expect(mergeLiveEvent(base, text(3, 'three again'))).toBe(base);
	});

	it('returns prev by identity for a duplicate eventSig', () => {
		expect(mergeLiveEvent(base, { ...text(9, 'three'), ts: 99 })).toBe(base);
	});

	it('drops a seq-less event so the after= cursor stays on real seqs', () => {
		const optimistic: AgentEvent = { type: 'reply', content: 'typed', ts: 5, seq: undefined };
		expect(mergeLiveEvent(base, optimistic)).toBe(base);
		expect(mergeLiveEvent(base, { type: 'reply', content: 'typed', ts: 5, seq: null })).toBe(base);
	});

	it('never seeds an absent or empty cache entry', () => {
		expect(mergeLiveEvent(undefined, text(7, 'seven'))).toBeUndefined();
		const empty: AgentEvent[] = [];
		expect(mergeLiveEvent(empty, text(7, 'seven'))).toBe(empty);
	});
});
