import { del, get, keys, set } from 'idb-keyval';
import { MAX_TOTAL_BYTES } from './attachments';

// Draft text lives in localStorage (drafts.ts); File handles cannot, so they
// go to IndexedDB under a key derived from the draft key. Without IndexedDB
// (SSR, tests) an in-memory map still covers a remount within the page.
const PREFIX = 'cctui_files_';
export const attachmentKey = (draftKey: string) => `${PREFIX}${draftKey}`;

/** Composer records are keyed off `composerKey(sessionId)` (drafts.ts), so the
 *  session id is recoverable from the store key — that is what lets the boot
 *  sweep match a record against the session roster. */
const COMPOSER_PREFIX = `${PREFIX}cctui_draft_`;

/** Cached bodies of already-sent attachments, keyed by session so the sweep
 *  can drop them the moment that session is archived. */
const ATTACH_PREFIX = `${PREFIX}att_`;
export const attachmentCacheKey = (sessionId: string, hash: string) =>
	`${ATTACH_PREFIX}${sessionId}:${hash}`;

/** A record untouched for this long is dropped on the next boot. */
export const MAX_AGE_MS = 14 * 24 * 60 * 60 * 1000;

// Names are always recorded; files only while under the cap, so a restore can
// tell "never attached" from "attached but lost".
interface Record {
	names: string[];
	files: File[];
	/** Body of a cached sent attachment; absent on composer records. */
	text?: string;
	/** Last write, epoch ms. Absent on pre-sweep records, which age out at once. */
	updatedAt?: number;
}

export interface RestoredAttachments {
	files: File[];
	missing: string[];
}

const hasIdb = () => typeof indexedDB !== 'undefined';
const memory = new Map<string, Record>();

async function read(key: string): Promise<Record | undefined> {
	if (!hasIdb()) return memory.get(key);
	try {
		return await get<Record>(key);
	} catch {
		return undefined;
	}
}

async function write(key: string, rec: Record): Promise<void> {
	if (!hasIdb()) {
		memory.set(key, rec);
		return;
	}
	try {
		await set(key, rec);
	} catch {
		if (rec.files.length) await write(key, { ...rec, files: [] });
	}
}

async function allKeys(): Promise<string[]> {
	if (!hasIdb()) return [...memory.keys()];
	try {
		const all = await keys();
		return all.filter((k): k is string => typeof k === 'string' && k.startsWith(PREFIX));
	} catch {
		return [];
	}
}

async function remove(key: string): Promise<void> {
	if (!hasIdb()) {
		memory.delete(key);
		return;
	}
	try {
		await del(key);
	} catch {
		// best effort
	}
}

