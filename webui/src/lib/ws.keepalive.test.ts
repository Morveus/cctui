import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { RESUME_STALE_MS, WATCHDOG_MS, WsClient } from './ws.svelte';
import { auth } from './auth.svelte';

class FakeSocket {
	static CONNECTING = 0;
	static OPEN = 1;
	static CLOSING = 2;
	static CLOSED = 3;
	readyState = 0;
	closed = false;
	sent: string[] = [];
	onopen: (() => void) | null = null;
	onmessage: ((ev: { data: string }) => void) | null = null;
	onclose: (() => void) | null = null;
	onerror: (() => void) | null = null;
	constructor(public url: string) {
		sockets.push(this);
	}
	send(data: string) {
		this.sent.push(data);
	}
	close() {
		if (this.closed) return;
		this.closed = true;
		this.readyState = 3;
		this.onclose?.();
	}
	accept() {
		this.readyState = 1;
		this.onopen?.();
	}
	deliver(obj: unknown) {
		this.onmessage?.({ data: JSON.stringify(obj) });
	}
}

let sockets: FakeSocket[] = [];
let realWs: unknown;
const last = (): FakeSocket => {
	const s = sockets.at(-1);
	if (!s) throw new Error('no socket');
	return s;
};

beforeEach(() => {
	sockets = [];
	realWs = (globalThis as Record<string, unknown>).WebSocket;
	(globalThis as Record<string, unknown>).WebSocket = FakeSocket;
	auth.isAuthed = true;
	vi.useFakeTimers();
});

afterEach(() => {
	vi.useRealTimers();
	vi.restoreAllMocks();
	(globalThis as Record<string, unknown>).WebSocket = realWs;
});

function openClient(): WsClient {
	const c = new WsClient();
	c.connect();
	last().accept();
	return c;
}

describe('watchdog (CCT-1048)', () => {
	it('reconnects when no frame arrives within the window', () => {
		const c = openClient();
		const dead = last();
		expect(sockets).toHaveLength(1);

		vi.advanceTimersByTime(WATCHDOG_MS + 1);

		expect(dead.closed).toBe(true);
		expect(sockets).toHaveLength(2);
		expect(last()).not.toBe(dead);
		c.disconnect();
	});

	it('an arriving frame rearms the window instead of reconnecting', () => {
		const c = openClient();
		const sock = last();

		vi.advanceTimersByTime(WATCHDOG_MS - 1000);
		sock.deliver({ type: 'session_deregistered', session_id: 's1' });
		vi.advanceTimersByTime(WATCHDOG_MS - 1000);

		expect(sock.closed).toBe(false);
		expect(sockets).toHaveLength(1);

		vi.advanceTimersByTime(2000);
		expect(sockets).toHaveLength(2);
		c.disconnect();
	});

	it('the superseded socket’s late onclose does not clobber the new one', () => {
		const c = openClient();
		const dead = last();
		vi.advanceTimersByTime(WATCHDOG_MS + 1);
		const fresh = last();
		fresh.accept();

		dead.onclose?.();

		expect(c.status).toBe('open');
		vi.advanceTimersByTime(10_000);
		expect(sockets).toHaveLength(2);
		c.disconnect();
	});

	it('the server heartbeat rearms the window with no other traffic', () => {
		const c = openClient();
		const sock = last();

		for (let i = 0; i < 15; i++) {
			vi.advanceTimersByTime(20_000);
			sock.deliver({ type: 'heartbeat' });
		}

		expect(sock.closed).toBe(false);
		expect(sockets).toHaveLength(1);

		vi.advanceTimersByTime(WATCHDOG_MS + 1);
		expect(sockets).toHaveLength(2);
		c.disconnect();
	});

	it('stops watching once disconnected', () => {
		const c = openClient();
		c.disconnect();
		vi.advanceTimersByTime(WATCHDOG_MS * 3);
		expect(sockets).toHaveLength(1);
	});
});

describe('resumeCheck (CCT-1048)', () => {
	it('forces a fresh socket when the current one has gone quiet', () => {
		const c = openClient();
		const dead = last();

		vi.advanceTimersByTime(RESUME_STALE_MS + 1);
		c.resumeCheck();

		expect(dead.closed).toBe(true);
		expect(sockets).toHaveLength(2);
		c.disconnect();
	});

	it('keeps a socket that is merely idle', () => {
		const c = openClient();
		const sock = last();

		vi.advanceTimersByTime(RESUME_STALE_MS - 1000);
		c.resumeCheck();

		expect(sock.closed).toBe(false);
		expect(sockets).toHaveLength(1);
		c.disconnect();
	});

	it('dials again when there is no socket at all', () => {
		const c = openClient();
		last().close();
		expect(c.status).toBe('closed');

		c.resumeCheck();

		expect(sockets).toHaveLength(2);
		c.disconnect();
	});

	it('does nothing once the client has been disconnected', () => {
		const c = openClient();
		c.disconnect();
		c.resumeCheck();
		expect(sockets).toHaveLength(1);
	});
});

describe('ack timeout forces a reconnect (CCT-1048)', () => {
	it('dials a new socket rather than retrying the dead one', () => {
		const c = openClient();
		const dead = last();
		c.trackedSend('s1', 'hello', 1000);
		expect(dead.sent).toHaveLength(1);

		// ACK_TIMEOUT_MS is 8 s, below the watchdog window and the first backoff.
		vi.advanceTimersByTime(8500);

		expect(dead.closed).toBe(true);
		expect(sockets).toHaveLength(2);
		expect(c.deliverySnapshot('s1').pending.has(1000)).toBe(true);
		c.disconnect();
	});

	it('re-dispatches the parked send on the new socket', () => {
		const c = openClient();
		c.trackedSend('s1', 'hello', 1000);
		vi.advanceTimersByTime(8500);

		const fresh = last();
		fresh.accept();

		expect(fresh.sent.some((f) => f.includes('hello'))).toBe(true);
		c.disconnect();
	});
});
