import { beforeEach, describe, expect, it } from 'vitest';
import {
	attachmentDraftSync,
	attachmentStore,
	dropMissingTokens,
	isStale,
	MAX_AGE_MS
} from './attachmentStore';
import { MAX_TOTAL_BYTES } from './attachments';

const KEY = 'cctui_spawn_draft';
const file = (name: string, size = 3) => new File(['x'.repeat(size)], name, { type: 'text/plain' });

beforeEach(async () => {
	await attachmentStore.clearAll();
});

describe('attachmentStore', () => {
	it('round-trips files under a draft key', async () => {
		await attachmentStore.set(KEY, [file('a.txt'), file('b.png')]);
		const r = await attachmentStore.get(KEY);
		expect(r.files.map((f) => f.name)).toEqual(['a.txt', 'b.png']);
		expect(r.missing).toEqual([]);
	});

	it('empty list removes the record', async () => {
		await attachmentStore.set(KEY, [file('a.txt')]);
		await attachmentStore.set(KEY, []);
		expect(await attachmentStore.get(KEY)).toEqual({ files: [], missing: [] });
	});

	it('over-cap lists keep names only, reported as missing', async () => {
		await attachmentStore.set(KEY, [file('big.bin', MAX_TOTAL_BYTES + 1)]);
		const r = await attachmentStore.get(KEY);
		expect(r.files).toEqual([]);
		expect(r.missing).toEqual(['big.bin']);
	});

	it('clear and clearAll drop records', async () => {
		await attachmentStore.set(KEY, [file('a.txt')]);
		await attachmentStore.set('cctui_draft_s1', [file('b.txt')]);
		await attachmentStore.clear(KEY);
		expect((await attachmentStore.get(KEY)).files).toEqual([]);
		expect((await attachmentStore.get('cctui_draft_s1')).files.length).toBe(1);
		await attachmentStore.clearAll();
		expect((await attachmentStore.get('cctui_draft_s1')).files).toEqual([]);
	});
});

describe('isStale', () => {
	const now = 1_700_000_000_000;
	const fresh = { names: ['a.txt'], files: [], updatedAt: now - 1000 };
	const live = new Set(['s1']);
	const spawn = 'cctui_files_cctui_spawn_draft\u001fm1\u001f/repo';
	const composer = 'cctui_files_cctui_draft_s1';

	it('keeps a fresh record whose session is live', () => {
		expect(isStale(composer, fresh, { now, live })).toBe(false);
		expect(isStale(spawn, fresh, { now, live })).toBe(false);
	});

	it('drops a record past the age cap, whatever its key', () => {
		const old = { ...fresh, updatedAt: now - MAX_AGE_MS - 1 };
		expect(isStale(spawn, old, { now, live })).toBe(true);
		expect(isStale(composer, old, { now, live })).toBe(true);
	});

	it('drops a record with no timestamp or no body', () => {
		expect(isStale(spawn, { names: [], files: [] }, { now, live })).toBe(true);
		expect(isStale(spawn, undefined, { now, live })).toBe(true);
	});

	it('drops a composer record whose session is archived or gone', () => {
		expect(isStale('cctui_files_cctui_draft_s9', fresh, { now, live })).toBe(true);
	});

	it('keeps composer records when the roster is unknown', () => {
		expect(isStale('cctui_files_cctui_draft_s9', fresh, { now, live: null })).toBe(false);
	});

	it('drops a cached attachment body once its session is archived', () => {
		const cached = { names: [], files: [], text: 'hello', updatedAt: now - 1000 };
		expect(isStale('cctui_files_att_s1:abc123', cached, { now, live })).toBe(false);
		expect(isStale('cctui_files_att_s9:abc123', cached, { now, live })).toBe(true);
		expect(isStale('cctui_files_att_s9:abc123', cached, { now, live: null })).toBe(false);
	});
});

describe('sweep', () => {
	it('removes aged records and composers of dead sessions, keeping the rest', async () => {
		await attachmentStore.set('cctui_draft_s1', [file('live.txt')]);
		await attachmentStore.set('cctui_draft_s2', [file('archived.txt')]);
		await attachmentStore.set('cctui_draft_s3', [file('gone.txt')]);
		await attachmentStore.set(KEY, [file('spawn.txt')]);

		const dropped = await attachmentStore.sweep([
			{ id: 's1', status: 'running' },
			{ id: 's2', status: 'archived' }
		]);

		expect(dropped).toBe(2);
		expect((await attachmentStore.get('cctui_draft_s1')).files.length).toBe(1);
		expect((await attachmentStore.get('cctui_draft_s2')).files).toEqual([]);
		expect((await attachmentStore.get('cctui_draft_s3')).files).toEqual([]);
		expect((await attachmentStore.get(KEY)).files.length).toBe(1);
	});

	it('ages every record out once past the cap', async () => {
		await attachmentStore.set(KEY, [file('a.txt')]);
		await attachmentStore.set('cctui_draft_s1', [file('b.txt')]);
		const dropped = await attachmentStore.sweep(null, Date.now() + MAX_AGE_MS + 1);
		expect(dropped).toBe(2);
		expect(await attachmentStore.totalBytes()).toBe(0);
	});
});

describe('totalBytes', () => {
	it('sums the stored files across every record', async () => {
		await attachmentStore.set(KEY, [file('a.txt', 10), file('b.txt', 5)]);
		await attachmentStore.set('cctui_draft_s1', [file('c.txt', 7)]);
		expect(await attachmentStore.totalBytes()).toBe(22);
	});

	it('counts nothing for an over-cap record, whose files are not kept', async () => {
		await attachmentStore.set(KEY, [file('big.bin', MAX_TOTAL_BYTES + 1)]);
		expect(await attachmentStore.totalBytes()).toBe(0);
	});

	it('is zero on an empty store', async () => {
		expect(await attachmentStore.totalBytes()).toBe(0);
	});
});

describe('dropMissingTokens', () => {
	it('removes only tokens of missing files', () => {
		const r = dropMissingTokens('fix [a.txt] and [b.txt] now [keep]', ['b.txt']);
		expect(r).toEqual({ text: 'fix [a.txt] and now [keep]', dropped: 1 });
	});

	it('is a no-op without missing names', () => {
		const text = 'see [a.txt]';
		expect(dropMissingTokens(text, [])).toEqual({ text, dropped: 0 });
	});

	it('trims a trailing token', () => {
		expect(dropMissingTokens('prompt [paste-1.txt]', ['paste-1.txt']).text).toBe('prompt');
	});
});

describe('attachmentDraftSync', () => {
	it('a restore in flight when the draft is sent does not bring the files back', async () => {
		const sync = attachmentDraftSync();
		await sync.persist(KEY, [file('paste-1.txt')]);
		const pending = sync.restore(KEY);
		await sync.discard(KEY);
		expect(await pending).toBeNull();
		expect(await attachmentStore.get(KEY)).toEqual({ files: [], missing: [] });
		const next = await sync.restore(KEY);
		expect(next).toEqual({ files: [], missing: [] });
	});

	it('only the latest of overlapping restores applies', async () => {
		const sync = attachmentDraftSync();
		await sync.persist(KEY, [file('a.txt')]);
		const first = sync.restore(KEY);
		const second = sync.restore(KEY);
		expect(await first).toBeNull();
		expect((await second)?.files.map((f) => f.name)).toEqual(['a.txt']);
	});
});
