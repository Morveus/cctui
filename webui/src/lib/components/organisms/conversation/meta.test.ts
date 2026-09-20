import { describe, it, expect } from 'vitest';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import type { AgentEvent } from '@bindings/AgentEvent';
import { META_TAGS, isSyntheticImageNotice, looksMeta, mergeEventSources } from './format';

describe('looksMeta', () => {
	it('matches a marker that is not at the start of the turn', () => {
		// The case a prefix-only test misses: the harness prefixes its own
		// sentence before the wrapper it injects.
		expect(
			looksMeta('Another session sent a message:\n<task-notification>done</task-notification>')
		).toBe(true);
		expect(looksMeta('preamble\n  <system-reminder>hi</system-reminder>')).toBe(true);
	});

	it('still matches a marker at the start of the turn', () => {
		expect(looksMeta('<system-reminder>hi</system-reminder>')).toBe(true);
		expect(looksMeta('# Autonomous loop\ntick')).toBe(true);
	});

	it('leaves human prose that quotes a marker inline alone', () => {
		expect(looksMeta('the <system-reminder> tag keeps firing, can we mute it?')).toBe(false);
		expect(looksMeta('ship it')).toBe(false);
	});

	it('treats image bookkeeping as meta whatever the wording', () => {
		expect(looksMeta('[Image: source: /tmp/a.png]')).toBe(true);
		expect(
			looksMeta(
				'[Image: original 1440x3120, displayed at 923x2000. Multiply coordinates by 1.56 to map to original image.]'
			)
		).toBe(true);
		expect(looksMeta('[Image #2]')).toBe(true);
	});
});

describe('isSyntheticImageNotice', () => {
	it('accepts every shape of the [Image …] family, alone or repeated', () => {
		expect(isSyntheticImageNotice('[Image]')).toBe(true);
		expect(isSyntheticImageNotice('[Image #1]\n[Image #2]')).toBe(true);
		expect(isSyntheticImageNotice('[Image: original 100x200, displayed at 50x100.]')).toBe(true);
	});

	it('rejects a turn that also carries human prose', () => {
		expect(isSyntheticImageNotice('[Image #1]\nwhat is in this screenshot?')).toBe(false);
		expect(isSyntheticImageNotice('look at [Image #1] please')).toBe(false);
		expect(isSyntheticImageNotice('')).toBe(false);
	});
});

const ev = (seq: number, content: string): AgentEvent =>
	({ type: 'text', content, ts: seq, seq, meta: false, kind: null }) as unknown as AgentEvent;

describe('mergeEventSources', () => {
	it('collapses duplicates inside a single history array', () => {
		const dupe = [ev(1, 'hello'), ev(2, 'hello'), ev(3, 'bye')];
		const out = mergeEventSources(dupe, [], []);
		expect(out.map((e) => (e as { content: string }).content)).toEqual(['hello', 'bye']);
	});

	it('still collapses duplicates across the three sources', () => {
		const out = mergeEventSources([ev(2, 'hello')], [ev(1, 'hello')], [ev(3, 'hello')]);
		expect(out).toHaveLength(1);
	});

	it('keeps distinct events from every source, ordered by seq', () => {
		const out = mergeEventSources([ev(2, 'b')], [ev(1, 'a')], [ev(3, 'c')]);
		expect(out.map((e) => (e as { content: string }).content)).toEqual(['a', 'b', 'c']);
	});
});

// The daemon and the webui each need their own copy of the marker list, so the
// only thing that can keep them honest is asserting they are equal.
describe('marker list parity with the daemon', () => {
	// `import.meta.url` is an http: URL under Vite, so the Rust file is located
	// by walking up from the working directory instead.
	const REL = 'crates/cctui-daemon/src/adapters/claude_code/transcript.rs';

	function findRepoFile(rel: string): string {
		let dir = process.cwd();
		for (;;) {
			const candidate = resolve(dir, rel);
			if (existsSync(candidate)) return candidate;
			const parent = dirname(dir);
			if (parent === dir) throw new Error(`cannot locate ${rel} upward from ${process.cwd()}`);
			dir = parent;
		}
	}

	it('matches META_MARKERS in transcript.rs', () => {
		const src = readFileSync(findRepoFile(REL), 'utf8');
		const block = /const META_MARKERS: \[&str; \d+\] = \[([\s\S]*?)\];/.exec(src);
		if (!block) throw new Error('META_MARKERS declaration not found in transcript.rs');
		const markers = [...block[1].matchAll(/"((?:[^"\\]|\\.)*)"/g)].map((m) =>
			m[1].replace(/\\"/g, '"')
		);
		// Guard against a vacuous pass: two empty lists must not compare equal.
		expect(markers.length).toBeGreaterThan(0);
		expect(META_TAGS.length).toBeGreaterThan(0);
		expect(markers).toEqual(META_TAGS);
	});
});
