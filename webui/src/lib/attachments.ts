// Shared file-attachment helpers for the spawn modal and the mid-chat
// composer: one source of truth for caps, unique-name merging, error
// derivation, and size formatting. Mirrors the server caps in
// `cctui-server/src/uploads.rs` so we reject before uploading.

export const MAX_FILE_BYTES = 5 * 1024 * 1024;
export const MAX_TOTAL_BYTES = 20 * 1024 * 1024;
export const MAX_FILES = 10;

/** Split `name` into stem and extension (`a.tar.gz` → `a.tar` + `.gz`). */
function splitExt(name: string): [string, string] {
	const i = name.lastIndexOf('.');
	return i > 0 ? [name.slice(0, i), name.slice(i)] : [name, ''];
}

/** Merge `incoming` into `current` with names kept unique: a clash is renamed
 *  `stem-2.ext`, `stem-3.ext`, … (never replaced), so the list always matches
 *  what the daemon stages. Returns the merged list and the incoming files as
 *  actually added (post-rename), for tokenizing. */
export function mergeFilesRenamed(
	current: File[],
	incoming: File[]
): { files: File[]; added: File[] } {
	const taken = new Set(current.map((f) => f.name));
	const added: File[] = [];
	for (const f of incoming) {
		let file = f;
		if (taken.has(f.name)) {
			const [stem, ext] = splitExt(f.name);
			let n = 2;
			while (taken.has(`${stem}-${n}${ext}`)) n++;
			file = new File([f], `${stem}-${n}${ext}`, { type: f.type, lastModified: f.lastModified });
		}
		taken.add(file.name);
		added.push(file);
	}
	return { files: [...current, ...added], added };
}

/** Merge `incoming` into `current`, renaming duplicate names (see
 *  `mergeFilesRenamed`). */
export function mergeFiles(current: File[], incoming: File[]): File[] {
	return mergeFilesRenamed(current, incoming).files;
}

/** Add `incoming` to `files` and reference each (post-rename) name in `text`. */
export function attachFiles(
	files: File[],
	text: string,
	incoming: File[]
): { files: File[]; text: string } {
	const merged = mergeFilesRenamed(files, incoming);
	return { files: merged.files, text: appendFileTokens(text, merged.added) };
}

const PASTE_NAME = /\bpaste-(\d+)\.txt\b/g;

/** Next free `paste-N.txt` index: max N over attachment names, `[paste-N.txt]`
 *  tokens in `text` and `used` (the names the session already staged), plus one.
 *  Derived, not counted, so it survives a remount whose draft still references
 *  earlier pastes. Without `used` every fresh draft would restart at
 *  `paste-1.txt` and collide with an earlier message's upload. */
export function nextPasteIndex(files: File[], text: string, used: Iterable<string> = []): number {
	let max = 0;
	const scan = (s: string) => {
		for (const m of s.matchAll(PASTE_NAME)) max = Math.max(max, Number(m[1]));
	};
	for (const f of files) scan(f.name);
	for (const name of used) scan(name);
	scan(text);
	return max + 1;
}

/** Point each `[name]` token at the name staging actually gave the file.
 *  `paths` is the staged absolute path per entry of `files`, in order; a clash
 *  is renamed server-side (`paste-1.txt` → `paste-1-1.txt`) and a token left
 *  on the old name resolves to some other message's upload. */
export function rewriteFileTokens(text: string, files: File[], paths: string[]): string {
	let out = text;
	files.forEach((f, i) => {
		const staged = paths[i]?.split('/').pop();
		if (!staged || staged === f.name) return;
		out = out.split(`[${f.name}]`).join(`[${staged}]`);
	});
	return out;
}

/** Append a `[name]` reference for each attached file to the draft text,
 *  skipping names it already contains so a re-pick doesn't duplicate. */
export function appendFileTokens(text: string, files: File[]): string {
	let out = text;
	for (const f of files) {
		const token = `[${f.name}]`;
		if (out.includes(token)) continue;
		out = out && !/\s$/.test(out) ? `${out} ${token}` : `${out}${token}`;
	}
	return out;
}

/** Drop the file with `name` from the list. */
export function removeFileByName(files: File[], name: string): File[] {
	return files.filter((f) => f.name !== name);
}

/** Validate a file list against the caps; returns a human error or '' if ok. */
export function fileCapError(files: File[]): string {
	const total = files.reduce((n, f) => n + f.size, 0);
	if (files.some((f) => f.size > MAX_FILE_BYTES))
		return `A file exceeds the ${MAX_FILE_BYTES / 1024 / 1024} MB per-file cap`;
	if (files.length > MAX_FILES) return `Too many files (max ${MAX_FILES})`;
	if (total > MAX_TOTAL_BYTES)
		return `Attachments exceed the ${MAX_TOTAL_BYTES / 1024 / 1024} MB total cap`;
	return '';
}

/** Human-readable byte size. */
export function fmtSize(n: number): string {
	if (n < 1024) return `${n} B`;
	if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
	return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

// ── Clipboard → files ────────────────────────────────────────────
// Common clipboard MIME types → file extensions. A pasted screenshot has no
// filename, so we synthesize `clipboard-N.<ext>`; anything unmapped falls back
// to the MIME subtype (sanitised) or `.bin`.
const MIME_EXT: Record<string, string> = {
	'image/png': 'png',
	'image/jpeg': 'jpg',
	'image/gif': 'gif',
	'image/webp': 'webp',
	'image/bmp': 'bmp',
	'image/svg+xml': 'svg',
	'image/tiff': 'tiff',
	'application/pdf': 'pdf'
};
export function extForType(type: string): string {
	if (MIME_EXT[type]) return MIME_EXT[type];
	const sub = type.split('/')[1]?.split(';')[0];
	return sub ? sub.replace(/[^a-z0-9]/gi, '') || 'bin' : 'bin';
}

/** Stateful clipboard-file extractor: one per composer/form so the synthesized
 *  `clipboard-N.<ext>` names stay unique within that surface. */
export function makeClipboardFiles() {
	let counter = 1;
	// Give a clipboard blob a stable, unique filename if it has none (pasted
	// screenshots/images arrive nameless), so dedupe-by-name doesn't collapse them.
	const named = (f: File): File => {
		if (f.name?.trim()) return f;
		const ext = extForType(f.type || 'application/octet-stream');
		return new File([f], `clipboard-${counter++}.${ext}`, {
			type: f.type || 'application/octet-stream'
		});
	};
	// Extract binary files from a paste (copied files OR a pasted image/screenshot).
	// Prefer `items` (some browsers expose pasted screenshots only there, not in
	// `.files`), then fall back to `.files`.
	return (cd: DataTransfer): File[] => {
		const out: File[] = [];
		for (const item of Array.from(cd.items ?? [])) {
			if (item.kind !== 'file') continue;
			const f = item.getAsFile();
			if (f) out.push(named(f));
		}
		if (out.length === 0) {
			for (const f of Array.from(cd.files ?? [])) out.push(named(f));
		}
		return out;
	};
}
