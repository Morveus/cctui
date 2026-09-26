import { browser } from '$app/environment';
import { attachmentStore } from './attachmentStore';

/** localStorage-backed draft text (composer per session, spawn form). */
export const drafts = {
	get(key: string): string {
		return browser ? (localStorage.getItem(key) ?? '') : '';
	},
	set(key: string, value: string) {
		if (!browser) return;
		if (value) localStorage.setItem(key, value);
		else localStorage.removeItem(key);
	},
	clear(key: string) {
		if (browser) localStorage.removeItem(key);
	}
};

export const composerKey = (sessionId: string) => `cctui_draft_${sessionId}`;
export const historyKey = (sessionId: string) => `cctui_history_${sessionId}`;

export const PROMPT_HISTORY = 'cctui_prompt_history';

const HISTORY_MAX = 5;
/** Spawns are rarer and more varied than replies, so a deeper recall pays off. */
const PROMPT_HISTORY_MAX = 15;

function readHistory(key: string): string[] {
	if (!browser) return [];
	try {
		const raw = localStorage.getItem(key);
		const arr = raw ? JSON.parse(raw) : [];
		return Array.isArray(arr) ? arr.filter((x): x is string => typeof x === 'string') : [];
	} catch {
		return [];
	}
}

function pushHistory(key: string, value: string, max: number) {
	if (!browser) return;
	const v = value.trim();
	if (!v) return;
	const list = readHistory(key).filter((x) => x !== v);
	list.push(v);
	localStorage.setItem(key, JSON.stringify(list.slice(-max)));
}

/** localStorage-backed per-session sent-message history (most-recent-last,
 * capped at HISTORY_MAX). Used for ArrowUp/ArrowDown recall in the composer. */
export const history = {
	get: (sessionId: string) => readHistory(historyKey(sessionId)),
	push: (sessionId: string, value: string) => pushHistory(historyKey(sessionId), value, HISTORY_MAX),
	clear(sessionId: string) {
		if (browser) localStorage.removeItem(historyKey(sessionId));
	}
};

/** Global (not per-session) history of prompts used to spawn, sharing the
 * composer history's shape: most-recent-last, de-duped, capped. */
export const promptHistory = {
	get: () => readHistory(PROMPT_HISTORY),
	push: (value: string) => pushHistory(PROMPT_HISTORY, value, PROMPT_HISTORY_MAX),
	clear() {
		if (browser) localStorage.removeItem(PROMPT_HISTORY);
	}
};

/** Remove all localStorage tied to a session (draft + sent-message history).
 * Called when a conversation is archived. */
export function clearSessionStorage(sessionId: string) {
	if (!browser) return;
	localStorage.removeItem(composerKey(sessionId));
	localStorage.removeItem(historyKey(sessionId));
	void attachmentStore.clear(composerKey(sessionId));
}

/** Remove the spawn slot for a (machine, cwd) target: its autosaved payload,
 * its attachments, and the resume pointer when it names this slot. Called when
 * the draft behind the slot is launched or discarded, so a later form for the
 * same cwd does not restore files from a draft that no longer exists. */
export function clearSpawnSlot(machineId: string, workingDir: string) {
	if (!browser) return;
	const key = spawnSlotKey(machineId, workingDir);
	drafts.clear(key);
	if (drafts.get(SPAWN_SLOT) === key) drafts.clear(SPAWN_SLOT);
	void attachmentStore.clear(key);
}

/** Wipe every `cctui`-namespaced key from both web storages (drafts, sent
 * history, view options, settings mirror, theme/font/notify, gh-review token).
 * Called on logout so a shared browser never hands the next user the previous
 * user's prompts or a cached bearer. */
export function clearCctuiStorage() {
	if (!browser) return;
	for (const store of [localStorage, sessionStorage]) {
		const doomed: string[] = [];
		for (let i = 0; i < store.length; i++) {
			const k = store.key(i);
			if (k?.startsWith('cctui')) doomed.push(k);
		}
		for (const k of doomed) store.removeItem(k);
	}
	void attachmentStore.clearAll();
}

/** Canonicalize a working-directory path for storage/dedup: strip
 * trailing slashes so `folder` and `folder/` collapse to one `folder`, but
 * keep the filesystem root `/` (a bare run of slashes) intact. Leaves the
 * empty string as-is. */
