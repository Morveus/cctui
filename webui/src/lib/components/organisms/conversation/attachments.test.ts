import { describe, expect, it } from 'vitest';
import { isPasteName, parseUserUploadRefs } from './lines';
import { pickAttachment, type SessionAttachment } from '$lib/queries/types';

const sid = '0a1b2c3d-1111-2222-3333-444455556666';

describe('parseUserUploadRefs', () => {
	it('reads staged paths and the session id they live under', () => {
		const text = `look at these\n\nAttached files (2):\n- /tmp/cctui-uploads/${sid}/shot.png\n- /tmp/cctui-uploads/${sid}/report final.pdf`;
		expect(parseUserUploadRefs(text)).toEqual({
			sessionId: sid,
			names: ['shot.png', 'report final.pdf']
		});
	});

	it('keeps a single staged file and its [token] as one entry', () => {
		const text = `[paste-1.txt]\n\nAttached file:\n- /tmp/cctui-uploads/${sid}/paste-1.txt`;
		expect(parseUserUploadRefs(text)).toEqual({ sessionId: sid, names: ['paste-1.txt'] });
	});

	it('reads paste tokens even without a staged path block', () => {
		expect(parseUserUploadRefs('see [paste-2.txt] and [paste-10.txt]')).toEqual({
			sessionId: null,
			names: ['paste-2.txt', 'paste-10.txt']
		});
	});

	it('ignores prose brackets and markdown links', () => {
		expect(parseUserUploadRefs('[not a file] and [docs](https://x.y/a.md)').names).toEqual([]);
		expect(parseUserUploadRefs(undefined).names).toEqual([]);
	});
});

describe('isPasteName', () => {
	it('matches the composer naming exactly', () => {
		expect(isPasteName('paste-1.txt')).toBe(true);
		expect(isPasteName('paste-12.txt')).toBe(true);
		expect(isPasteName('paste-1.md')).toBe(false);
		expect(isPasteName('mypaste-1.txt')).toBe(false);
	});
});

const att = (name: string, created_at: number, hash = name): SessionAttachment => ({
	id: `${name}-${created_at}`,
	session_id: sid,
	message_id: null,
	name,
	hash,
	size: 10,
	content_type: 'text/plain',
	created_at,
	machine_id: 'mach-1'
});

describe('pickAttachment', () => {
	const all = [
		att('paste-1.txt', 1_000, 'a'),
		att('paste-1.txt', 500_000, 'b'),
		att('x.png', 500_100)
	];

	it('takes the newest upload that precedes the message', () => {
		expect(pickAttachment(all, 'paste-1.txt', 600_000)?.hash).toBe('b');
		expect(pickAttachment(all, 'paste-1.txt', 2_000)?.hash).toBe('a');
	});

	it('tolerates clock skew between upload and message timestamps', () => {
		expect(pickAttachment(all, 'x.png', 500_000)?.name).toBe('x.png');
	});

	it('falls back to the earliest upload of that name, and null when unknown', () => {
		expect(pickAttachment(all, 'paste-1.txt', 10)?.hash).toBe('a');
		expect(pickAttachment(all, 'nope.txt', 6000)).toBeNull();
	});
});
