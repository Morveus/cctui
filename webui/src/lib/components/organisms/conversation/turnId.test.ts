import { describe, it, expect } from 'vitest';
import type { AgentEvent } from '@bindings/AgentEvent';
import { eventSig, mergeEventSources } from './format';
import { newTurnId } from '$lib/turnid';

const userEv = (seq: number, content: string, turnId?: string): AgentEvent =>
	({
		type: 'text',
		content: `▷ User: ${content}`,
		ts: seq,
		seq,
		meta: false,
		kind: null,
		...(turnId ? { turn_id: turnId } : {})
	}) as unknown as AgentEvent;

// The three encodings Claude stores one attachment-carrying user turn in.
const ENCODINGS = [
	'look at this\nAttached file:\n- /tmp/cctui-uploads/s1/shot.png',
	'[Image #1][shot.png]look at this',
	'[Image: source: /tmp/cctui-uploads/s1/shot.png]'
];

describe('eventSig', () => {
	it('keys on the turn id whenever the event carries one', () => {
		const id = newTurnId();
		expect(eventSig(userEv(1, 'anything', id))).toBe(`t:${id}`);
	});

	it('gives two encodings of one turn the same signature', () => {
		const id = newTurnId();
		const sigs = new Set(ENCODINGS.map((t, i) => eventSig(userEv(i, t, id))));
		expect(sigs.size).toBe(1);
	});

	it('gives two distinct turns with identical text distinct signatures', () => {
		expect(eventSig(userEv(1, 'continue', newTurnId()))).not.toBe(
			eventSig(userEv(2, 'continue', newTurnId()))
		);
	});

	it('falls back to the content key when there is no turn id', () => {
		expect(eventSig(userEv(1, 'continue'))).toBe('u:continue');
	});
});

describe('mergeEventSources with turn ids', () => {
	it('collapses the encodings of one turn to a single bubble', () => {
		const id = newTurnId();
		const rows = ENCODINGS.map((t, i) => userEv(i + 1, t, id));
		const out = mergeEventSources(rows, [], []);
		expect(out).toHaveLength(1);
	});

	it('collapses them across the three sources too', () => {
		const id = newTurnId();
		const [a, b, c] = ENCODINGS.map((t, i) => userEv(i + 1, t, id));
		expect(mergeEventSources([a], [b], [c])).toHaveLength(1);
	});

	it('keeps two identical-text turns that carry different ids', () => {
		const out = mergeEventSources(
			[userEv(1, 'continue', newTurnId()), userEv(2, 'continue', newTurnId())],
			[],
			[]
		);
		expect(out).toHaveLength(2);
	});

	it('still collapses turn-less rows by content', () => {
		const out = mergeEventSources([userEv(1, 'hello'), userEv(2, 'hello')], [], []);
		expect(out).toHaveLength(1);
	});

	it('does not collapse a turn-less row into a turn-carrying one', () => {
		const id = newTurnId();
		const out = mergeEventSources([userEv(1, 'hello', id), userEv(2, 'hello')], [], []);
		expect(out).toHaveLength(2);
	});
});

describe('newTurnId', () => {
	it('mints a well-formed v7 uuid', () => {
		const id = newTurnId();
		expect(id).toMatch(
			/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
		);
	});

	it('leads with the mint time, so ids from different milliseconds sort', async () => {
		const first = newTurnId();
		await new Promise((r) => setTimeout(r, 3));
		expect(newTurnId() > first).toBe(true);
	});

	it('does not repeat', () => {
		const ids = new Set(Array.from({ length: 200 }, newTurnId));
		expect(ids.size).toBe(200);
	});
});
