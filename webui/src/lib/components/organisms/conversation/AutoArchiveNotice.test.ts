import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { flushSync, mount, unmount } from 'svelte';
import AutoArchiveNotice from './AutoArchiveNotice.svelte';

const NOW = Date.parse('2026-09-25T10:00:00Z');
const inMin = (mins: number) => new Date(NOW + mins * 60_000).toISOString();

let comp: ReturnType<typeof mount> | null = null;

beforeEach(() => {
	vi.useFakeTimers();
	vi.setSystemTime(NOW);
});

afterEach(() => {
	if (comp) unmount(comp);
	comp = null;
	document.body.innerHTML = '';
	vi.useRealTimers();
});

type Props = {
	status: 'active' | 'inactive' | 'archived';
	liveness: 'active' | 'stale' | 'dead';
	bucket: 'working' | 'blocked' | 'review' | 'done';
	auto_archive_at?: string | null;
	archived_by?: 'user' | 'automatic' | null;
};

const idle = (over: Partial<Props> = {}): Props => ({
	status: 'inactive',
	liveness: 'dead',
	bucket: 'done',
	auto_archive_at: inMin(30),
	...over
});

function render(session: Props, onpin = () => {}): HTMLElement {
	if (comp) unmount(comp);
	document.body.innerHTML = '';
	const host = document.createElement('div');
	document.body.appendChild(host);
	comp = mount(AutoArchiveNotice, { target: host, props: { session, onpin } as never });
	flushSync();
	return host;
}

const notice = (host: HTMLElement) => host.querySelector('[data-testid="auto-archive-notice"]');

describe('AutoArchiveNotice', () => {
	it('warns an idle session archived within the window and pins on click', () => {
		const onpin = vi.fn();
		const host = render(idle(), onpin);
		expect(notice(host)?.textContent).toContain('cctui will archive this session in 30 minutes');
		host.querySelector('button')?.click();
		flushSync();
		expect(onpin).toHaveBeenCalledOnce();
	});

	it('shows for review sessions too', () => {
		expect(notice(render(idle({ bucket: 'review' })))).not.toBeNull();
	});

	it('stays silent while the session is working, even with a stale heartbeat', () => {
		expect(notice(render(idle({ bucket: 'working', liveness: 'stale' })))).toBeNull();
	});

	it('stays silent while the session waits on the human', () => {
		expect(notice(render(idle({ bucket: 'blocked' })))).toBeNull();
	});

	it('stays silent while the heartbeat is fresh', () => {
		expect(notice(render(idle({ liveness: 'active' })))).toBeNull();
	});

	it('stays silent when the archive is more than two hours away or not scheduled', () => {
		expect(notice(render(idle({ auto_archive_at: inMin(121) })))).toBeNull();
		expect(notice(render(idle({ auto_archive_at: null })))).toBeNull();
	});

	it('appears on its own once the window opens', () => {
		const host = render(idle({ auto_archive_at: inMin(121) }));
		expect(notice(host)).toBeNull();
		vi.advanceTimersByTime(2 * 60_000);
		flushSync();
		expect(notice(host)).not.toBeNull();
	});

	it('says an archived session was archived automatically, not by the user', () => {
		const auto = render(idle({ status: 'archived', archived_by: 'automatic' }));
		expect(notice(auto)).not.toBeNull();
		expect(auto.querySelector('button')).toBeNull();
		expect(notice(render(idle({ status: 'archived', archived_by: 'user' })))).toBeNull();
	});
});