export function normalizeDir(path: string): string {
	if (!path) return path;
	const stripped = path.replace(/\/+$/, '');
	return stripped === '' ? '/' : stripped;
}

/** The spawn form's local autosave, one slot per (machine, cwd) target:
 * `cctui_spawn_draft` + SEP + machine + SEP + cwd. The pointer key names the
 * slot in progress so a reopen resumes it. */
export const SPAWN_DRAFT = 'cctui_spawn_draft';
export const SPAWN_SLOT = 'cctui_spawn_slot';
const SLOT_SEP = '\u001f';

export function spawnSlotKey(machineId: string, workingDir: string): string {
	return `${SPAWN_DRAFT}${SLOT_SEP}${machineId}${SLOT_SEP}${normalizeDir(workingDir.trim())}`;
}

/** The slot a reopen resumes: the pointer's, else the legacy single slot. */
export function currentSpawnSlot(): string {
	return drafts.get(SPAWN_SLOT) || SPAWN_DRAFT;
}

export interface SpawnSlotPayload {
	prompt?: string;
	name?: string;
	machine_id?: string;
	working_dir?: string;
	adapter_id?: string;
	permission_mode?: string;
	account?: string;
	account_provider?: string;
	model_claude?: string;
	model_codex?: string;
	model_account?: string;
	effort_claude?: string;
	effort_codex?: string;
	labels?: string[];
	envRows?: { key: string; value?: string }[];
	/** Server draft row this slot autosaves into, once created. */
	draftId?: string | null;
	attachmentNames?: string[];
	[k: string]: unknown;
}

export function readSpawnSlot(key: string): SpawnSlotPayload | null {
	const raw = drafts.get(key);
	if (!raw) return null;
	try {
		const v = JSON.parse(raw);
		return v && typeof v === 'object' ? (v as SpawnSlotPayload) : null;
	} catch {
		return null;
	}
}

/** Whether a slot holds anything the user would miss: a prompt, a name, an
 * env key or an attachment. Config alone (machine, cwd, model) is not dirt. */
export function spawnSlotDirty(p: SpawnSlotPayload | null): boolean {
	if (!p) return false;
	return (
		!!p.prompt?.trim() ||
		!!p.name?.trim() ||
		(p.envRows ?? []).some((r) => r.key?.trim()) ||
		(p.attachmentNames ?? []).length > 0
	);
}
export const LAST_MACHINE = 'cctui_last_machine';

/** The session name last submitted from the spawn dialog (either target).
 * A fresh dialog open proposes it with a bumped numeric suffix. */
export const LAST_SPAWN_NAME = 'cctui_last_spawn_name';

/** Label ids (comma-joined) last attached from the spawn dialog. A
 * fresh dialog open defaults its label picker to this set; an empty submit
 * clears it. */
export const LAST_SPAWN_LABELS = 'cctui_last_spawn_labels';
export const FOLLOWUP_ARCHIVE_SOURCE = 'cctui_followup_archive_source';

/** Next proposed session name: bump a trailing `-<n>` suffix, else append
 * `-2` (`toto` → `toto-2`, `toto-5` → `toto-6`). Zero-padding is kept
 * (`run-09` → `run-10`). */
export function nextSessionName(last: string): string {
	const m = last.match(/^(.*)-(\d+)$/);
	if (!m) return `${last}-2`;
	const next = String(Number(m[2]) + 1);
	return `${m[1]}-${next.padStart(m[2].length, '0')}`;
}
export const VIEW_OPTS = 'cctui_view_opts';
export const LIST_DENSITY = 'cctui_list_density';
// Main session list layout: 'list' (rows, default) or 'card' (responsive
// grid of detailed cards).
export const LIST_VIEW = 'cctui_list_view';
// Which session section is in view: 'starred' | 'live' | 'dispatched'
// | 'archived'. Replaces the old archived on/off checkbox with a 4-way picker.
export const LIST_SECTION = 'cctui_list_section';
// Section headers hidden by their eye toggle, comma-joined section keys.
export const LIST_HIDDEN = 'cctui_list_hidden';
// Selected label-filter ids, comma-joined. Empty = show all.
export const LIST_LABELS = 'cctui_list_labels';
