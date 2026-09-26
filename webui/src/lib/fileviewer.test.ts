import { afterEach, describe, expect, it, vi } from 'vitest';

vi.mock('$app/environment', () => ({ browser: true }));

import { classify, deniedRoots, installFileViewer, refusalMessage } from './fileviewer';

describe('fileviewer classify', () => {
	it('routes by base content type', () => {
		expect(classify('image/png')).toBe('image');
		expect(classify('text/markdown; charset=utf-8')).toBe('markdown');
		expect(classify('text/plain; charset=utf-8')).toBe('text');
		expect(classify('application/json; charset=utf-8')).toBe('text');
		expect(classify('application/pdf')).toBe('download');
		expect(classify('application/octet-stream')).toBe('download');
		expect(classify('text/html')).toBe('download');
		expect(classify(null)).toBe('download');
	});
});

describe('fileviewer refusalMessage', () => {
	it('names the file and distinguishes the refusal kinds', () => {
		const tooLarge = refusalMessage(413, 'big.zip');
		const denied = refusalMessage(403, 'x.md');
		const missing = refusalMessage(404, 'x.md');
		const offline = refusalMessage(503, 'x.md');
		const other = refusalMessage(500, 'x.md');
		for (const t of [tooLarge, denied, missing, offline, other]) expect(t).toMatch(/x\.md|big\.zip/);
		expect(new Set([tooLarge, denied, missing, offline, other]).size).toBe(5);
		expect(other).toContain('500');
	});

	it('gives blob 404, fs 404 and fs 503 three distinct messages', () => {
		const blobMissing = refusalMessage(404, 'shot.png', 'blob');
		const fsMissing = refusalMessage(404, 'shot.png', 'machine');
		const fsOffline = refusalMessage(503, 'shot.png', 'machine');
		expect(new Set([blobMissing, fsMissing, fsOffline]).size).toBe(3);
		for (const t of [blobMissing, fsMissing, fsOffline]) expect(t).toContain('shot.png');
	});

	it('never blames the machine for a blob-store refusal', () => {
		for (const status of [400, 404, 500, 503]) {
			expect(refusalMessage(status, 'shot.png', 'blob')).not.toMatch(/machine/i);
		}
	});

	it('names the roots a denial was checked against, when the daemon listed them', () => {
		const detail = '/home/gtax/.claude/jobs/cdfadc1d/tmp/x.md is outside the allowed roots: /tmp, /srv/app';
		expect(deniedRoots(detail)).toEqual(['/tmp', '/srv/app']);
		expect(deniedRoots('path was not linked in this session')).toEqual([]);

		const withRoots = refusalMessage(403, 'x.md', 'machine', detail);
		expect(withRoots).toContain('/tmp');
		expect(withRoots).toContain('/srv/app');
		expect(withRoots).not.toBe(refusalMessage(403, 'x.md', 'machine'));
	});

	it('calls a network failure a network failure on either source', () => {
		for (const source of ['machine', 'blob'] as const) {
			expect(refusalMessage(0, 'x.md', source)).toContain('network');
		}
	});
});

describe('fileviewer inline refusals', () => {
	afterEach(() => {
		document.body.innerHTML = '';
		vi.unstubAllGlobals();
	});

	function link(): HTMLAnchorElement {
		URL.createObjectURL = vi.fn(() => 'blob:stub');
		URL.revokeObjectURL = vi.fn();
		document.body.innerHTML =
			'<a class="md-file" href="/api/v1/machines/m/fs/file?path=/x/note.md&session_id=s" data-file-name="note.md">/x/note.md</a>';
		installFileViewer();
		return document.querySelector('a.md-file') as HTMLAnchorElement;
	}

	function click(a: HTMLAnchorElement): void {
		a.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 }));
	}

	it('shows the refusal next to the link instead of navigating to the body', async () => {
		const detail = '/x/note.md is outside the allowed roots: /tmp, /home/gtax/.claude/jobs';
		vi.stubGlobal(
			'fetch',
			vi.fn(async () => new Response(JSON.stringify({ error: detail }), { status: 403 }))
		);
		const a = link();
		click(a);

		const err = await vi.waitFor(() => {
			const el = document.querySelector('.md-file-error');
			expect(el).not.toBeNull();
			return el;
		});
		expect(err?.textContent).toContain('note.md');
		expect(err?.textContent).toContain('/home/gtax/.claude/jobs');
		expect(a.nextElementSibling).toBe(err);
	});

	it('clears a previous refusal when a retry succeeds', async () => {
		const fetchMock = vi
			.fn()
			.mockResolvedValueOnce(new Response(JSON.stringify({ error: 'nope' }), { status: 404 }))
			.mockResolvedValueOnce(
				new Response('hello', { status: 200, headers: { 'content-type': 'text/plain' } })
			);
		vi.stubGlobal('fetch', fetchMock);
		const a = link();

		click(a);
		await vi.waitFor(() => expect(document.querySelector('.md-file-error')).not.toBeNull());

		click(a);
		await vi.waitFor(() => expect(document.querySelector('.md-file-error')).toBeNull());
	});
});