export const attachmentStore = {
	async get(draftKey: string): Promise<RestoredAttachments> {
		const rec = await read(attachmentKey(draftKey));
		if (!rec) return { files: [], missing: [] };
		const files = Array.isArray(rec.files) ? rec.files.filter((f) => f instanceof Blob) : [];
		const present = new Set(files.map((f) => f.name));
		const names = Array.isArray(rec.names) ? rec.names : [];
		return { files, missing: names.filter((n) => !present.has(n)) };
	},
	/** Empty list removes the record. Over-cap lists record names only. */
	async set(draftKey: string, files: File[]): Promise<void> {
		const key = attachmentKey(draftKey);
		if (files.length === 0) return remove(key);
		const total = files.reduce((n, f) => n + f.size, 0);
		const names = files.map((f) => f.name);
		await write(key, {
			names,
			files: total > MAX_TOTAL_BYTES ? [] : files,
			updatedAt: Date.now()
		});
	},
	clear(draftKey: string): Promise<void> {
		return remove(attachmentKey(draftKey));
	},
	/** Bytes held by every record, for the Settings › Storage readout. Records
	 *  whose files were dropped for being over the cap contribute nothing. */
	async totalBytes(): Promise<number> {
		let total = 0;
		for (const key of await allKeys()) {
			const rec = await read(key);
			const files = Array.isArray(rec?.files) ? rec.files : [];
			for (const f of files) if (f instanceof Blob) total += f.size;
			if (typeof rec?.text === 'string') total += rec.text.length;
		}
		return total;
	},
	/** Drop every record the sweep predicate rejects. `sessions` is the roster
	 *  the queries layer already holds; `null` when it isn't loaded, which
	 *  limits the pass to ageing. Returns how many records were removed. */
	async sweep(sessions: SweepSession[] | null, now = Date.now()): Promise<number> {
		const live = sessions
			? new Set(sessions.filter((s) => s.status !== 'archived').map((s) => s.id))
			: null;
		let dropped = 0;
		for (const key of await allKeys()) {
			if (!isStale(key, await read(key), { now, live })) continue;
			await remove(key);
			dropped++;
		}
		return dropped;
	},
	/** A sent attachment's body, cached until its session is archived. */
	async cachedText(sessionId: string, hash: string): Promise<string | null> {
		const rec = await read(attachmentCacheKey(sessionId, hash));
		return typeof rec?.text === 'string' ? rec.text : null;
	},
	async cacheText(sessionId: string, hash: string, text: string): Promise<void> {
		await write(attachmentCacheKey(sessionId, hash), {
			names: [],
			files: [],
			text,
			updatedAt: Date.now()
		});
	},
	async clearAll(): Promise<void> {
		if (!hasIdb()) {
			memory.clear();
			return;
		}
		try {
			await Promise.all((await allKeys()).map((k) => del(k)));
		} catch {
			// best effort
		}
	}
};

/** Restore/persist sequencing for one composer. A restore superseded by a
 *  later restore or by `discard` resolves to null, so a record read before a
 *  send cannot put the sent files back. */
export function attachmentDraftSync(store = attachmentStore) {
	let generation = 0;
	return {
		async restore(draftKey: string): Promise<RestoredAttachments | null> {
			const g = ++generation;
			const restored = await store.get(draftKey);
			return g === generation ? restored : null;
		},
		persist(draftKey: string, files: File[]): Promise<void> {
			return store.set(draftKey, files);
		},
		discard(draftKey: string): Promise<void> {
			generation++;
			return store.clear(draftKey);
		}
	};
}

export interface SweepSession {
	id: string;
	status: string;
}

interface SweepContext {
	now: number;
	/** Ids of sessions still worth keeping a composer draft for; `null` when
	 *  the roster is unknown, in which case composer records are kept. */
	live: Set<string> | null;
}

/** Whether a record should be dropped: missing or corrupt, older than
 *  MAX_AGE_MS (an absent `updatedAt` counts as older), or a composer record
 *  whose session has been archived or deleted. */
export function isStale(key: string, rec: unknown, ctx: SweepContext): boolean {
	if (!rec || typeof rec !== 'object') return true;
	const updatedAt = (rec as Record).updatedAt;
	if (typeof updatedAt !== 'number' || ctx.now - updatedAt > MAX_AGE_MS) return true;
	if (ctx.live && key.startsWith(COMPOSER_PREFIX)) {
		return !ctx.live.has(key.slice(COMPOSER_PREFIX.length));
	}
	if (ctx.live && key.startsWith(ATTACH_PREFIX)) {
		const sid = key.slice(ATTACH_PREFIX.length).split(':')[0];
		return !ctx.live.has(sid);
	}
	return false;
}

const TOKEN = /\[([^[\]\n]+)\]/g;

/** Remove `[name]` tokens whose name is in `missing`, collapsing the
 *  surrounding whitespace. */
export function dropMissingTokens(
	text: string,
	missing: string[]
): { text: string; dropped: number } {
	if (missing.length === 0) return { text, dropped: 0 };
	const gone = new Set(missing);
	let dropped = 0;
	const out = text
		.replace(TOKEN, (tok, name: string) => {
			if (!gone.has(name)) return tok;
			dropped++;
			return '';
		})
		.replace(/[ \t]{2,}/g, ' ')
		.replace(/[ \t]+$/gm, '');
	return { text: dropped ? out : text, dropped };
}
