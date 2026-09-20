import { browser } from '$app/environment';
import { m } from '$lib/paraglide/messages';
import { renderMarkdown } from '$lib/markdown';
import { toasts } from '$lib/toast.svelte';

// Delegated opener for agent-linked local files (`a.md-file`, injected via
// {@html} by the markdown renderer, so — like the image lightbox — one
// document-level listener covers every bubble). The link points at the
// machine-scoped read-file route; the response's content type decides what
// happens: images and text/markdown open in an overlay, anything else is
// downloaded. Refusals (too large, outside the allow-list, daemon offline)
// surface as a toast instead of a bare error page.
let installed = false;

export function installFileViewer(): void {
	if (!browser || installed) return;
	installed = true;
	document.addEventListener('click', (e) => {
		if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey) return;
		const target = e.target as HTMLElement | null;
		const link = target?.closest('a.md-file') as HTMLAnchorElement | null;
		if (!link) return;
		const href = link.getAttribute('href');
		if (!href) return;
		e.preventDefault();
		e.stopPropagation();
		void openLocalFile(href, link.dataset.fileName ?? link.textContent ?? 'file');
	});
}

export type FileKind = 'image' | 'text' | 'markdown' | 'download';

/** What the viewer does with a response of this content type. */
export function classify(contentType: string | null): FileKind {
	const base = (contentType ?? '').split(';')[0].trim().toLowerCase();
	if (base.startsWith('image/')) return 'image';
	if (base === 'text/markdown') return 'markdown';
	if (base === 'text/plain' || base === 'application/json') return 'text';
	return 'download';
}

/**
 * Which route the href points at. `blob` is the server's own store
 * (`/sessions/{id}/blobs/{hash}`), which knows nothing about any machine, so
 * its refusals must never be worded as a machine-side absence; `machine` is
 * `/machines/{id}/fs/file`, where the daemon and the filesystem are in play.
 */
export type FileSource = 'machine' | 'blob';

/** Toast text for a refused read, by HTTP status and route. */
export function refusalMessage(
	status: number,
	name: string,
	source: FileSource = 'machine'
): string {
	if (source === 'blob') {
		return status === 404
			? m.conversation_attachment_gone({ name })
			: m.conversation_file_open_failed({ name, status: String(status) });
	}
	switch (status) {
		case 413:
			return m.conversation_file_too_large({ name });
		case 403:
			return m.conversation_file_denied({ name });
		case 404:
			return m.conversation_file_not_found({ name });
		case 503:
		case 504:
			return m.conversation_file_daemon_offline({ name });
		default:
			return m.conversation_file_open_failed({ name, status: String(status) });
	}
}

/**
 * Open `href`, returning `null` on success and the HTTP status (`0` for a
 * network failure) on refusal — without toasting, so a caller with a fallback
 * chain can try the next source before saying anything.
 */
export async function tryOpenLocalFile(
	href: string,
	name: string
): Promise<number | null> {
	let res: Response;
	try {
		res = await fetch(href, { credentials: 'same-origin' });
	} catch {
		return 0;
	}
	if (!res.ok) return res.status;
	await present(res, name);
	return null;
}

export async function openLocalFile(
	href: string,
	name: string,
	source: FileSource = 'machine'
): Promise<void> {
	const status = await tryOpenLocalFile(href, name);
	if (status === 0) {
		toasts.error(m.conversation_file_open_failed({ name, status: 'network' }));
	} else if (status !== null) {
		toasts.error(refusalMessage(status, name, source));
	}
}

async function present(res: Response, name: string): Promise<void> {
	const kind = classify(res.headers.get('content-type'));
	if (kind === 'download') {
		download(await res.blob(), name);
		return;
	}
	if (kind === 'image') {
		const url = URL.createObjectURL(await res.blob());
		openOverlay(name, url, () => URL.revokeObjectURL(url), (body) => {
			const img = document.createElement('img');
			img.className = 'md-lightbox-img';
			img.src = url;
			img.alt = name;
			body.appendChild(img);
		});
		return;
	}
	const text = await res.text();
	const blobUrl = URL.createObjectURL(new Blob([text], { type: 'text/plain' }));
	openOverlay(name, blobUrl, () => URL.revokeObjectURL(blobUrl), (body) => {
		if (kind === 'markdown') {
			const div = document.createElement('div');
			div.className = 'md-fileviewer-md';
			div.innerHTML = renderMarkdown(text);
			body.appendChild(div);
		} else {
			const pre = document.createElement('pre');
			pre.className = 'md-fileviewer-pre';
			pre.textContent = text;
			body.appendChild(pre);
		}
	});
}

function download(blob: Blob, name: string): void {
	const url = URL.createObjectURL(blob);
	const a = document.createElement('a');
	a.href = url;
	a.download = name;
	document.body.appendChild(a);
	a.click();
	a.remove();
	setTimeout(() => URL.revokeObjectURL(url), 10_000);
}

function openOverlay(
	name: string,
	downloadUrl: string,
	cleanup: () => void,
	fill: (body: HTMLElement) => void
): void {
	const previous = document.activeElement as HTMLElement | null;
	const overlay = document.createElement('div');
	overlay.className = 'md-lightbox md-fileviewer';
	overlay.setAttribute('role', 'dialog');
	overlay.setAttribute('aria-modal', 'true');
	overlay.setAttribute('aria-label', name);

	const panel = document.createElement('div');
	panel.className = 'md-fileviewer-panel';
	panel.addEventListener('click', (e) => e.stopPropagation());

	const head = document.createElement('div');
	head.className = 'md-fileviewer-head';
	const title = document.createElement('span');
	title.className = 'md-fileviewer-title';
	title.textContent = name;
	const dl = document.createElement('a');
	dl.className = 'md-fileviewer-btn';
	dl.href = downloadUrl;
	dl.download = name;
	dl.textContent = m.conversation_file_download();
	const closeBtn = document.createElement('button');
	closeBtn.type = 'button';
	closeBtn.className = 'md-fileviewer-btn';
	closeBtn.textContent = m.common_close();
	closeBtn.setAttribute('aria-label', m.common_close());
	head.append(title, dl, closeBtn);

	const body = document.createElement('div');
	body.className = 'md-fileviewer-body';
	fill(body);
	panel.append(head, body);
	overlay.appendChild(panel);

	const close = (): void => {
		overlay.remove();
		document.removeEventListener('keydown', onKey);
		cleanup();
		previous?.focus?.();
	};
	const onKey = (ev: KeyboardEvent): void => {
		if (ev.key === 'Escape') close();
	};
	closeBtn.addEventListener('click', close);
	overlay.addEventListener('click', close);
	document.addEventListener('keydown', onKey);
	document.body.appendChild(overlay);
	closeBtn.focus();
}
