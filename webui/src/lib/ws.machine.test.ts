import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { WsClient, backoffDelay } from './ws.svelte';
import { auth } from './auth.svelte';

class FakeSocket {
	static CONNECTING = 0;
	static OPEN = 1;
	static CLOSING = 2;
	static CLOSED = 3;
	readyState = 0;
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

const last = () => sockets.at(-1)!;
const frames = (s: FakeSocket) => s.sent.map((f) => JSON.parse(f) as Record<string, unknown>);
const messages = (s: FakeSocket) => frames(s).filter((f) => f.type === 'message');
/** Every message frame the client emitted, across sockets: an ack timeout
 *  retries on a NEW socket, so a send's attempts are spread over several. */
const allMessages = () => sockets.flatMap(messages);
/** Bring up the socket the client dialled, the way a working network would.
 *  No-op if the newest socket is already open. */
const acceptDialled = () => {
	if (last().readyState === 0) last().accept();
};

beforeEach(() => {
	sockets = [];
	realWs = (globalThis as Record<string, unknown>).WebSocket;
	(globalThis as Record<string, unknown>).WebSocket = FakeSocket;
	auth.isAuthed = true;
	vi.useFakeTimers();
	vi.spyOn(Math, 'random').mockReturnValue(0.5);
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

const ACK_TIMEOUT_MS = 8000;
const DELIVERY_ACK_TIMEOUT_MS = 20_000;
const MAX_ATTEMPTS = 5;

describe('backoffDelay (send auto-retry)', () => {
	it('grows exponentially then caps at 30s (jitter pinned to 1x)', () => {
		expect(backoffDelay(1)).toBe(1000);
		expect(backoffDelay(2)).toBe(2000);
		expect(backoffDelay(3)).toBe(4000);
		expect(backoffDelay(4)).toBe(8000);
		expect(backoffDelay(5)).toBe(16000);
		expect(backoffDelay(6)).toBe(30000);
		expect(backoffDelay(20)).toBe(30000);
	});

	it('applies full jitter within [0.75x, 1.25x] of the base', () => {
		vi.spyOn(Math, 'random').mockReturnValue(0);
		expect(backoffDelay(2)).toBe(1500);
		vi.spyOn(Math, 'random').mockReturnValue(0.999999);
		expect(backoffDelay(2)).toBe(2500);
	});
});

describe('TrackedSend ack / retry / reconnect state machine', () => {
	it('sends one frame and, on ack ok, clears the pending send + its timer', () => {
		const c = openClient();
		c.trackedSend('s1', 'hello', 100);

		const s = last();
		expect(messages(s).length).toBe(1);
		expect(c.deliverySnapshot('s1').pending.has(100)).toBe(true);

		const cid = messages(s)[0].client_msg_id as string;
		s.deliver({ type: 'message_ack', client_msg_id: cid, ok: true });

		const snap = c.deliverySnapshot('s1');
		expect(snap.pending.size).toBe(0);
		expect(snap.failed.size).toBe(0);

		// The watchdog swaps the socket after 60 s of silence; a settled send
		// must not be re-dispatched onto the replacement.
		vi.advanceTimersByTime(60_000);
		expect(messages(s).length).toBe(1);
		expect(allMessages().length).toBe(1);
		expect(c.deliverySnapshot('s1').pending.size).toBe(0);
	});

	it('retries on a fresh socket after the ack timeout (new correlation id)', () => {
		const c = openClient();
		c.trackedSend('s1', 'hello', 100);
		const first = last();
		const cid1 = messages(first)[0].client_msg_id as string;

		vi.advanceTimersByTime(ACK_TIMEOUT_MS);
		// An unacked frame means the socket is half-open: retrying on it would
		// time out identically, so it is abandoned for a fresh one.
		expect(first.readyState).toBe(3);
		expect(last()).not.toBe(first);
		expect(c.deliverySnapshot('s1').retrying.has(100)).toBe(true);

		const second = last();
		second.accept();
		const msgs = messages(second);
		expect(msgs.length).toBe(1);
		expect(msgs[0].content).toBe('hello');
		expect(msgs[0].client_msg_id).not.toBe(cid1);
		expect(messages(first).length).toBe(1);
		expect(c.deliverySnapshot('s1').pending.has(100)).toBe(true);
	});

	it('exhausts MAX_ATTEMPTS into a hard failure, then retryNow resets the loop', () => {
		const c = openClient();
		c.trackedSend('s1', 'hello', 100);

		for (let i = 0; i < MAX_ATTEMPTS + 2; i++) {
			vi.advanceTimersByTime(ACK_TIMEOUT_MS);
			acceptDialled();
			vi.advanceTimersByTime(30000);
			acceptDialled();
		}
		const failed = c.deliverySnapshot('s1');
		expect(failed.failed.has(100)).toBe(true);
		expect(failed.pending.has(100)).toBe(false);
		expect(allMessages().length).toBe(MAX_ATTEMPTS);

		c.retryNow('s1', 100);
		const retried = c.deliverySnapshot('s1');
		expect(retried.failed.size).toBe(0);
		expect(retried.pending.has(100)).toBe(true);
		expect(allMessages().length).toBe(MAX_ATTEMPTS + 1);
	});

	// The budget counts frames the server ignored, not writes the client could
	// not make — otherwise a reconnect slower than the backoff fails a message
	// that never reached anyone.
	it('does not spend retry budget while the replacement socket is still dialling', () => {
		const c = openClient();
		c.trackedSend('s1', 'hello', 100);

		// Ack timeout, then leave every reconnect hanging (dead network).
		vi.advanceTimersByTime(ACK_TIMEOUT_MS);
		vi.advanceTimersByTime(120_000);

		const snap = c.deliverySnapshot('s1');
		expect(snap.failed.size).toBe(0);
		expect(snap.pending.has(100)).toBe(true);
		expect(allMessages().length).toBe(1);

		// The moment a socket comes up, the parked send goes out on it.
		acceptDialled();
		expect(messages(last()).length).toBe(1);
		expect(allMessages().length).toBe(2);
	});

	it('parks a send when the socket is down and delivers it on reconnect', () => {
		const c = new WsClient();
		const ok = c.trackedSend('s1', 'survive me', 100);
		expect(ok).toBe(false);

		const snap = c.deliverySnapshot('s1');
		expect(snap.pending.has(100)).toBe(true);
		expect(snap.failed.size).toBe(0);

		const s = last();
		expect(messages(s).length).toBe(0);
		s.accept();

		const msgs = messages(s);
		expect(msgs.length).toBe(1);
		expect(msgs[0].content).toBe('survive me');
	});

	it('ignores a stale ack for a superseded attempt', () => {
		const c = openClient();
		c.trackedSend('s1', 'hello', 100);
		const first = last();
		const cid1 = messages(first)[0].client_msg_id as string;

		vi.advanceTimersByTime(ACK_TIMEOUT_MS);
		acceptDialled();
		const second = last();
		expect(allMessages().length).toBe(2);

		// A late ack for attempt 1, arriving on the socket that outlived it,
		// must not settle the send now riding attempt 2.
		second.deliver({ type: 'message_ack', client_msg_id: cid1, ok: true });
		expect(c.deliverySnapshot('s1').pending.has(100)).toBe(true);

		const cid2 = messages(second)[0].client_msg_id as string;
		expect(cid2).not.toBe(cid1);
		second.deliver({ type: 'message_ack', client_msg_id: cid2, ok: true });
		expect(c.deliverySnapshot('s1').pending.size).toBe(0);
	});

	it('drives the send into auto-retry on a server ok=false ack', () => {
		const c = openClient();
		c.trackedSend('s1', 'hello', 100);
		const s = last();
		const cid = messages(s)[0].client_msg_id as string;

		s.deliver({ type: 'message_ack', client_msg_id: cid, ok: false, error: 'no daemon' });
		expect(c.deliverySnapshot('s1').retrying.has(100)).toBe(true);

		vi.advanceTimersByTime(backoffDelay(1));
		expect(messages(s).length).toBe(2);
	});

	// An ok ack only says the server queued the frame toward a daemon. With a
	// `command_id` the send stays pending until the adapter reports it actually
	// delivered, so a reply routed to the wrong worker pod goes red.
	it('holds the send pending after an ok ack that carries a command_id', async () => {
		const c = openClient();
		c.trackedSend('s1', 'hello', 100);
		const s = last();
		const cid = messages(s)[0].client_msg_id as string;

		s.deliver({ type: 'message_ack', client_msg_id: cid, ok: true, command_id: 'cmd-1' });
		expect(c.deliverySnapshot('s1').pending.has(100)).toBe(true);

		s.deliver({ type: 'command_result', command_id: 'cmd-1', ok: true });
		await vi.advanceTimersByTimeAsync(0);
		expect(c.deliverySnapshot('s1').pending.size).toBe(0);
		expect(c.deliverySnapshot('s1').failed.size).toBe(0);
	});

	it('retries the send when the adapter reports it could not deliver', async () => {
		const c = openClient();
		c.trackedSend('s1', 'hello', 100);
		const s = last();
		const cid = messages(s)[0].client_msg_id as string;

		s.deliver({ type: 'message_ack', client_msg_id: cid, ok: true, command_id: 'cmd-1' });
		s.deliver({
			type: 'command_result',
			command_id: 'cmd-1',
			ok: false,
			error: 'no session id on disk to resume'
		});
		await vi.advanceTimersByTimeAsync(0);
		expect(c.deliverySnapshot('s1').retrying.has(100)).toBe(true);

		await vi.advanceTimersByTimeAsync(backoffDelay(1));
		expect(messages(s).length).toBe(2);
	});

	// Adapters that never report a delivery result must not strand the send:
	// unconfirmed clears rather than failing, so it never invites a duplicate.
	it('clears the send when no adapter delivery result ever arrives', async () => {
		const c = openClient();
		c.trackedSend('s1', 'hello', 100);
		const s = last();
		const cid = messages(s)[0].client_msg_id as string;

		s.deliver({ type: 'message_ack', client_msg_id: cid, ok: true, command_id: 'cmd-1' });
		await vi.advanceTimersByTimeAsync(DELIVERY_ACK_TIMEOUT_MS);

		const snap = c.deliverySnapshot('s1');
		expect(snap.pending.size).toBe(0);
		expect(snap.failed.size).toBe(0);
		expect(messages(s).length).toBe(1);
	});
});

describe('list patches vs refetch triggers', () => {
	it('user-message stream frame emits a list patch and no changeTick bump', () => {
		const c = openClient();
		const patches: unknown[] = [];
		c.onListPatch((p) => patches.push(p));
		const before = c.changeTick;
		last().deliver({
			type: 'stream',
			session_id: 's1',
			data: { type: 'text', content: '▷ User: hello   world', meta: false, ts: 1000 }
		});
		expect(patches).toEqual([
			{
				session_id: 's1',
				last_message_text: 'hello world',
				last_message_at: new Date(1000).toISOString()
			}
		]);
		expect(c.changeTick).toBe(before);
	});

	it('assistant text emits no patch and no bump', () => {
		const c = openClient();
		const patches: unknown[] = [];
		c.onListPatch((p) => patches.push(p));
		const before = c.changeTick;
		last().deliver({
			type: 'stream',
			session_id: 's1',
			data: { type: 'text', content: 'assistant prose', meta: false, ts: 1000 }
		});
		expect(patches).toEqual([]);
		expect(c.changeTick).toBe(before);
	});

	it('permission request patches attention; resolution refetches debounced', () => {
		const c = openClient();
		const patches: { attention?: string }[] = [];
		c.onListPatch((p) => patches.push(p));
		const before = c.changeTick;
		last().deliver({ type: 'permission_request', session_id: 's1', request_id: 'r1' });
		expect(patches).toEqual([{ session_id: 's1', attention: 'needs_input', bucket: 'blocked' }]);
		expect(c.changeTick).toBe(before);
		last().deliver({ type: 'permission_resolved', session_id: 's1', request_id: 'r1' });
		expect(c.changeTick).toBe(before);
		vi.advanceTimersByTime(2100);
		expect(c.changeTick).toBe(before + 1);
	});
});
